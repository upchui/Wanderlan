//! One desired state for the box from those of its networks.
//!
//! Every network derives its own [`Desired`]; the box has one kernel, so
//! they are put together before the reconcile. The networks come in their
//! order (the default network first, the others as they were added); what
//! two of them want differently goes to the earlier one, and the later one
//! is told what it lost:
//!
//! - **A trunk port in two networks** puts both segments into one bridge
//!   (the one of the earlier network). Their VLANs on it must differ: the
//!   later network loses the VLANs the earlier one uses there. A port with
//!   VLAN mappings (tc) is not shared: the later network loses it.
//! - **The ports** are the network's own: a network with the tunnel or
//!   control port of an earlier one gets nothing (the kernel takes one VNI
//!   per tunnel port, a socket takes a control port once).
//! - **A local breakout** is made by one network on the box at most.
//! - **The trunk ports of a later network** are not required: one that is
//!   missing (an unplugged adapter) leaves its segment out, it does not stop
//!   the box.
//!
//! The rest (tunnels, MSS clamping, split horizon, the learn table) only
//! adds up: the names of the tunnels differ between the networks.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::VlanSet;
use crate::desired::{Desired, TrunkSpec};

/// Something a network lost to an earlier network of the box.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lost {
    /// Everything: `port` is a port of network `owner`.
    Ports {
        /// The port.
        port: u16,
        /// The network that has it.
        owner: String,
    },
    /// A trunk port network `owner` has too, with VLAN mappings (`mapped`)
    /// or in another bridge.
    Trunk {
        /// The port.
        port: String,
        /// The network that has it.
        owner: String,
        /// One of them maps VLANs there.
        mapped: bool,
    },
    /// A segment whose bridge name an earlier network has.
    Segment {
        /// The bridge.
        bridge: String,
    },
    /// A tunnel: its segment has no trunk port left, or its name is taken.
    Tunnel {
        /// Its interface.
        name: String,
    },
    /// A VLAN of a bridge network `owner` has too.
    Vlan {
        /// The bridge.
        bridge: String,
        /// The VLAN.
        vid: u16,
        /// The network that has it there.
        owner: String,
    },
    /// The local breakout: network `owner` breaks out on this box.
    Breakout {
        /// The network that does.
        owner: String,
    },
}

impl Lost {
    /// What was lost, whoever took it (a change is checked by this).
    pub fn what(&self) -> (u8, String, u16) {
        match self {
            Self::Ports { port, .. } => (0, String::new(), *port),
            Self::Trunk { port, .. } => (1, port.clone(), 0),
            Self::Segment { bridge } => (2, bridge.clone(), 0),
            Self::Tunnel { name } => (3, name.clone(), 0),
            Self::Vlan { bridge, vid, .. } => (4, bridge.clone(), *vid),
            Self::Breakout { .. } => (5, String::new(), 0),
        }
    }
}

/// `lost` in sentences: the VLANs of a bridge together, the tunnels in one.
pub fn describe(lost: &[Lost]) -> Vec<String> {
    let mut out = Vec::new();
    let mut vlans: BTreeMap<(&str, &str), Vec<u16>> = BTreeMap::new();
    let mut tunnels: Vec<&str> = Vec::new();
    for item in lost {
        match item {
            Lost::Ports { port, owner } => out.push(format!(
                "everything: port {port} is a port of network \"{owner}\" (`wanderlan settings` gives this site other ports)"
            )),
            Lost::Trunk {
                port,
                owner,
                mapped: true,
            } => out.push(format!(
                "trunk port {port}: it is a port of network \"{owner}\" too, and one of them maps VLANs there"
            )),
            Lost::Trunk { port, owner, .. } => out.push(format!(
                "trunk port {port}: it is in another bridge of network \"{owner}\""
            )),
            Lost::Segment { bridge } => out.push(format!(
                "segment {bridge}: an unrelated bridge of the same name belongs to another network"
            )),
            Lost::Tunnel { name } => tunnels.push(name),
            Lost::Vlan { bridge, vid, owner } => {
                vlans.entry((bridge, owner)).or_default().push(*vid);
            }
            Lost::Breakout { owner } => out.push(format!(
                "the local breakout: network \"{owner}\" breaks out on this box (one network does at most)"
            )),
        }
    }
    for ((bridge, owner), vids) in vlans {
        let set: VlanSet = vids.into_iter().collect();
        out.push(format!(
            "VLANs {set} in bridge {bridge}: network \"{owner}\" has them there"
        ));
    }
    if !tunnels.is_empty() {
        out.push(format!(
            "tunnels {}: their segment has no trunk port left, or the name is taken",
            tunnels.join(", ")
        ));
    }
    out
}

/// The box's desired state and what the networks lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// What the box should look like.
    pub desired: Desired,
    /// What a network lost to an earlier one, by network.
    pub lost: BTreeMap<String, Vec<Lost>>,
    /// The same in sentences ([`describe`]).
    pub conflicts: BTreeMap<String, Vec<String>>,
    /// The bridges each network's segments are in on the box.
    pub bridges: BTreeMap<String, Vec<String>>,
}

