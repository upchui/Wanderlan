//! Joining a network with an invitation.
//!
//! Any site of a network can create an invitation code. A new site sends the
//! code and its name to that site (TCP, control port); the inviting site adds
//! it to the network, with the source address of the connection as its address,
//! and answers with the complete `network.toml`. Everything here is pure logic;
//! the daemon does the networking.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::api::SiteRules;
use crate::config::{
    LocalVid, MAX_SITE_NAME_LEN, Network, Site, VlanMap, VlanSet, has_errors, site_name_error,
    validate,
};

/// Version of the join protocol.
pub const PROTOCOL: u32 = 1;
/// Characters of an invitation code (Crockford base32: no I, L, O, U).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters of a code (50 bits), shown as two groups of five.
pub const CODE_LEN: usize = 10;

/// The request a joining site sends (one JSON line after `JOIN `).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    /// Protocol version, [`PROTOCOL`].
    pub protocol: u32,
    /// The invitation code.
    pub code: String,
    /// Proposed site name.
    pub name: String,
    /// True if the name is the host name, not chosen by the user.
    #[serde(default)]
    pub name_is_default: bool,
    /// wanderlan version of the joining site (for messages).
    #[serde(default)]
    pub version: String,
}

/// The answer of the inviting site (one JSON line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinReply {
    /// Accepted: the complete network configuration.
    Ok {
        /// Name the new site got.
        site: String,
        /// Name of the inviting site.
        via: String,
        /// `network.toml` including the new site.
        #[serde(alias = "mesh")]
        network: String,
    },
    /// Another change is waiting for confirmation; try again shortly.
    Busy,
    /// Refused.
    Error {
        /// Why, in plain language.
        message: String,
    },
}

/// Why a site cannot be added.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    /// The name is not a valid site name.
    #[error("{0}")]
    InvalidName(String),
    /// Another site has this name.
    #[error(
        "the name \"{name}\" is already used by the site at {address}; choose another name (--name)"
    )]
    NameTaken {
        /// The requested name.
        name: String,
        /// Address of the site that has it.
        address: Ipv4Addr,
    },
    /// Another site has this address.
    #[error(
        "the address {address} already belongs to site \"{site}\"; join as \"{site}\" or remove that site first"
    )]
    AddressTaken {
        /// The address.
        address: Ipv4Addr,
        /// The site that has it.
        site: String,
    },
    /// The resulting configuration would be invalid.
    #[error("the network configuration would become invalid: {0}")]
    Invalid(String),
    /// The rules of the invitation cannot be read.
    #[error("{0}")]
    InvalidRules(String),
}

/// What [`add_site`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The network to send (and, if `added`, to apply on the inviting site).
    pub network: Network,
    /// Final name of the joining site.
    pub name: String,
    /// False if the site was already a member (same name and address): it
    /// joins again, e.g. after a reset.
    pub added: bool,
    /// The network changed (revision + 1) and must be applied: a new site, or
    /// a member that joined again through another site and now hangs off it.
    pub changed: bool,
}

/// A client that joins again through another site now hangs off that site
/// (it reached it, maybe not its old uplink). Servers stay servers, and a
/// site never hangs off one of its own clients.
fn rejoin(network: &Network, inviter: &str, name: &str) -> Option<Admission> {
    let site = network.site(name)?;
    let current = site.uplink.as_deref()?;
    if current == inviter || name == inviter {
        return None;
    }
    // The inviter must not hang (indirectly) off the joining site.
    let mut up = network.site(inviter)?.uplink.as_deref();
    let mut steps = 0;
    while let Some(u) = up {
        if u == name || steps > network.sites.len() {
            return None;
        }
        up = network.site(u).and_then(|s| s.uplink.as_deref());
        steps += 1;
    }
    let mut new = network.clone();
    new.revision = network.revision + 1;
    let entry = new.sites.iter_mut().find(|s| s.name == name)?;
    entry.uplink = Some(inviter.to_string());
    Some(Admission {
        network: new,
        name: name.to_string(),
        added: false,
        changed: true,
    })
}

