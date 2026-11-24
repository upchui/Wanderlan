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
}

/// Checks a received network against the local configuration and returns the
/// complete configuration to apply.
pub fn accept(received: Network, local: &Local) -> Result<Config, Rejection> {
    let me = local.self_name.clone().unwrap_or_default();
    if received.site(&me).is_none() {
        return Err(Rejection::SelfMissing(me));
    }
    let diags = validate(&received, Some(local));
    if let Some(err) = diags.iter().find(|d| d.is_error()) {
        return Err(Rejection::Invalid(err.message.clone()));
    }
    Ok(Config {
        network: received,
        local: local.clone(),
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
        let network = parse_network(include_str!("../../../packaging/examples/network.toml"))
            .unwrap()
            .network
            .unwrap();
        let local = Local {
            self_name: Some("haus2".into()),
            trunk: Some("ens19".into()),
            ..Default::default()
        };
        assert!(accept(network.clone(), &local).is_ok());
        let stranger = Local {
            self_name: Some("haus9".into()),
            trunk: Some("ens19".into()),
            ..Default::default()
        };
        assert_eq!(
            accept(network, &stranger).unwrap_err(),
            Rejection::SelfMissing("haus9".into())
        );
    }
}
