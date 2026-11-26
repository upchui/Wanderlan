//! Rules of the automatic configuration sync.
//!
//! Every daemon announces the revision and fingerprint of its confirmed
//! `network.toml` in its control probes. A site fetches the network from a direct
//! peer that announces a newer one: a higher revision (also with the same
//! content, e.g. a site taken offline and online again, so that the revision
//! numbers agree everywhere), or the same revision with a different content
//! whose fingerprint is larger (a deterministic tie-break for concurrent
//! edits). Rejected fingerprints are never fetched again, so a configuration
//! that cannot be applied here does not loop.

use std::collections::BTreeSet;

use crate::config::{Config, Local, Network, fingerprint, validate};
use crate::identity::SiteId;

/// Revision and fingerprint of a network.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// Revision counter.
    pub revision: u64,
    /// Hex SHA-256 fingerprint of the content.
    pub fingerprint: String,
}

impl Version {
    /// The version of a network.
    pub fn of(network: &Network) -> Self {
        Self {
            revision: network.revision,
            fingerprint: fingerprint(network),
        }
    }

    /// True if `self` should replace `ours`.
    pub fn is_newer_than(&self, ours: &Version) -> bool {
        self.revision > ours.revision
            || (self.revision == ours.revision && self.fingerprint > ours.fingerprint)
    }
}

/// Picks the peer to fetch from: the newest announced version that is newer
/// than ours and was not rejected before.
pub fn choose<'a>(
    ours: &Version,
    announced: impl IntoIterator<Item = (&'a str, Version)>,
    rejected: &BTreeSet<String>,
) -> Option<(&'a str, Version)> {
    announced
        .into_iter()
        .filter(|(_, v)| v.is_newer_than(ours) && !rejected.contains(&v.fingerprint))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
}

/// Why a received network cannot be used here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    /// The text is not a valid network.
    #[error("the received configuration is invalid: {0}")]
    Invalid(String),
    /// This site is not part of it.
    #[error("the received configuration does not contain this site (\"{0}\")")]
    SelfMissing(String),
    /// Another site renamed this one, which only its manager may.
    #[error(
        "the received configuration renames this site (\"{0}\") to \"{1}\", but only it and its manager may rename it, and it has none"
    )]
    RenamedWithoutConsent(String, String),
    /// The entry of this site belongs to another box now: it joined again
    /// with this name (after a reset there, or a cloned machine).
    #[error(
        "the received configuration gives this site (\"{0}\") another identity ({1}): another box joined as \"{0}\"; run `wanderlan reset` here and join again, or remove the other box"
    )]
    IdentityTakenOver(String, String),
}

/// Writes this site's identity into its own entry if it has none yet (a
/// network of an older version, or one the uplink has not received the
/// claim for yet), with a new revision so that it spreads. Returns true if
/// it changed the network.
pub fn claim_own_id(network: &mut Network, me: &str, id: SiteId) -> bool {
    match network.sites.iter_mut().find(|s| s.name == me) {
        Some(site) if site.id.is_none() => {
            site.id = Some(id);
            network.revision = crate::config::next_revision(network.revision);
            true
        }
        _ => false,
    }
}