/// Adds the site `name` at `address` to `network` (revision + 1), with the
/// rules of the invitation. The new site starts **offline**: its tunnels
/// stay down until a user takes it online.
///
/// * The name in the rules wins over the requested name.
/// * A site with the same name and address is already a member: the network
///   stays as it is (rules are not applied again), except that a client
///   joining through another site now hangs off that site.
/// * The new site is a client of the inviting site (`uplink`).
/// * A default name (the host name) that is taken gets a suffix `-2`, `-3`,
///   ...; a default name whose address already belongs to a site joins as
///   that site.
/// * A chosen name that is taken, or an address of another site, is refused.
pub fn add_site(
    network: &Network,
    inviter: &str,
    name: &str,
    name_is_default: bool,
    address: Ipv4Addr,
    rules: &SiteRules,
) -> Result<Admission, JoinError> {
    let (name, name_is_default) = match rules.name.as_deref() {
        Some(n) => (n, false),
        None => (name, name_is_default),
    };
    let unchanged = |name: &str| Admission {
        network: network.clone(),
        name: name.to_string(),
        added: false,
        changed: false,
    };
    if let Some(msg) = site_name_error(name) {
        return Err(JoinError::InvalidName(msg));
    }
    if let Some(existing) = network.sites.iter().find(|s| s.address == address) {
        if existing.name == name || name_is_default {
            return Ok(rejoin(network, inviter, &existing.name)
                .unwrap_or_else(|| unchanged(&existing.name)));
        }
        return Err(JoinError::AddressTaken {
            address,
            site: existing.name.clone(),
        });
    }
    let name = match network.site(name) {
        None => name.to_string(),
        Some(taken) if !name_is_default => {
            return Err(JoinError::NameTaken {
                name: name.to_string(),
                address: taken.address,
            });
        }
        Some(_) => free_name(network, name),
    };
    let vlans = match rules.vlans.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(
            text.parse::<VlanSet>()
                .map_err(|e| JoinError::InvalidRules(format!("VLANs \"{text}\": {e}")))?,
        ),
    };
    let mut new = network.clone();
    new.revision = network.revision + 1;
    new.sites.push(Site {
        name: name.clone(),
        address,
        vlans,
        own_vlans: None,
        online: false,
        managed_by: Some(inviter.to_string()),
        self_managed: rules.allow_self,
        rename_by_manager: false,
        // Plugged into the inviting site: its only tunnel leads there.
        uplink: Some(inviter.to_string()),
    });
    if let Some(shared) = rules.native {
        new.maps.push(VlanMap {
            site: name.clone(),
            local: LocalVid::Native,
            shared,
        });
    }
    for t in &rules.translations {
        new.maps.push(VlanMap {
            site: name.clone(),
            local: LocalVid::Vid(t.local),
            shared: t.shared,
        });
    }
    let diags = validate(&new, None);
    if has_errors(&diags) {
        let first = diags
            .iter()
            .find(|d| d.is_error())
            .map(|d| d.message.clone())
            .unwrap_or_default();
        return Err(JoinError::Invalid(first));
    }
    Ok(Admission {
        network: new,
        name,
        added: true,
        changed: true,
    })
}

/// Checks the rules of a new invitation against the network as it is now,
/// with an address no site has.
pub fn check_rules(network: &Network, inviter: &str, rules: &SiteRules) -> Result<(), JoinError> {
    let address = (1..=254u8)
        .map(|last| Ipv4Addr::new(192, 0, 2, last))
        .find(|a| network.sites.iter().all(|s| s.address != *a))
        .unwrap_or(Ipv4Addr::new(198, 51, 100, 1));
    add_site(network, inviter, "new-site", true, address, rules).map(|_| ())
}

/// `base-2`, `base-3`, ... (shortened to fit) that is not used yet.
fn free_name(network: &Network, base: &str) -> String {
    (2u32..)
        .map(|n| {
            let suffix = format!("-{n}");
            let keep = MAX_SITE_NAME_LEN.saturating_sub(suffix.len());
            let stem: String = base.chars().take(keep).collect();
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|candidate| network.site(candidate).is_none())
        .unwrap_or_else(|| base.to_string())
}

/// Turns a host name into a valid site name: lowercase, `a-z0-9-` only, at
/// most [`MAX_SITE_NAME_LEN`] characters; `site` if nothing usable is left.
pub fn site_name_from_hostname(hostname: &str) -> String {
    let short = hostname.split('.').next().unwrap_or_default();
    let mut out = String::new();
    for c in short.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(MAX_SITE_NAME_LEN).collect();
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "site".into() } else { out }
}

/// Formats random bytes as an invitation code `XXXXX-XXXXX`.
pub fn code_from_bytes(bytes: &[u8; CODE_LEN]) -> String {
    let chars: String = bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
        .collect();
    format!("{}-{}", &chars[..5], &chars[5..])
}

/// Normalises a code as typed: case, dashes and spaces do not matter, and
/// the easily confused letters O, I and L count as 0, 1 and 1.
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

