//! Open invitations of this site (`/var/lib/wanderlan/invites.json`).
//!
//! An invitation is a random code that lets exactly one new site join. It
//! is valid for a while (a day unless asked otherwise, or until it is used
//! or deleted); it survives restarts. While a site joins with it (the change
//! waits for the new site to come up), it is reserved; if the join is rolled
//! back it becomes usable again. Expired ones are removed.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use wanderlan_core::api::{INVITE_VALID_SECS, SiteRules};
use wanderlan_core::join::{CODE_LEN, code_from_bytes, normalize_code};

/// One stored invitation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// The code as shown, e.g. `K7F3Q-9QX2M`.
    pub code: String,
    /// Unix time of creation.
    pub created_secs: u64,
    /// Rules for the site that joins with it.
    #[serde(default)]
    pub rules: SiteRules,
    /// How long it is valid in seconds; 0: until it is used or deleted;
    /// none (an invitation of an earlier version): [`INVITE_VALID_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_secs: Option<u64>,
}

impl Invite {
    /// Unix time it expires; none: never.
    pub fn expires_secs(&self) -> Option<u64> {
        match self.valid_secs.unwrap_or(INVITE_VALID_SECS) {
            0 => None,
            secs => Some(self.created_secs.saturating_add(secs)),
        }
    }

    fn expired(&self, now_secs: u64) -> bool {
        self.expires_secs().is_some_and(|at| at <= now_secs)
    }
}

/// The invitations of this site.
#[derive(Debug)]
pub struct Invites {
    path: PathBuf,
    list: Vec<Invite>,
    reserved: Option<String>,
    /// The reserved invitation, used up before the join is confirmed (the
    /// server of this site has the new site already): back if it is rolled
    /// back.
    taken: Option<Invite>,
}

