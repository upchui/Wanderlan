//! Open invitations of this site (`/var/lib/wanderlan/invites.json`).
//!
//! An invitation is a random code that lets exactly one new site join. It
//! stays valid until it is used or deleted; it survives restarts. While a
//! site joins with it (the change waits for the new site to come up), it is
//! reserved; if the join is rolled back it becomes usable again.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use wanderlan_core::api::SiteRules;
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
}

/// The invitations of this site.
#[derive(Debug)]
pub struct Invites {
    path: PathBuf,
    list: Vec<Invite>,
    reserved: Option<String>,
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

    /// Creates and stores a new invitation.
    pub fn create(&mut self, now_secs: u64, rules: SiteRules) -> Result<Invite> {
        let mut bytes = [0u8; CODE_LEN];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .context("cannot read random bytes for the invitation code")?;
        let invite = Invite {
            code: code_from_bytes(&bytes),
            created_secs: now_secs,
            rules,
        };
        self.list.push(invite.clone());
        self.save()?;
        Ok(invite)
    }

    /// Finds an open, not reserved invitation by its code as typed.
    pub fn find_free(&self, code: &str) -> Option<&Invite> {
        let wanted = normalize_code(code);
        self.list
            .iter()
            .find(|i| normalize_code(&i.code) == wanted)
            .filter(|i| !self.is_reserved(&i.code))
    }

    /// Reserves an invitation for a site that is joining.
    pub fn reserve(&mut self, code: &str) {
        self.reserved = Some(normalize_code(code));
    }

    /// The join was rolled back: the reserved invitation can be used again.
    pub fn release(&mut self) {
        self.reserved = None;
    }

    /// The join succeeded: the reserved invitation is used up.
    pub fn consume_reserved(&mut self) -> Result<Option<String>> {
        let Some(code) = self.reserved.take() else {
            return Ok(None);
        };
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
        let a = inv.create(1, SiteRules::default()).unwrap();
        let rules = SiteRules {
            name: Some("branch".into()),
            native: Some(14),
            ..SiteRules::default()
        };
        let b = inv.create(2, rules.clone()).unwrap();
        assert_ne!(a.code, b.code);
        assert_eq!(a.code.len(), 11);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let mut inv = Invites::load(path.clone());
        assert_eq!(inv.list().len(), 2);
        assert_eq!(inv.list()[1].rules, rules, "rules are stored");
        let typed = a.code.to_lowercase().replace('-', "");
        assert!(inv.find_free(&typed).is_some());
        inv.reserve(&typed);
        assert!(inv.find_free(&a.code).is_none(), "reserved while joining");
        inv.release();
        assert!(
            inv.find_free(&a.code).is_some(),
            "usable again after rollback"
        );
        inv.reserve(&a.code);
        assert_eq!(inv.consume_reserved().unwrap(), Some(a.code.clone()));
        assert!(inv.find_free(&a.code).is_none());
        assert_eq!(Invites::load(path.clone()).list().len(), 1);

        assert_eq!(inv.remove(&b.code).unwrap(), Some(b.code));
        assert!(Invites::load(path.clone()).list().is_empty());
        assert_eq!(inv.remove("nope").unwrap(), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
