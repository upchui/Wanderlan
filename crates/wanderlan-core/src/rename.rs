//! Renaming sites.
//!
//! A site is known by its name everywhere: the other sites' `uplink` and
//! `managed_by`, the mappings, the tunnel limits, `self` and the trunks per
//! tunnel in `local.toml`, the interfaces `vx-<name>`. Its identity (`id`)
//! stays when it is renamed, so a rename is recognised as the same identity
//! under a new name; sites without an identity (older versions) by their
//! address, which is unique among them. Who may rename whom is in
//! [`crate::policy`].

use crate::config::{Local, Network, Site, site_pair};

/// The renames from `old` to `new`, as (old name, new name): a site of `new`
/// that is the same box as a site of `old` which `new` no longer has, while
/// its own name is new. The same box: the same identity, or the same address
/// where one of the two has no identity. A site without an identity that
/// changes its name and its address at once is a removed and an added site.
pub fn renames(old: &Network, new: &Network) -> Vec<(String, String)> {
    new.sites
        .iter()
        .filter(|s| old.site(&s.name).is_none())
        .filter_map(|s| {
            old.sites
                .iter()
                .find(|o| same_box(o, s) && new.site(&o.name).is_none())
                .map(|o| (o.name.clone(), s.name.clone()))
        })
        .collect()
}

/// True if the two entries describe the same box.
fn same_box(a: &Site, b: &Site) -> bool {
    match (a.id, b.id) {
        (Some(x), Some(y)) => x == y,
        _ => a.address == b.address,
    }
}

impl Network {
    /// Renames the site `old` to `new`, with every reference to it: the
    /// `uplink` of its clients, `managed_by`, the mappings and the tunnel
    /// limits.
    pub fn rename_site(&mut self, old: &str, new: &str) {
        let rename = |name: &mut String| {
            if name == old {
                *name = new.to_string();
            }
        };
        for site in &mut self.sites {
            rename(&mut site.name);
            if let Some(up) = &mut site.uplink {
                rename(up);
            }
            if let Some(by) = &mut site.managed_by {
                rename(by);
            }
        }
        for map in &mut self.maps {
            rename(&mut map.site);
        }
        for tunnel in &mut self.tunnels {
            if tunnel.touches(old) {
                let [a, b] = tunnel.sites.clone();
                let name = |n: String| if n == old { new.to_string() } else { n };
                tunnel.sites = site_pair(&name(a), &name(b));
            }
        }
    }
}

impl Local {
    /// Follows the rename of the site `old` to `new`: `self` if it is this
    /// site, the trunk of the tunnel to it if it is a peer.
    pub fn rename_site(&mut self, old: &str, new: &str) {
        if self.self_name.as_deref() == Some(old) {
            self.self_name = Some(new.to_string());
        }
        if let Some(trunk) = self.tunnel_trunks.remove(old) {
            self.tunnel_trunks.insert(new.to_string(), trunk);
        }
    }
}

/// Applies the renames from `previous` to `network` to `local`; returns
/// them.
pub fn follow_renames(
    previous: &Network,
    network: &Network,
    local: &mut Local,
) -> Vec<(String, String)> {
    let renamed = renames(previous, network);
    for (old, new) in &renamed {
        local.rename_site(old, new);
    }
    renamed
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::{LocalVid, Site, TunnelVlans, VlanMap, VlanSet};

    fn site(name: &str, last: u8, uplink: Option<&str>) -> Site {
        Site {
            name: name.into(),
            address: Ipv4Addr::new(10, 0, 0, last),
            id: None,
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: uplink.map(Into::into),
            self_managed: false,
            uplink: uplink.map(Into::into),
            breakout: Default::default(),
            ports: Default::default(),
        }
    }

    /// hq a server, c a client of hq, x a client of c.
    fn network() -> Network {
        let mut network = Network::new(VlanSet::all());
        network.sites = vec![
            site("hq", 1, None),
            site("c", 2, Some("hq")),
            site("x", 3, Some("c")),
        ];
        network.maps = vec![VlanMap {
            site: "c".into(),
            local: LocalVid::Vid(130),
            shared: 30,
        }];
        network.tunnels = vec![TunnelVlans {
            sites: site_pair("c", "hq"),
            vlans: VlanSet::from_range(30, 40),
        }];
        network
    }

    #[test]
    fn a_rename_takes_its_references_along() {
        let old = network();
        let mut new = old.clone();
        new.rename_site("c", "c2");
        let names: Vec<&str> = new.sites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["hq", "c2", "x"]);
        let x = new.site("x").unwrap();
        assert_eq!(x.uplink.as_deref(), Some("c2"));
        assert_eq!(x.managed_by.as_deref(), Some("c2"));
        assert_eq!(new.maps[0].site, "c2");
        assert_eq!(new.tunnels[0].sites, site_pair("c2", "hq"));
        assert_eq!(renames(&old, &new), [("c".to_string(), "c2".to_string())]);
        assert!(renames(&old, &old).is_empty());
    }

    #[test]
    fn several_renames_and_what_is_no_rename() {
        let old = network();
        let mut new = old.clone();
        new.rename_site("hq", "main");
        new.rename_site("x", "x2");
        assert_eq!(
            renames(&old, &new),
            [
                ("hq".to_string(), "main".to_string()),
                ("x".to_string(), "x2".to_string())
            ]
        );
        // A new name with a new address: removed and added.
        let mut moved = old.clone();
        moved.rename_site("x", "y");
        moved.sites[2].address = Ipv4Addr::new(10, 0, 0, 9);
        assert!(renames(&old, &moved).is_empty());
        // The old site is still there: the new one is a new site.
        let mut added = old.clone();
        added.sites.push(Site {
            address: Ipv4Addr::new(10, 0, 0, 3),
            id: None,
            ..site("y", 0, None)
        });
        assert!(renames(&old, &added).is_empty());
    }

    #[test]
    fn with_identities_a_rename_is_the_same_identity() {
        let id = |n: u8| Some(crate::identity::SiteId::from_bytes([n; 32]));
        let mut old = network();
        for (i, site) in old.sites.iter_mut().enumerate() {
            site.id = id(i as u8 + 1);
        }
        // Renamed and moved at once: still the same box.
        let mut moved = old.clone();
        moved.rename_site("x", "y");
        moved.sites[2].address = Ipv4Addr::new(10, 0, 0, 9);
        assert_eq!(renames(&old, &moved), [("x".to_string(), "y".to_string())]);
        // Another box at the address of a site that left (behind the same
        // NAT): no rename.
        let mut other = old.clone();
        other.sites[2].name = "z".into();
        other.sites[2].id = id(9);
        assert!(renames(&old, &other).is_empty());
    }

    #[test]
    fn local_toml_follows() {
        let old = network();
        let mut new = old.clone();
        new.rename_site("c", "c2");
        new.rename_site("hq", "main");
        let mut local = Local {
            self_name: Some("c".into()),
            trunk: vec!["ens19".into()],
            tunnel_trunks: [("hq".to_string(), vec!["ens20".to_string()])].into(),
            ..Local::default()
        };
        let renamed = follow_renames(&old, &new, &mut local);
        assert_eq!(renamed.len(), 2);
        assert_eq!(local.self_name.as_deref(), Some("c2"));
        assert_eq!(
            local.tunnel_trunks.get("main").map(Vec::as_slice),
            Some(["ens20".to_string()].as_slice())
        );
        assert!(!local.tunnel_trunks.contains_key("hq"));
    }
}