/// Parses `<address>` or `<address>:<port>`.
pub fn parse_target(text: &str, default_port: u16) -> Result<(Ipv4Addr, u16), String> {
    let text = text.trim();
    let (host, port) = match text.split_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>()
                .map_err(|_| format!("\"{p}\" is not a port number"))?,
        ),
        None => (text, default_port),
    };
    let address = host
        .parse::<Ipv4Addr>()
        .map_err(|_| format!("\"{host}\" is not an IPv4 address (e.g. 10.8.0.1)"))?;
    Ok((address, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VlanSet;

    fn network() -> Network {
        let mut network = Network::new(VlanSet::from_range(10, 50));
        network.revision = 4;
        network.sites.push(Site {
            name: "site1".into(),
            address: Ipv4Addr::new(10, 13, 0, 103),
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            rename_by_manager: false,
            uplink: None,
        });
        network
    }

    fn none() -> SiteRules {
        SiteRules::default()
    }

    const NEW: Ipv4Addr = Ipv4Addr::new(10, 13, 0, 104);

    #[test]
    fn a_new_site_is_added_with_the_next_revision() {
        let a = add_site(&network(), "site1", "site2", false, NEW, &none()).unwrap();
        assert!(a.added);
        assert_eq!(a.name, "site2");
        assert_eq!(a.network.revision, 5);
        assert_eq!(a.network.sites.len(), 2);
        assert_eq!(a.network.site("site2").unwrap().address, NEW);
        assert!(
            !a.network.site("site2").unwrap().online,
            "new sites start offline"
        );
        assert_eq!(
            a.network.site("site2").unwrap().uplink.as_deref(),
            Some("site1"),
            "a client of the inviting site"
        );
        assert!(a.changed);
    }

    #[test]
    fn the_rules_of_the_invitation_shape_the_new_site() {
        let rules = SiteRules {
            name: Some("branch".into()),
            vlans: Some("native,10-20".into()),
            native: None,
            translations: vec![crate::api::Translation {
                local: 130,
                shared: 15,
            }],
            allow_self: true,
        };
        let a = add_site(&network(), "site1", "wanderlan", true, NEW, &rules).unwrap();
        assert_eq!(a.name, "branch", "the name of the invitation wins");
        let site = a.network.site("branch").unwrap();
        assert_eq!(site.vlans.as_ref().unwrap().to_string(), "native,10-20");
        assert_eq!(a.network.maps_of("branch").count(), 1);
        assert_eq!(site.managed_by.as_deref(), Some("site1"));
        assert!(site.self_managed);

        let native = SiteRules {
            native: Some(14),
            ..SiteRules::default()
        };
        let b = add_site(&network(), "site1", "client", false, NEW, &native).unwrap();
        assert!(
            b.network
                .maps_of("client")
                .any(|m| m.local == LocalVid::Native && m.shared == 14)
        );

        // Rules are checked when the invitation is created.
        assert!(check_rules(&network(), "site1", &rules).is_ok());
        let bad = SiteRules {
            native: Some(99),
            ..SiteRules::default()
        };
        assert!(matches!(
            check_rules(&network(), "site1", &bad),
            Err(JoinError::Invalid(_))
        ));
        let taken = SiteRules {
            name: Some("site1".into()),
            ..SiteRules::default()
        };
        assert!(matches!(
            check_rules(&network(), "site1", &taken),
            Err(JoinError::NameTaken { .. })
        ));
        let unreadable = SiteRules {
            vlans: Some("ten".into()),
            ..SiteRules::default()
        };
        assert!(matches!(
            check_rules(&network(), "site1", &unreadable),
            Err(JoinError::InvalidRules(_))
        ));
    }

    #[test]
    fn a_member_joining_again_gets_the_network_unchanged() {
        let first = add_site(&network(), "site1", "site2", false, NEW, &none()).unwrap();
        let again = add_site(&first.network, "site1", "site2", false, NEW, &none()).unwrap();
        assert!(!again.added);
        assert_eq!(again.network, first.network);
        // With its host name it joins as the site that has its address.
        let by_host = add_site(&first.network, "site1", "wanderlan", true, NEW, &none()).unwrap();
        assert!(!by_host.added);
        assert!(!by_host.changed);
        assert_eq!(by_host.name, "site2");
    }

    #[test]
    fn a_client_joining_again_through_another_site_hangs_off_that_site() {
        // site1 and site3 are servers, site2 a client of site1.
        let mut base = add_site(&network(), "site1", "site2", false, NEW, &none())
            .unwrap()
            .network;
        base.sites.push(Site {
            name: "site3".into(),
            address: Ipv4Addr::new(10, 13, 0, 105),
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            rename_by_manager: false,
            uplink: None,
        });
        let moved = add_site(&base, "site3", "site2", false, NEW, &none()).unwrap();
        assert!(!moved.added && moved.changed);
        assert_eq!(moved.network.revision, base.revision + 1);
        let site2 = moved.network.site("site2").unwrap();
        assert_eq!(site2.uplink.as_deref(), Some("site3"));
        assert_eq!(
            site2.managed_by.as_deref(),
            Some("site1"),
            "the manager stays"
        );

        // A server joining again stays a server.
        let server = add_site(
            &base,
            "site3",
            "site1",
            false,
            Ipv4Addr::new(10, 13, 0, 103),
            &none(),
        )
        .unwrap();
        assert!(!server.changed);
        assert_eq!(server.network.site("site1").unwrap().uplink, None);

        // Never off one of its own clients.
        let mut chain = base.clone();
        chain.sites.push(Site {
            name: "site4".into(),
            address: Ipv4Addr::new(10, 13, 0, 106),
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            rename_by_manager: false,
            uplink: Some("site2".into()),
        });
        let no_loop = add_site(&chain, "site4", "site2", false, NEW, &none()).unwrap();
        assert!(!no_loop.changed);
        assert_eq!(
            no_loop.network.site("site2").unwrap().uplink.as_deref(),
            Some("site1")
        );
    }

    #[test]
    fn a_taken_default_name_gets_a_suffix() {
        let a = add_site(&network(), "site1", "site1", true, NEW, &none()).unwrap();
        assert_eq!(a.name, "site1-2");
        let b = add_site(
            &a.network,
            "site1",
            "site1",
            true,
            Ipv4Addr::new(10, 13, 0, 105),
            &none(),
        )
        .unwrap();
        assert_eq!(b.name, "site1-3");
        let long = add_site(&network(), "site1", "abcdefghijkl", true, NEW, &none()).unwrap();
        assert_eq!(long.name, "abcdefghijkl");
        let longer = add_site(
            &long.network,
            "site1",
            "abcdefghijkl",
            true,
            Ipv4Addr::new(10, 1, 1, 1),
            &none(),
        )
        .unwrap();
        assert_eq!(longer.name, "abcdefghij-2");
    }

    #[test]
    fn a_chosen_name_or_a_foreign_address_is_refused() {
        assert!(matches!(
            add_site(&network(), "site1", "site1", false, NEW, &none()),
            Err(JoinError::NameTaken { .. })
        ));
        assert!(matches!(
            add_site(
                &network(),
                "site1",
                "other",
                false,
                Ipv4Addr::new(10, 13, 0, 103),
                &none()
            ),
            Err(JoinError::AddressTaken { .. })
        ));
        assert!(matches!(
            add_site(&network(), "site1", "Bad_Name", false, NEW, &none()),
            Err(JoinError::InvalidName(_))
        ));
    }

    #[test]
    fn host_names_become_site_names() {
        assert_eq!(site_name_from_hostname("wanderlan"), "wanderlan");
        assert_eq!(site_name_from_hostname("PVE-Node_01.lan"), "pve-node-01");
        assert_eq!(
            site_name_from_hostname("averyverylonghostname"),
            "averyverylon"
        );
        assert_eq!(site_name_from_hostname("---"), "site");
        assert!(site_name_error(&site_name_from_hostname("x--y-")).is_none());
    }

    #[test]
    fn codes_are_formatted_and_normalised() {
        let code = code_from_bytes(&[0, 1, 2, 3, 31, 32, 33, 200, 255, 10]);
        assert_eq!(code, "0123Z-018ZA");
        assert_eq!(normalize_code("k7f3q-9qx2m"), "K7F3Q9QX2M");
        assert_eq!(normalize_code("O1L I"), "0111");
    }

    #[test]
    fn targets_with_and_without_port() {
        assert_eq!(
            parse_target("10.13.0.103", 51821),
            Ok((Ipv4Addr::new(10, 13, 0, 103), 51821))
        );
        assert_eq!(
            parse_target(" 10.13.0.103:4000 ", 51821),
            Ok((Ipv4Addr::new(10, 13, 0, 103), 4000))
        );
        assert!(parse_target("host", 1).is_err());
        assert!(parse_target("10.0.0.1:x", 1).is_err());
    }

    #[test]
    fn protocol_lines() {
        let reply = JoinReply::Error {
            message: "no".into(),
        };
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"error":{"message":"no"}}"#
        );
        assert_eq!(
            serde_json::to_string(&JoinReply::Busy).unwrap(),
            r#""busy""#
        );
    }
}
