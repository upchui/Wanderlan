//! Joining a network with an invitation.
//!
//! Any site of a network can create an invitation code. A new site sends the
//! code and its name to that site (TCP, control port); the inviting site adds
//! it to the mesh, with the source address of the connection as its address,
//! and answers with the complete `mesh.toml`. Everything here is pure logic;
//! the daemon does the networking.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::config::{MAX_SITE_NAME_LEN, Mesh, Site, has_errors, site_name_error, validate};

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
        /// `mesh.toml` including the new site.
        mesh: String,
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
}

/// What [`add_site`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The mesh to send (and, if `added`, to apply on the inviting site).
    pub mesh: Mesh,
    /// Final name of the joining site.
    pub name: String,
    /// False if the site was already a member (same name and address): it
    /// joins again, e.g. after a reset, and the mesh is unchanged.
    pub added: bool,
}

/// Adds the site `name` at `address` to `mesh` (revision + 1).
///
/// * A site with the same name and address is already a member: the mesh
///   stays as it is.
/// * A default name (the host name) that is taken gets a suffix `-2`, `-3`,
///   ...; a default name whose address already belongs to a site joins as
///   that site.
/// * A chosen name that is taken, or an address of another site, is refused.
pub fn add_site(
    mesh: &Mesh,
    name: &str,
    name_is_default: bool,
    address: Ipv4Addr,
) -> Result<Admission, JoinError> {
    let unchanged = |name: &str| Admission {
        mesh: mesh.clone(),
        name: name.to_string(),
        added: false,
    };
    if let Some(msg) = site_name_error(name) {
        return Err(JoinError::InvalidName(msg));
    }
    if let Some(existing) = mesh.sites.iter().find(|s| s.address == address) {
        if existing.name == name || name_is_default {
            return Ok(unchanged(&existing.name));
        }
        return Err(JoinError::AddressTaken {
            address,
            site: existing.name.clone(),
        });
    }
    let name = match mesh.site(name) {
        None => name.to_string(),
        Some(taken) if !name_is_default => {
            return Err(JoinError::NameTaken {
                name: name.to_string(),
                address: taken.address,
            });
        }
        Some(_) => free_name(mesh, name),
    };
    let mut new = mesh.clone();
    new.revision = mesh.revision + 1;
    new.sites.push(Site {
        name: name.clone(),
        address,
        vlans: None,
    });
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
        mesh: new,
        name,
        added: true,
    })
}

/// `base-2`, `base-3`, ... (shortened to fit) that is not used yet.
fn free_name(mesh: &Mesh, base: &str) -> String {
    (2u32..)
        .map(|n| {
            let suffix = format!("-{n}");
            let keep = MAX_SITE_NAME_LEN.saturating_sub(suffix.len());
            let stem: String = base.chars().take(keep).collect();
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|candidate| mesh.site(candidate).is_none())
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

    fn mesh() -> Mesh {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.revision = 4;
        mesh.sites.push(Site {
            name: "site1".into(),
            address: Ipv4Addr::new(10, 13, 0, 103),
            vlans: None,
        });
        mesh
    }

    const NEW: Ipv4Addr = Ipv4Addr::new(10, 13, 0, 104);

    #[test]
    fn a_new_site_is_added_with_the_next_revision() {
        let a = add_site(&mesh(), "site2", false, NEW).unwrap();
        assert!(a.added);
        assert_eq!(a.name, "site2");
        assert_eq!(a.mesh.revision, 5);
        assert_eq!(a.mesh.sites.len(), 2);
        assert_eq!(a.mesh.site("site2").unwrap().address, NEW);
    }

    #[test]
    fn a_member_joining_again_gets_the_mesh_unchanged() {
        let first = add_site(&mesh(), "site2", false, NEW).unwrap();
        let again = add_site(&first.mesh, "site2", false, NEW).unwrap();
        assert!(!again.added);
        assert_eq!(again.mesh, first.mesh);
        // With its host name it joins as the site that has its address.
        let by_host = add_site(&first.mesh, "wanderlan", true, NEW).unwrap();
        assert!(!by_host.added);
        assert_eq!(by_host.name, "site2");
    }

    #[test]
    fn a_taken_default_name_gets_a_suffix() {
        let a = add_site(&mesh(), "site1", true, NEW).unwrap();
        assert_eq!(a.name, "site1-2");
        let b = add_site(&a.mesh, "site1", true, Ipv4Addr::new(10, 13, 0, 105)).unwrap();
        assert_eq!(b.name, "site1-3");
        let long = add_site(&mesh(), "abcdefghijkl", true, NEW).unwrap();
        assert_eq!(long.name, "abcdefghijkl");
        let longer =
            add_site(&long.mesh, "abcdefghijkl", true, Ipv4Addr::new(10, 1, 1, 1)).unwrap();
        assert_eq!(longer.name, "abcdefghij-2");
    }

    #[test]
    fn a_chosen_name_or_a_foreign_address_is_refused() {
        assert!(matches!(
            add_site(&mesh(), "site1", false, NEW),
            Err(JoinError::NameTaken { .. })
        ));
        assert!(matches!(
            add_site(&mesh(), "other", false, Ipv4Addr::new(10, 13, 0, 103)),
            Err(JoinError::AddressTaken { .. })
        ));
        assert!(matches!(
            add_site(&mesh(), "Bad_Name", false, NEW),
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
