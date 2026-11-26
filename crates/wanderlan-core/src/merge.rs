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
//!
//! The rest (tunnels, MSS clamping, split horizon, the learn table) only
//! adds up: the names of the tunnels differ between the networks.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::VlanSet;
use crate::desired::{Desired, TrunkSpec};

/// The box's desired state and what the networks lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// What the box should look like.
    pub desired: Desired,
    /// What a network lost to an earlier one, by network.
    pub conflicts: BTreeMap<String, Vec<String>>,
}

/// Puts the desired states of the networks together, the first one wins
/// (see the module doc). `None` without any network.
pub fn merge(parts: &[(&str, &Desired)]) -> Option<Merged> {
    let ((first, rest), mut out) = match parts.split_first() {
        Some((first, rest)) => ((first, rest), first.1.clone()),
        None => return None,
    };
    let mut conflicts: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
            lost.push(format!(
                "everything: port {port} is a port of network \"{other}\" (`wanderlan settings` gives this site other ports)"
            ));
            conflicts.insert(network.to_string(), lost);
            continue;
        }
        port_of.extend(ports(part).map(|p| (p, *network)));
        let mut part = (*part).clone();
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
                lost.push(format!(
                    "trunk port {}: it is a port of network \"{owner}\" too, and one of them maps VLANs there",
                    trunk.name
                ));
                dropped.insert(trunk.name.clone());
                continue;
            }
            match bridge_map.get(&trunk.bridge) {
                Some(target) if *target != theirs.bridge => {
                    lost.push(format!(
                        "trunk port {}: it is in another bridge ({}) of network \"{owner}\"",
                        trunk.name, theirs.bridge
                    ));
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
            lost.push(format!(
                "segment {bridge}: an unrelated bridge of the same name belongs to another network"
            ));
        }
        part.trunks.retain(|t| !clashing.contains(&t.bridge));
        part.breakout.retain(|b| !clashing.contains(&b.bridge));
        // A segment without a trunk port has no bridge: its tunnels go.
        let segments: BTreeSet<&String> = part.trunks.iter().map(|t| &t.bridge).collect();
        let orphans: BTreeSet<String> = part
            .tunnels
            .iter()
            .filter(|t| !segments.contains(&t.bridge))
            .map(|t| t.name.clone())
            .collect();
        if !orphans.is_empty() {
            let list: Vec<&str> = orphans.iter().map(String::as_str).collect();
            lost.push(format!(
                "tunnels {}: their segment has no trunk port left",
                list.join(", ")
            ));
        }
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
            let owners: BTreeSet<&str> = out
                .trunks
                .iter()
                .filter(|t| t.bridge == *bridge)
                .filter_map(|t| trunk_of.get(&t.name).copied())
                .collect();
            let owners: Vec<String> = owners.iter().map(|o| format!("\"{o}\"")).collect();
            lost.push(format!(
                "VLANs {overlap} in bridge {bridge}: network {} has them there",
                owners.join(", ")
            ));
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
                lost.push(format!(
                    "the local breakout: network \"{other}\" breaks out on this box (one network does at most)"
                ));
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
        for name in &taken {
            lost.push(format!("tunnel {name}: the name is taken"));
        }
        drop_tunnels(&mut part, &taken);
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
            conflicts.insert(network.to_string(), lost);
        }
    }
    Some(Merged {
        desired: out,
        conflicts,
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