impl Merged {
    /// True if `network` got its ports (else it got nothing at all).
    pub fn has_ports(&self, network: &str) -> bool {
        !self
            .lost
            .get(network)
            .is_some_and(|l| l.iter().any(|x| matches!(x, Lost::Ports { .. })))
    }
}

/// Puts the desired states of the networks together, the first one wins
/// (see the module doc). `None` without any network.
pub fn merge(parts: &[(&str, &Desired)]) -> Option<Merged> {
    let ((first, rest), mut out) = match parts.split_first() {
        Some((first, rest)) => ((first, rest), first.1.clone()),
        None => return None,
    };
    let mut all_lost: BTreeMap<String, Vec<Lost>> = BTreeMap::new();
    let mut bridges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    bridges.insert(
        first.0.to_string(),
        first.1.bridges().map(str::to_string).collect(),
    );
    // Who has what, for the messages.
    let mut port_of: BTreeMap<u16, &str> = ports(first.1).map(|p| (p, first.0)).collect();
    let mut trunk_of: BTreeMap<String, &str> = first
        .1
        .trunks
        .iter()
        .map(|t| (t.name.clone(), first.0))
        .collect();
    let mut breakout_of: Option<&str> = (!first.1.breakout.is_empty()).then_some(first.0);
    for (network, part) in rest {
        let mut lost = Vec::new();
        if let Some((port, other)) = ports(part).find_map(|p| Some((p, port_of.get(&p)?))) {
            lost.push(Lost::Ports {
                port,
                owner: (*other).to_string(),
            });
            all_lost.insert(network.to_string(), lost);
            continue;
        }
        port_of.extend(ports(part).map(|p| (p, *network)));
        let mut part = (*part).clone();
        // Only the main port of the first network must be there: a missing
        // port of a later one leaves its segment out (not the box).
        for trunk in &mut part.trunks {
            trunk.required = false;
        }
        // Where the bridges of this network go: into the bridge of an
        // earlier network that has one of its ports, else its own.
        let mut bridge_map: BTreeMap<String, String> = BTreeMap::new();
        let mut dropped: BTreeSet<String> = BTreeSet::new();
        for trunk in &part.trunks {
            let Some(theirs) = out.trunks.iter().find(|t| t.name == trunk.name) else {
                continue;
            };
            let owner = trunk_of.get(&trunk.name).copied().unwrap_or("?");
            if !trunk.tc.is_empty() || !theirs.tc.is_empty() {
                lost.push(Lost::Trunk {
                    port: trunk.name.clone(),
                    owner: owner.to_string(),
                    mapped: true,
                });
                dropped.insert(trunk.name.clone());
                continue;
            }
            match bridge_map.get(&trunk.bridge) {
                Some(target) if *target != theirs.bridge => {
                    lost.push(Lost::Trunk {
                        port: trunk.name.clone(),
                        owner: owner.to_string(),
                        mapped: false,
                    });
                    dropped.insert(trunk.name.clone());
                }
                Some(_) => {}
                None => {
                    bridge_map.insert(trunk.bridge.clone(), theirs.bridge.clone());
                }
            }
        }
        part.trunks.retain(|t| !dropped.contains(&t.name));
        // A bridge of its own must not have the name of an earlier one.
        let used: BTreeSet<String> = out.bridges().map(str::to_string).collect();
        let own: BTreeSet<String> = part
            .trunks
            .iter()
            .map(|t| t.bridge.clone())
            .chain(part.tunnels.iter().map(|t| t.bridge.clone()))
            .filter(|b| !bridge_map.contains_key(b))
            .collect();
        let clashing: BTreeSet<String> = own.intersection(&used).cloned().collect();
        for bridge in &clashing {
            lost.push(Lost::Segment {
                bridge: bridge.clone(),
            });
        }
        part.trunks.retain(|t| !clashing.contains(&t.bridge));
        // A segment without a trunk port has no bridge: its tunnels and its
        // breakout go.
        let segments: BTreeSet<String> = part.trunks.iter().map(|t| t.bridge.clone()).collect();
        part.breakout.retain(|b| segments.contains(&b.bridge));
        let orphans: BTreeSet<String> = part
            .tunnels
            .iter()
            .filter(|t| !segments.contains(&t.bridge))
            .map(|t| t.name.clone())
            .collect();
        lost.extend(
            orphans
                .iter()
                .map(|name| Lost::Tunnel { name: name.clone() }),
        );
        drop_tunnels(&mut part, &orphans);
        let rename = |bridge: &mut String| {
            if let Some(target) = bridge_map.get(bridge.as_str()) {
                bridge.clone_from(target);
            }
        };
        for t in &mut part.trunks {
            rename(&mut t.bridge);
        }
        for t in &mut part.tunnels {
            rename(&mut t.bridge);
        }
        for b in &mut part.breakout {
            rename(&mut b.bridge);
        }
        // The VLANs of a shared bridge: those of the earlier networks stay.
        for bridge in bridge_map.values().collect::<BTreeSet<_>>() {
            let taken = vlans_on(&out, bridge);
            let wanted = vlans_on(&part, bridge);
            let overlap: VlanSet = taken.intersection(&wanted).copied().collect();
            if overlap.is_empty() {
                continue;
            }
            let owner = out
                .trunks
                .iter()
                .filter(|t| t.bridge == *bridge)
                .find_map(|t| trunk_of.get(&t.name).copied())
                .unwrap_or("?");
            lost.extend(overlap.iter().map(|vid| Lost::Vlan {
                bridge: bridge.clone(),
                vid,
                owner: owner.to_string(),
            }));
            for t in part.trunks.iter_mut().filter(|t| t.bridge == *bridge) {
                t.vlans.retain(|v, _| !overlap.contains(*v));
            }
            for t in part.tunnels.iter_mut().filter(|t| t.bridge == *bridge) {
                t.vlans.retain(|v, _| !overlap.contains(*v));
            }
            part.breakout
                .retain(|b| b.bridge != *bridge || !overlap.contains(b.vid));
        }
        if !part.breakout.is_empty() {
            if let Some(other) = breakout_of {
                lost.push(Lost::Breakout {
                    owner: other.to_string(),
                });
                part.breakout.clear();
            } else {
                breakout_of = Some(network);
            }
        }
        // Tunnel names differ between the networks; if not, the first wins.
        let taken: BTreeSet<String> = part
            .tunnels
            .iter()
            .filter(|t| out.tunnels.iter().any(|o| o.name == t.name))
            .map(|t| t.name.clone())
            .collect();
        lost.extend(taken.iter().map(|name| Lost::Tunnel { name: name.clone() }));
        drop_tunnels(&mut part, &taken);
        bridges.insert(
            network.to_string(),
            part.trunks
                .iter()
                .map(|t| t.bridge.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        );
        for trunk in part.trunks {
            match out.trunks.iter_mut().find(|t| t.name == trunk.name) {
                Some(theirs) => add_vlans(theirs, &trunk),
                None => {
                    trunk_of.insert(trunk.name.clone(), network);
                    out.trunks.push(trunk);
                }
            }
        }
        out.tunnels.extend(part.tunnels);
        out.tunnels.sort_by(|a, b| a.name.cmp(&b.name));
        out.mss.extend(part.mss);
        out.split.extend(part.split);
        out.learn.extend(&part.learn);
        out.breakout.extend(part.breakout);
        if !lost.is_empty() {
            all_lost.insert(network.to_string(), lost);
        }
    }
    let conflicts = all_lost
        .iter()
        .map(|(n, l)| (n.clone(), describe(l)))
        .collect();
    Some(Merged {
        desired: out,
        lost: all_lost,
        conflicts,
        bridges,
    })
}

/// The ports a network takes on the box: its control port, its tunnel port
/// and the one above (the source ports through NAT).
pub fn ports(desired: &Desired) -> impl Iterator<Item = u16> + use<> {
    [
        desired.control_port,
        desired.tunnel_port,
        desired.tunnel_port.saturating_add(1),
    ]
    .into_iter()
}

/// Leaves out the tunnels `names` and what belongs to them.
fn drop_tunnels(part: &mut Desired, names: &BTreeSet<String>) {
    if names.is_empty() {
        return;
    }
    let vnis: BTreeSet<u32> = part
        .tunnels
        .iter()
        .filter(|t| names.contains(&t.name))
        .map(|t| t.params.vni)
        .collect();
    part.tunnels.retain(|t| !names.contains(&t.name));
    part.learn.rules.retain(|r| !names.contains(&r.client));
    part.learn.fixed.retain(|f| !vnis.contains(&f.vni));
    part.mss.retain(|m| !names.contains(&m.port));
    part.split = std::mem::take(&mut part.split)
        .into_iter()
        .filter_map(|mut rule| {
            rule.ports.retain(|p| !names.contains(p));
            (rule.ports.len() >= 2).then_some(rule)
        })
        .collect();
}

/// The VLANs in `bridge`: on its trunk ports and its tunnels.
fn vlans_on(desired: &Desired, bridge: &str) -> BTreeSet<u16> {
    let trunks = desired
        .trunks
        .iter()
        .filter(|t| t.bridge == bridge)
        .flat_map(|t| t.vlans.keys());
    let tunnels = desired
        .tunnels
        .iter()
        .filter(|t| t.bridge == bridge)
        .flat_map(|t| t.vlans.keys());
    trunks.chain(tunnels).copied().collect()
}

/// A trunk port of two networks: the VLANs of both (they differ).
fn add_vlans(theirs: &mut TrunkSpec, ours: &TrunkSpec) {
    for (vid, flags) in &ours.vlans {
        theirs.vlans.entry(*vid).or_insert(*flags);
    }
    theirs.required |= ours.required;
    if theirs.down_reason.is_none() {
        theirs.down_reason.clone_from(&ours.down_reason);
    }
}

#[cfg(test)]
mod tests;