impl Invites {
    /// Loads the stored invitations; a missing or unreadable file means none.
    pub fn load(path: PathBuf) -> Self {
        let list = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            path,
            list,
            reserved: None,
            taken: None,
        }
    }

    /// All open invitations, oldest first.
    pub fn list(&self) -> &[Invite] {
        &self.list
    }

    /// True if the code is the one reserved by a joining site.
    pub fn is_reserved(&self, code: &str) -> bool {
        self.reserved.as_deref() == Some(normalize_code(code).as_str())
    }

    /// Creates and stores a new invitation, valid for `valid_secs` (0: until
    /// it is used or deleted).
    pub fn create(&mut self, now_secs: u64, rules: SiteRules, valid_secs: u64) -> Result<Invite> {
        let bytes =
            crate::random::bytes::<CODE_LEN>().context("cannot create the invitation code")?;
        let invite = Invite {
            code: code_from_bytes(&bytes),
            created_secs: now_secs,
            rules,
            valid_secs: Some(valid_secs),
        };
        self.list.push(invite.clone());
        self.save()?;
        Ok(invite)
    }

    /// Finds an open, not reserved invitation by its code as typed that has
    /// not expired at `now_secs`.
    pub fn find_free(&self, code: &str, now_secs: u64) -> Option<&Invite> {
        let wanted = normalize_code(code);
        self.list
            .iter()
            .find(|i| normalize_code(&i.code) == wanted)
            .filter(|i| !self.is_reserved(&i.code) && !i.expired(now_secs))
    }

    /// Removes the invitations expired at `now_secs` (not one a site is
    /// joining with); returns their codes.
    pub fn expire(&mut self, now_secs: u64) -> Result<Vec<String>> {
        let gone: Vec<String> = self
            .list
            .iter()
            .filter(|i| i.expired(now_secs) && !self.is_reserved(&i.code))
            .map(|i| i.code.clone())
            .collect();
        if !gone.is_empty() {
            self.list.retain(|i| !gone.contains(&i.code));
            self.save()?;
        }
        Ok(gone)
    }

    /// Reserves an invitation for a site that is joining.
    pub fn reserve(&mut self, code: &str) {
        self.reserved = Some(normalize_code(code));
    }

    /// The reserved invitation is used up already, before the join is
    /// confirmed: the server of this site has the new site (a restart here
    /// must not let another one in with it). Back with [`Self::release`].
    pub fn take_reserved(&mut self) -> Result<()> {
        let Some(code) = &self.reserved else {
            return Ok(());
        };
        let Some(pos) = self
            .list
            .iter()
            .position(|i| normalize_code(&i.code) == *code)
        else {
            return Ok(());
        };
        self.taken = Some(self.list.remove(pos));
        self.save()
    }

    /// The join was rolled back: the reserved invitation can be used again.
    pub fn release(&mut self) -> Result<()> {
        self.reserved = None;
        if let Some(invite) = self.taken.take() {
            self.list.push(invite);
            self.save()?;
        }
        Ok(())
    }

    /// The join succeeded: the reserved invitation is used up.
    pub fn consume_reserved(&mut self) -> Result<Option<String>> {
        let Some(code) = self.reserved.take() else {
            return Ok(None);
        };
        if let Some(invite) = self.taken.take() {
            return Ok(Some(invite.code));
        }
        self.remove(&code)
    }

    /// Deletes an invitation; returns its code if it existed.
    pub fn remove(&mut self, code: &str) -> Result<Option<String>> {
        let wanted = normalize_code(code);
        let Some(pos) = self
            .list
            .iter()
            .position(|i| normalize_code(&i.code) == wanted)
        else {
            return Ok(None);
        };
        let removed = self.list.remove(pos);
        if self.reserved.as_deref() == Some(wanted.as_str()) {
            self.reserved = None;
        }
        self.save()?;
        Ok(Some(removed.code))
    }

    /// Forgets all invitations (reset; the file is removed with the state
    /// directory).
    pub fn clear(&mut self) {
        self.list.clear();
        self.reserved = None;
        self.taken = None;
    }

    fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.list).context("cannot encode invitations")?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        crate::files::write_atomic(&self.path, &format!("{text}\n"))?;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot protect {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wl-inv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("invites.json")
    }

    #[test]
    fn invitations_survive_a_restart_and_are_used_once() {
        let path = temp("once");
        let mut inv = Invites::load(path.clone());
        let a = inv.create(1, SiteRules::default(), 0).unwrap();
        let rules = SiteRules {
            name: Some("branch".into()),
            native: Some(14),
            ..SiteRules::default()
        };
        let b = inv.create(2, rules.clone(), 0).unwrap();
        assert_ne!(a.code, b.code);
        assert_eq!(a.code.len(), 11);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let mut inv = Invites::load(path.clone());
        assert_eq!(inv.list().len(), 2);
        assert_eq!(inv.list()[1].rules, rules, "rules are stored");
        let typed = a.code.to_lowercase().replace('-', "");
        assert!(inv.find_free(&typed, 3).is_some());
        inv.reserve(&typed);
        assert!(
            inv.find_free(&a.code, 3).is_none(),
            "reserved while joining"
        );
        inv.release().unwrap();
        assert!(
            inv.find_free(&a.code, 3).is_some(),
            "usable again after rollback"
        );
        // Used up before the confirmation (the server has the site), back
        // with a rollback.
        inv.reserve(&a.code);
        inv.take_reserved().unwrap();
        assert_eq!(Invites::load(path.clone()).list().len(), 1);
        inv.release().unwrap();
        assert_eq!(Invites::load(path.clone()).list().len(), 2);
        inv.reserve(&a.code);
        inv.take_reserved().unwrap();
        assert_eq!(inv.consume_reserved().unwrap(), Some(a.code.clone()));
        assert!(inv.find_free(&a.code, 3).is_none());
        assert_eq!(Invites::load(path.clone()).list().len(), 1);

        assert_eq!(inv.remove(&b.code).unwrap(), Some(b.code));
        assert!(Invites::load(path.clone()).list().is_empty());
        assert_eq!(inv.remove("nope").unwrap(), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn invitations_expire() {
        let path = temp("expire");
        let mut inv = Invites::load(path.clone());
        let day = inv
            .create(100, SiteRules::default(), INVITE_VALID_SECS)
            .unwrap();
        let hour = inv.create(100, SiteRules::default(), 3600).unwrap();
        let never = inv.create(100, SiteRules::default(), 0).unwrap();
        assert_eq!(hour.expires_secs(), Some(3700));
        assert!(inv.find_free(&hour.code, 3699).is_some());
        assert!(inv.find_free(&hour.code, 3700).is_none(), "expired");
        // One a site is joining with stays until the join is over.
        inv.reserve(&hour.code);
        assert!(inv.expire(4000).unwrap().is_empty());
        inv.release().unwrap();
        assert_eq!(inv.expire(4000).unwrap(), vec![hour.code.clone()]);
        assert_eq!(inv.expire(100 + INVITE_VALID_SECS).unwrap(), vec![day.code]);
        assert!(inv.find_free(&never.code, u64::MAX / 2).is_some());
        // One of an earlier version (no validity stored): a day.
        let text = format!(r#"[{{"code": "{}", "created_secs": 10}}]"#, never.code);
        std::fs::write(&path, text).unwrap();
        let mut old = Invites::load(path.clone());
        assert_eq!(old.list()[0].expires_secs(), Some(10 + INVITE_VALID_SECS));
        assert_eq!(old.expire(10 + INVITE_VALID_SECS).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