/// Checks a received network against the local configuration and returns the
/// complete configuration to apply.
///
/// Renames since `previous` (the network in effect here; a site with the
/// same address under a new name) are followed in `local.toml`: the own
/// name and the trunks of tunnels. This site itself may be renamed by
/// another only if it allows it, or if `taken_back`: its server refused a
/// change of this site, its new name included, and its network applies
/// again.
///
/// With `own_id`, the entry of this site must carry that identity: one it
/// lacks is claimed again ([`claim_own_id`]), another one means another box
/// took this site over.
pub fn accept(
    mut received: Network,
    local: &Local,
    previous: Option<&Network>,
    taken_back: bool,
    own_id: Option<SiteId>,
) -> Result<Config, Rejection> {
    let mut local = local.clone();
    if let Some(previous) = previous {
        let me = local.self_name.clone().unwrap_or_default();
        let renamed = crate::rename::renames(previous, &received);
        // Only its manager may rename it (see `policy::may_rename`).
        let manager_may = previous
            .site(&me)
            .and_then(|s| crate::policy::manager(previous, s))
            .is_some_and(|m| m != me && crate::policy::may_rename(previous, &me, m));
        if let Some((_, new)) = renamed.iter().find(|(old, _)| *old == me)
            && !taken_back
            && !manager_may
        {
            return Err(Rejection::RenamedWithoutConsent(me, new.clone()));
        }
        for (old, new) in &renamed {
            local.rename_site(old, new);
        }
    }
    let me = local.self_name.clone().unwrap_or_default();
    let Some(entry) = received.site(&me) else {
        return Err(Rejection::SelfMissing(me));
    };
    if let (Some(theirs), Some(mine)) = (entry.id, own_id)
        && theirs != mine
    {
        return Err(Rejection::IdentityTakenOver(me, theirs.fingerprint()));
    }
    if let Some(mine) = own_id {
        claim_own_id(&mut received, &me, mine);
    }
    let diags = validate(&received, Some(&local));
    if let Some(err) = diags.iter().find(|d| d.is_error()) {
        return Err(Rejection::Invalid(err.message.clone()));
    }
    Ok(Config {
        network: received,
        local,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_network;

    fn v(revision: u64, fp: &str) -> Version {
        Version {
            revision,
            fingerprint: fp.into(),
        }
    }

    #[test]
    fn newer_rules() {
        let ours = v(3, "bb");
        assert!(v(4, "aa").is_newer_than(&ours));
        assert!(!v(2, "cc").is_newer_than(&ours));
        assert!(
            v(3, "cc").is_newer_than(&ours),
            "tie-break: larger fingerprint wins"
        );
        assert!(!v(3, "aa").is_newer_than(&ours));
        assert!(
            v(9, "bb").is_newer_than(&ours),
            "same content, higher revision: adopt the revision"
        );
        assert!(!v(3, "bb").is_newer_than(&ours), "the same version");
    }

    #[test]
    fn choose_skips_rejected_and_takes_the_newest() {
        let ours = v(1, "aa");
        let rejected = BTreeSet::from(["dd".to_string()]);
        let announced = vec![
            ("hub1", v(2, "bb")),
            ("hub2", v(5, "dd")),
            ("s1", v(3, "cc")),
        ];
        assert_eq!(
            choose(&ours, announced, &rejected),
            Some(("s1", v(3, "cc")))
        );
        assert_eq!(choose(&ours, vec![("x", v(1, "aa"))], &rejected), None);
    }

    #[test]
    fn accept_requires_self() {
        let network = parse_network(include_str!("config/testdata/network-v1.toml"))
            .unwrap()
            .network
            .unwrap();
        let local = Local {
            self_name: Some("haus2".into()),
            trunk: vec!["ens19".into()],
            ..Default::default()
        };
        assert!(accept(network.clone(), &local, None, false, None).is_ok());
        let stranger = Local {
            self_name: Some("haus9".into()),
            trunk: vec!["ens19".into()],
            ..Default::default()
        };
        assert_eq!(
            accept(network, &stranger, None, false, None).unwrap_err(),
            Rejection::SelfMissing("haus9".into())
        );
    }

    #[test]
    fn renames_are_followed_and_this_site_needs_to_agree() {
        let previous = parse_network(include_str!("config/testdata/network-v1.toml"))
            .unwrap()
            .network
            .unwrap();
        let local = Local {
            self_name: Some("haus2".into()),
            trunk: vec!["ens19".into()],
            tunnel_trunks: [("heim".to_string(), vec!["ens20".to_string()])].into(),
            ..Local::default()
        };
        // A peer renamed: the trunk of the tunnel to it follows.
        let mut received = previous.clone();
        received.rename_site("heim", "home");
        let cfg = accept(received, &local, Some(&previous), false, None).unwrap();
        assert!(cfg.local.tunnel_trunks.contains_key("home"));
        assert!(!cfg.local.tunnel_trunks.contains_key("heim"));
        // This site renamed by another: only if it has a manager (haus2 has
        // none in the example).
        let mut received = previous.clone();
        received.rename_site("haus2", "h2");
        assert_eq!(
            accept(received.clone(), &local, Some(&previous), false, None).unwrap_err(),
            Rejection::RenamedWithoutConsent("haus2".into(), "h2".into())
        );
        let cfg = accept(received.clone(), &local, Some(&previous), true, None).unwrap();
        assert_eq!(cfg.local.self_name.as_deref(), Some("h2"), "taken back");
        let mut managed = previous.clone();
        managed
            .sites
            .iter_mut()
            .find(|s| s.name == "haus2")
            .unwrap()
            .managed_by = Some("heim".into());
        let cfg = accept(received, &local, Some(&managed), false, None).unwrap();
        assert_eq!(
            cfg.local.self_name.as_deref(),
            Some("h2"),
            "managed by heim"
        );
    }

    #[test]
    fn the_own_identity_is_claimed_again_and_another_is_refused() {
        let me = crate::identity::SiteId::from_bytes([1; 32]);
        let other = crate::identity::SiteId::from_bytes([2; 32]);
        let mut network = parse_network(include_str!("config/testdata/network-v1.toml"))
            .unwrap()
            .network
            .unwrap();
        let local = Local {
            self_name: Some("haus2".into()),
            trunk: vec!["ens19".into()],
            ..Local::default()
        };
        let revision = network.revision;
        let cfg = accept(network.clone(), &local, None, false, Some(me)).unwrap();
        assert_eq!(
            cfg.network.site("haus2").unwrap().id,
            Some(me),
            "claimed again"
        );
        assert_eq!(cfg.network.revision, revision + 1, "so that it spreads");
        // Already there: unchanged.
        let again = accept(cfg.network.clone(), &local, None, false, Some(me)).unwrap();
        assert_eq!(again.network, cfg.network);
        // Another box has this site now.
        if let Some(site) = network.sites.iter_mut().find(|s| s.name == "haus2") {
            site.id = Some(other);
        }
        let err = accept(network, &local, None, false, Some(me)).unwrap_err();
        assert!(matches!(err, Rejection::IdentityTakenOver(..)), "{err}");
        assert!(err.to_string().contains(&other.fingerprint()), "{err}");
    }
}
