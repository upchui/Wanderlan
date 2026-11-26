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
pub fn choose<K: Ord + Copy>(
    ours: &Version,
    announced: impl IntoIterator<Item = (K, Version)>,
    rejected: &BTreeSet<String>,
) -> Option<(K, Version)> {
    announced
        .into_iter()
        .filter(|(_, v)| v.is_newer_than(ours) && !rejected.contains(&v.fingerprint))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
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

/// Gives this site's entry its own identity `id` if it has a provisional one
/// (a network of an older version, or one the uplink has not received the
/// claim for yet), with every reference to it and `self` in `local`, with a
/// new revision so that it spreads. Returns true if it changed the network.
pub fn claim_own_id(network: &mut Network, local: &mut Local, id: SiteId) -> bool {
    let Some(me) = local.self_id else {
        return false;
    };
    if me == id {
        return false;
    }
    match network.site(me) {
        Some(site) if !site.has_identity() && network.site(id).is_none() => {
            network.rekey(me, id);
            local.rekey(me, id);
            network.revision = crate::config::next_revision(network.revision);
            true
        }
        _ => false,
    }
}

/// Checks a received network against the local configuration and returns the
/// complete configuration to apply.
///
/// Sites that got another identity since `previous` (the network in effect
/// here: a claim, a box that joined again) are followed in `local.toml`: the
/// trunks of tunnels. This site itself may be renamed by another only if it
/// allows it, or if `taken_back`: its server refused a change of this site,
/// its new name included, and its network applies again.
///
/// With `own_id`, the entry of this site must carry that identity: one that
/// still has its provisional identity claims it again ([`claim_own_id`]);
/// an entry of this site under another identity means another box took this
/// site over.
pub fn accept(
    mut received: Network,
    local: &Local,
    previous: Option<&Network>,
    taken_back: bool,
    own_id: Option<SiteId>,
) -> Result<Config, Rejection> {
    let mut local = local.clone();
    let me = local.self_id.unwrap_or_else(|| SiteId::provisional(""));
    let before = previous.and_then(|p| p.site(me));
    let name = before.map_or_else(
        || {
            me.provisional_name()
                .map_or_else(|| me.fingerprint(), str::to_string)
        },
        |s| s.name.clone(),
    );
    // A network of sites that did not see the claim yet names this site by
    // its provisional identity: claimed again.
    if let Some(own) = own_id
        && received.site(own).is_none()
    {
        let provisional = SiteId::provisional(&name);
        if received
            .site(provisional)
            .is_some_and(|s| !s.has_identity())
        {
            received.rekey(provisional, own);
            received.revision = crate::config::next_revision(received.revision);
            local.rekey(provisional, own);
        }
    }
    if let Some(own) = own_id {
        claim_own_id(&mut received, &mut local, own);
    }
    let me = local.self_id.unwrap_or(me);
    let Some(entry) = received.site(me) else {
        // Another box joined under this site's name and address.
        let address = before.map(|s| s.address);
        if let Some(other) = received
            .sites
            .iter()
            .find(|s| s.name == name && Some(s.address) == address && s.has_identity())
        {
            return Err(Rejection::IdentityTakenOver(name, other.id.fingerprint()));
        }
        return Err(Rejection::SelfMissing(name));
    };
    if let Some(previous) = previous
        && let Some(old) = previous.site(me)
        && old.name != entry.name
    {
        // Only its manager may rename it (see `policy::may_rename`).
        let manager_may = crate::policy::manager(previous, old)
            .is_some_and(|m| m != me && crate::policy::may_rename(previous, me, m));
        // A client may not see its manager (it hangs off another site); its
        // server, which knows it, decided.
        let manager_unseen = old.uplink.is_some()
            && old
                .managed_by
                .is_some_and(|m| m != me && previous.site(m).is_none());
        if !taken_back && !manager_may && !manager_unseen {
            return Err(Rejection::RenamedWithoutConsent(
                old.name.clone(),
                entry.name.clone(),
            ));
        }
    }
    if let Some(previous) = previous {
        // The other sites that got another identity: the trunks of the
        // tunnels to them follow.
        for (old, new) in crate::rename::rekeys(previous, &received) {
            if old != me && new != me {
                local.rekey(old, new);
            }
        }
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

    fn example() -> Network {
        parse_network(include_str!("config/testdata/network-v1.toml"))
            .unwrap()
            .network
            .unwrap()
    }

    fn id(name: &str) -> SiteId {
        SiteId::provisional(name)
    }

    #[test]
    fn accept_requires_self() {
        let network = example();
        let local = Local {
            self_id: Some(id("haus2")),
            trunk: vec!["ens19".into()],
            ..Default::default()
        };
        assert!(accept(network.clone(), &local, None, false, None).is_ok());
        let stranger = Local {
            self_id: Some(id("haus9")),
            trunk: vec!["ens19".into()],
            ..Default::default()
        };
        assert_eq!(
            accept(network, &stranger, None, false, None).unwrap_err(),
            Rejection::SelfMissing("haus9".into())
        );
    }

    #[test]
    fn this_site_needs_to_agree_to_be_renamed() {
        let previous = example();
        let local = Local {
            self_id: Some(SiteId::from_bytes([1; 32])),
            trunk: vec!["ens19".into()],
            ..Local::default()
        };
        let mut previous = previous;
        previous.rekey(id("haus2"), SiteId::from_bytes([1; 32]));
        let me = SiteId::from_bytes([1; 32]);
        // A peer renamed: nothing to follow, the trunks name its identity.
        let mut received = previous.clone();
        received.rename_site(id("heim"), "home");
        assert!(accept(received, &local, Some(&previous), false, None).is_ok());
        // This site renamed by another: only if it has a manager (haus2 has
        // none in the example).
        let mut received = previous.clone();
        received.rename_site(me, "h2");
        assert_eq!(
            accept(received.clone(), &local, Some(&previous), false, None).unwrap_err(),
            Rejection::RenamedWithoutConsent("haus2".into(), "h2".into())
        );
        let cfg = accept(received.clone(), &local, Some(&previous), true, None).unwrap();
        assert_eq!(cfg.self_name(), "h2", "taken back");
        let mut managed = previous.clone();
        managed.site_mut(me).unwrap().managed_by = Some(id("heim"));
        let cfg = accept(received, &local, Some(&managed), false, None).unwrap();
        assert_eq!(cfg.self_name(), "h2", "managed by heim");
        // A client whose manager it does not see (it hangs off another
        // site): its server knows the manager and decided.
        let mut unseen = previous.clone();
        let entry = unseen.site_mut(me).unwrap();
        entry.managed_by = Some(id("elsewhere"));
        entry.uplink.get_or_insert_with(|| id("heim"));
        let mut received = unseen.clone();
        received.rename_site(me, "h2");
        let cfg = accept(received, &local, Some(&unseen), false, None).unwrap();
        assert_eq!(cfg.self_name(), "h2");
    }

    #[test]
    fn the_own_identity_is_claimed_again_and_another_is_refused() {
        let me = SiteId::from_bytes([1; 32]);
        let other = SiteId::from_bytes([2; 32]);
        let network = example();
        let local = Local {
            self_id: Some(id("haus2")),
            trunk: vec!["ens19".into()],
            ..Local::default()
        };
        let revision = network.revision;
        let cfg = accept(network.clone(), &local, None, false, Some(me)).unwrap();
        assert!(cfg.network.site(me).is_some(), "claimed");
        assert_eq!(cfg.local.self_id, Some(me));
        assert_eq!(cfg.network.revision, revision + 1, "so that it spreads");
        // Already there: unchanged.
        let again = accept(cfg.network.clone(), &cfg.local, None, false, Some(me)).unwrap();
        assert_eq!(again.network, cfg.network);
        // From a site that did not see the claim yet: claimed again.
        let old = accept(
            network.clone(),
            &cfg.local,
            Some(&cfg.network),
            false,
            Some(me),
        );
        assert!(old.unwrap().network.site(me).is_some());
        // Another box has this site now (same name and address).
        let mut taken = cfg.network.clone();
        taken.rekey(me, other);
        let err = accept(taken, &cfg.local, Some(&cfg.network), false, Some(me)).unwrap_err();
        assert!(matches!(err, Rejection::IdentityTakenOver(..)), "{err}");
        assert!(err.to_string().contains(&other.fingerprint()), "{err}");
    }
}
