//! Renaming sites.
//!
//! A site is known by its name everywhere: the other sites' `uplink` and
//! `managed_by`, the mappings, the tunnel limits, `self` and the trunks per
//! tunnel in `local.toml`, the interfaces `gt-<name>`. Its address is
//! unique too and stays when it is renamed, so a rename is recognised as the
//! same address under a new name. Who may rename whom is in [`crate::policy`].

use crate::config::{Local, Network, site_pair};

/// The renames from `old` to `new`, as (old name, new name): a site of `new`
/// whose address belongs in `old` to a site that `new` no longer has, while
/// its own name is new. A site that changes its name and its address at once
/// is a removed and an added site.
pub fn renames(old: &Network, new: &Network) -> Vec<(String, String)> {
    new.sites
        .iter()
        .filter(|s| old.site(&s.name).is_none())
        .filter_map(|s| {
            old.sites
                .iter()
                .find(|o| o.address == s.address && new.site(&o.name).is_none())
                .map(|o| (o.name.clone(), s.name.clone()))
        })
        .collect()
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
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: uplink.map(Into::into),
            self_managed: false,
            uplink: uplink.map(Into::into),
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
            ..site("y", 0, None)
        });
        assert!(renames(&old, &added).is_empty());
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
