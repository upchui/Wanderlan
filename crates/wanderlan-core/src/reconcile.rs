//! Reconciliation: compare the desired with the actual state and change only
//! what differs.
//!
//! [`plan`] is a pure function from a snapshot of the system to a list of
//! [`Change`]s. [`reconcile`] repeats observe → plan → apply until nothing is
//! left to do, so changes that depend on each other (create a port, then add
//! its VLANs) converge in a few rounds. Applying the same configuration again
//! plans no changes at all.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddrV4;

use crate::config::VlanSet;
use crate::desired::{Desired, OWNER_ALIAS, TUNNEL_MTU};
use crate::sys::nft::{MssRule, NftState, SplitRule};
use crate::sys::nft_ip::{LearnRule, LearnState};
use crate::sys::tc::{IfTc, TcRule};
use crate::sys::{
    BridgeOptions, LinkKind, LinkState, LinkUpdate, PortVlans, SysError, System, VlanFlags,
    VxlanParams,
};
use crate::topology::{LEGACY_TUNNEL_PREFIX, is_tunnel_name};

/// Maximum number of observe/plan/apply rounds before giving up.
pub const MAX_ROUNDS: usize = 6;

/// A snapshot of the parts of the system wanderlan looks at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actual {
    /// All links by name.
    pub links: BTreeMap<String, LinkState>,
    /// VLAN entries by link index.
    pub vlans: BTreeMap<u32, PortVlans>,
    /// tc state of the links with a clsact qdisc, by index.
    pub tc: BTreeMap<u32, IfTc>,
    /// The nftables rules, `None` if the table does not exist.
    pub nft: Option<NftState>,
    /// The kernel can count traffic per VLAN and port.
    pub vlan_stats_supported: bool,
    /// Why the nftables rules could not be read (e.g. no `nft`).
    pub nft_error: Option<String>,
    /// Where the tunnel interfaces send to, by name.
    pub remotes: BTreeMap<String, SocketAddrV4>,
    /// The table `ip wanderlan`, `None` if it does not exist.
    pub learn: Option<LearnState>,
    /// Why the table `ip wanderlan` could not be read.
    pub nat_error: Option<String>,
}

impl Actual {
    /// Reads the current state from the system.
    pub async fn observe<N: System>(net: &N) -> Result<Self, SysError> {
        let links: BTreeMap<String, LinkState> = net
            .links()
            .await?
            .into_iter()
            .map(|l| (l.name.clone(), l))
            .collect();
        // The kernel names the port only where it is not the interface's.
        // A change that did not finish leaves the old address first: it is
        // set anew (which removes the others).
        let mut remotes = BTreeMap::new();
        for l in links.values() {
            if let LinkKind::Vxlan(params) = l.kind
                && let Some((address, port)) = net.tunnel_remote(l.index).await?
            {
                remotes.insert(
                    l.name.clone(),
                    SocketAddrV4::new(address, port.unwrap_or(params.port)),
                );
            }
        }
        let vlans = net.port_vlans().await?;
        let tc = net.tc_state().await?;
        // Without nftables only the MSS clamping is missing (unless the
        // split horizon needs it, see `plan`): the rest still goes on.
        let (nft, nft_error) = match net.nft_state().await {
            Ok(nft) => (nft, None),
            Err(e) => (None, Some(e.to_string())),
        };
        let (learn, nat_error) = match net.learn_state().await {
            Ok(learn) => (learn, None),
            Err(e) => (None, Some(e.to_string())),
        };
        Ok(Self {
            links,
            vlans,
            tc,
            nft,
            vlan_stats_supported: net.vlan_stats_supported(),
            nft_error,
            remotes,
            learn,
            nat_error,
        })
    }

    fn by_index(&self, index: u32) -> Option<&LinkState> {
        self.links.values().find(|l| l.index == index)
    }

    fn vlans_of(&self, link: &LinkState) -> PortVlans {
        self.vlans.get(&link.index).cloned().unwrap_or_default()
    }
}

/// Returns true if wanderlan created this link.
pub fn is_owned(link: &LinkState) -> bool {
    link.alias.as_deref() == Some(OWNER_ALIAS)
}

/// One change to the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Create the bridge.
    CreateBridge {
        name: String,
        options: BridgeOptions,
    },
    /// Change bridge options.
    SetBridgeOptions {
        name: String,
        options: BridgeOptions,
    },
    /// Create a tunnel (VXLAN); where it sends to follows (see
    /// [`Change::SetRemote`]).
    CreateTunnel {
        name: String,
        peer: String,
        params: VxlanParams,
    },
    /// Set where a tunnel sends to, in place (the port, its VLANs and
    /// counters stay); `None`: nowhere.
    SetRemote {
        name: String,
        remote: Option<SocketAddrV4>,
    },
    /// Write the rules that learn where the clients are.
    SetLearn {
        port: u16,
        rules: BTreeSet<LearnRule>,
        notrack: bool,
        fixed: BTreeSet<u32>,
    },
    /// Delete the table `ip wanderlan`.
    DeleteLearn,
    /// Delete a link created by wanderlan.
    DeleteLink { name: String, reason: String },
    /// Set the MTU of a link.
    SetMtu { name: String, mtu: u32 },
    /// Attach a link to the bridge.
    Attach { name: String, bridge: String },
    /// Take an interface that is no longer a trunk port off our bridge.
    Detach { name: String },
    /// Set split-horizon isolation of a bridge port.
    SetIsolated { name: String, isolated: bool },
    /// Bring a link up or down.
    SetUp {
        name: String,
        up: bool,
        reason: Option<String>,
    },
    /// Add or update VLAN entries of a port.
    AddVlans {
        port: String,
        vlans: Vec<(u16, VlanFlags)>,
    },
    /// Remove VLAN entries from a port.
    DelVlans { port: String, vids: Vec<u16> },
    /// Add a clsact qdisc for VLAN translation.
    AddClsact { dev: String },
    /// Remove the clsact qdisc.
    DelClsact { dev: String },
    /// Add a VLAN translation rule.
    AddTcRule { dev: String, rule: TcRule },
    /// Remove a VLAN translation rule.
    DelTcRule { dev: String, rule: TcRule },
    /// Replace the rules of the nftables table `bridge wanderlan`: MSS
    /// clamping and split horizon.
    SetNft {
        mss: BTreeSet<MssRule>,
        split: BTreeSet<SplitRule>,
    },
    /// Delete the nftables table.
    DeleteNftTable,
}

fn vid_list(vids: impl IntoIterator<Item = u16>) -> String {
    let mut set = VlanSet::new();
    for v in vids {
        // VLAN IDs in the system are always valid, insertion cannot fail.
        let _ = set.insert_checked(i64::from(v));
    }
    set.to_string()
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateBridge { name, options } => write!(
                f,
                "Creating bridge {name} with VLAN filtering, STP off and no default VLAN{}",
                if options.vlan_stats {
                    ", counting the traffic per VLAN"
                } else {
                    ""
                }
            ),
            Self::SetBridgeOptions { name, options } => write!(
                f,
                "Setting bridge {name} options (VLAN filtering {}, STP {}, default VLAN {})",
                on_off(options.vlan_filtering),
                on_off(options.stp),
                options.default_pvid
            ),
            Self::CreateTunnel { name, peer, params } => write!(
                f,
                "Creating VXLAN tunnel {name} to site \"{peer}\" (VNI {}, UDP {}, {})",
                params.vni,
                params.port,
                match params.source_port {
                    Some(_) => "through NAT",
                    None => "direct",
                }
            ),
            Self::SetRemote {
                name,
                remote: Some(remote),
            } => write!(f, "Tunnel {name} sends to {remote}"),
            Self::SetRemote { name, remote: None } => {
                write!(f, "Tunnel {name} sends nowhere until its client is seen")
            }
            Self::DeleteLink { name, reason } => write!(f, "Deleting interface {name} ({reason})"),
            Self::SetMtu { name, mtu } => write!(f, "Setting MTU of {name} to {mtu}"),
            Self::Attach { name, bridge } => write!(f, "Attaching {name} to bridge {bridge}"),
            Self::Detach { name } => {
                write!(f, "Taking {name} off its bridge (no longer a trunk port)")
            }
            Self::SetIsolated {
                name,
                isolated: true,
            } => write!(
                f,
                "Isolating port {name} (split horizon: frames from one tunnel never enter another)"
            ),
            Self::SetIsolated {
                name,
                isolated: false,
            } => {
                write!(f, "Removing isolation from port {name}")
            }
            Self::SetUp { name, up, reason } => {
                write!(f, "Bringing {name} {}", if *up { "up" } else { "down" })?;
                if let Some(reason) = reason {
                    write!(f, " ({reason})")?;
                }
                Ok(())
            }
            Self::AddVlans { port, vlans } => {
                let mut by_flags: BTreeMap<VlanFlags, Vec<u16>> = BTreeMap::new();
                for (vid, flags) in vlans {
                    by_flags.entry(*flags).or_default().push(*vid);
                }
                let parts: Vec<String> = by_flags
                    .into_iter()
                    .map(|(flags, vids)| format!("{} ({flags})", vid_list(vids)))
                    .collect();
                write!(f, "Adding VLANs {} to port {port}", parts.join(", "))
            }
            Self::DelVlans { port, vids } => write!(
                f,
                "Removing VLANs {} from port {port}",
                vid_list(vids.iter().copied())
            ),
            Self::AddClsact { dev } => {
                write!(f, "Adding a clsact qdisc to {dev} for VLAN translation")
            }
            Self::DelClsact { dev } => write!(f, "Removing the clsact qdisc from {dev}"),
            Self::AddTcRule { dev, rule } => write!(f, "On {dev}: {rule}"),
            Self::DelTcRule { dev, rule } => write!(f, "On {dev}: no longer {rule}"),
            Self::SetNft { mss, split } => {
                let mut per_port: BTreeMap<&str, u16> = BTreeMap::new();
                for r in mss.iter().filter(|r| !r.ipv6) {
                    per_port.insert(r.port.as_str(), r.mss);
                }
                let parts: Vec<String> = per_port.iter().map(|(p, m)| format!("{p} {m}")).collect();
                let split: Vec<String> = split.iter().map(ToString::to_string).collect();
                match (parts.is_empty(), split.is_empty()) {
                    (false, true) => write!(
                        f,
                        "Setting TCP MSS clamping in nftables table bridge wanderlan ({})",
                        parts.join(", ")
                    ),
                    (true, _) => write!(
                        f,
                        "Setting the split horizon in nftables table bridge wanderlan ({})",
                        split.join("; ")
                    ),
                    (false, false) => write!(
                        f,
                        "Setting TCP MSS clamping ({}) and the split horizon ({}) in nftables table bridge wanderlan",
                        parts.join(", "),
                        split.join("; ")
                    ),
                }
            }
            Self::DeleteNftTable => write!(f, "Deleting nftables table bridge wanderlan"),
            Self::SetLearn {
                port, rules, fixed, ..
            } => {
                let clients: Vec<&str> = rules.iter().map(|r| r.client.as_str()).collect();
                if clients.is_empty() && fixed.is_empty() {
                    write!(
                        f,
                        "Letting the client tunnels on UDP {port} skip connection tracking (table ip wanderlan)"
                    )
                } else if clients.is_empty() {
                    write!(
                        f,
                        "Sending the tunnel packets through NAT from UDP {port} ({} tunnel(s), table ip wanderlan)",
                        fixed.len()
                    )
                } else {
                    write!(
                        f,
                        "Learning where the clients are on UDP {port}: {}",
                        clients.join(", ")
                    )
                }
            }
            Self::DeleteLearn => write!(f, "Deleting nftables table ip wanderlan"),
        }
    }
}

fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// Reasons why the desired state cannot be reached safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// An interface with a name wanderlan needs exists and is not ours.
    #[error(
        "interface {0} already exists and was not created by wanderlan; remove or rename it, wanderlan will not touch it"
    )]
    Foreign(String),
    /// The configured trunk interface does not exist.
    #[error("trunk interface {0} does not exist")]
    TrunkMissing(String),
    /// The trunk is attached to another bridge or bond.
    #[error("trunk interface {trunk} is already attached to {master}; detach it first")]
    TrunkEnslaved { trunk: String, master: String },
    /// The split horizon needs nftables, which does not work.
    #[error("the split horizon between the server tunnels needs nftables: {0}")]
    Nft(String),
}

/// Computes the changes needed to get from `actual` to `desired`.
pub fn plan(desired: &Desired, actual: &Actual) -> Result<Vec<Change>, PlanError> {
    let mut deletes = Vec::new();
    let mut tunnel_changes = Vec::new();
    let mut bridge_changes = Vec::new();
    let mut creates = Vec::new();
    let mut settings = Vec::new();
    let mut vlan_dels = Vec::new();
    let mut vlan_adds = Vec::new();
    let mut ups = Vec::new();

    // Bridges: one per trunk (segment). Per-VLAN counters only where the
    // kernel has them.
    let options = BridgeOptions {
        vlan_stats: desired.bridge_options.vlan_stats && actual.vlan_stats_supported,
        ..desired.bridge_options
    };
    let wanted_bridges: BTreeSet<&str> = desired.bridges().collect();
    let mut bridge_index: BTreeMap<&str, u32> = BTreeMap::new();
    let mut rebuild = Vec::new();
    for name in desired.bridges() {
        let create = Change::CreateBridge {
            name: name.to_string(),
            options,
        };
        match actual.links.get(name) {
            Some(link) if !is_owned(link) => return Err(PlanError::Foreign(link.name.clone())),
            Some(link) => match &link.kind {
                LinkKind::Bridge(has) => {
                    bridge_index.insert(name, link.index);
                    if options.vlan_stats && !has.vlan_stats {
                        // The kernel takes the per-port counters only while
                        // no port has VLANs: build the bridge anew, once.
                        // Its ports are attached again in the next round.
                        rebuild.push(Change::DeleteLink {
                            name: link.name.clone(),
                            reason: "a new bridge counts the traffic per VLAN; once".into(),
                        });
                        rebuild.push(create);
                    } else {
                        // The counters stay as the bridge was built: a change
                        // cannot alter them, so they do not count here.
                        let wanted = BridgeOptions {
                            vlan_stats: has.vlan_stats,
                            ..options
                        };
                        if *has != wanted {
                            bridge_changes.push(Change::SetBridgeOptions {
                                name: link.name.clone(),
                                options: wanted,
                            });
                        }
                    }
                }
                _ => {
                    deletes.push(Change::DeleteLink {
                        name: link.name.clone(),
                        reason: "it is not a bridge".into(),
                    });
                    bridge_changes.push(create);
                }
            },
            None => bridge_changes.push(create),
        }
    }
    // Rebuilt bridges lose their ports: nothing else this round, the next
    // one plans against the new bridges.
    if !rebuild.is_empty() {
        return Ok(rebuild);
    }
    // Our bridges: ports may move between them. Bridges whose trunk is no
    // longer used are removed.
    let mut owned_bridges: BTreeSet<u32> = BTreeSet::new();
    for link in actual.links.values() {
        if !is_owned(link) || !matches!(link.kind, LinkKind::Bridge(_)) {
            continue;
        }
        owned_bridges.insert(link.index);
        if !wanted_bridges.contains(link.name.as_str()) {
            deletes.push(Change::DeleteLink {
                name: link.name.clone(),
                reason: "no tunnel uses its trunk any more".into(),
            });
        }
    }

    // Stale or changed tunnels.
    let wanted: BTreeMap<&str, _> = desired
        .tunnels
        .iter()
        .map(|t| (t.name.as_str(), t))
        .collect();
    for link in actual.links.values() {
        if !is_owned(link) || !is_tunnel_name(&link.name) {
            continue;
        }
        if link.name.starts_with(LEGACY_TUNNEL_PREFIX) {
            deletes.push(Change::DeleteLink {
                name: link.name.clone(),
                reason: "the tunnels run in VXLAN now".into(),
            });
            continue;
        }
        match (wanted.get(link.name.as_str()), &link.kind) {
            (None, _) => deletes.push(Change::DeleteLink {
                name: link.name.clone(),
                reason: "the site is no longer a peer".into(),
            }),
            // VNI, port and source port are fixed in the kernel: built anew.
            (Some(spec), LinkKind::Vxlan(params)) if *params != spec.params => {
                let reason = if params.vni != spec.params.vni {
                    "its VNI changes (from the identities of the two sites)"
                } else if params.port != spec.params.port {
                    "the tunnel port changes"
                } else {
                    "its tunnel mode changes"
                };
                deletes.push(Change::DeleteLink {
                    name: link.name.clone(),
                    reason: reason.into(),
                });
            }
            // Where it sends to changes in place.
            (Some(spec), LinkKind::Vxlan(_)) => {
                if actual.remotes.get(&link.name) != spec.remote.as_ref() {
                    tunnel_changes.push(Change::SetRemote {
                        name: link.name.clone(),
                        remote: spec.remote,
                    });
                }
            }
            (Some(_), _) => deletes.push(Change::DeleteLink {
                name: link.name.clone(),
                reason: "it is not a VXLAN interface".into(),
            }),
        }
    }
    let deleted: BTreeSet<String> = deletes
        .iter()
        .filter_map(|c| match c {
            Change::DeleteLink { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();

    // Interfaces on our bridges that are no longer trunk ports go first
    // (ours, the tunnels, are handled above and below).
    let ports: BTreeSet<&str> = desired.trunks.iter().map(|t| t.name.as_str()).collect();
    for link in actual.links.values() {
        if !is_owned(link)
            && link.master.is_some_and(|m| owned_bridges.contains(&m))
            && !ports.contains(link.name.as_str())
        {
            deletes.push(Change::Detach {
                name: link.name.clone(),
            });
        }
    }

    // Trunks.
    for spec in &desired.trunks {
        let Some(trunk) = actual.links.get(&spec.name) else {
            if spec.required {
                return Err(PlanError::TrunkMissing(spec.name.clone()));
            }
            // A further port that is not there (unplugged): the rest runs.
            continue;
        };
        if let Some(master) = trunk.master
            && !owned_bridges.contains(&master)
        {
            let master = actual
                .by_index(master)
                .map_or_else(|| format!("interface #{master}"), |l| l.name.clone());
            return Err(PlanError::TrunkEnslaved {
                trunk: trunk.name.clone(),
                master,
            });
        }
        port_changes(
            PortWanted {
                name: &spec.name,
                mtu: None,
                isolated: false,
                up: spec.down_reason.is_none(),
                up_reason: spec.down_reason.clone(),
                vlans: &spec.vlans,
            },
            trunk,
            bridge_index.get(spec.bridge.as_str()).copied(),
            &spec.bridge,
            actual,
            &mut settings,
            &mut vlan_dels,
            &mut vlan_adds,
            &mut ups,
        );
    }

    // Tunnels.
    for spec in &desired.tunnels {
        match actual.links.get(&spec.name) {
            Some(link) if !is_owned(link) => return Err(PlanError::Foreign(link.name.clone())),
            Some(link) if !deleted.contains(&link.name) => port_changes(
                PortWanted {
                    name: &spec.name,
                    mtu: Some(TUNNEL_MTU),
                    isolated: spec.isolated,
                    up: spec.enabled,
                    up_reason: spec.down_reason.clone(),
                    vlans: &spec.vlans,
                },
                link,
                bridge_index.get(spec.bridge.as_str()).copied(),
                &spec.bridge,
                actual,
                &mut settings,
                &mut vlan_dels,
                &mut vlan_adds,
                &mut ups,
            ),
            _ => creates.push(Change::CreateTunnel {
                name: spec.name.clone(),
                peer: spec.peer.clone(),
                params: spec.params,
            }),
        }
    }

    // The bridge devices themselves carry no VLANs (they have no address).
    for link in bridge_index.values().filter_map(|i| actual.by_index(*i)) {
        let current = actual.vlans_of(link);
        if !current.is_empty() {
            vlan_dels.push(Change::DelVlans {
                port: link.name.clone(),
                vids: current.keys().copied().collect(),
            });
        }
        if !link.admin_up {
            ups.push(Change::SetUp {
                name: link.name.clone(),
                up: true,
                reason: None,
            });
        }
    }

    let tc_changes = tc_changes(desired, actual);
    let want = NftState::of(&desired.mss, &desired.split);
    if let Some(error) = &actual.nft_error
        && !desired.split.is_empty()
    {
        return Err(PlanError::Nft(error.clone()));
    }
    let nft_changes = match (&actual.nft, want.is_empty()) {
        _ if actual.nft_error.is_some() => Vec::new(),
        (None, true) => Vec::new(),
        (Some(_), true) => vec![Change::DeleteNftTable],
        (Some(have), false) if *have == want => Vec::new(),
        _ => vec![Change::SetNft {
            mss: desired.mss.clone(),
            split: desired.split.clone(),
        }],
    };
    // A port loses `isolated` only once the split horizon in nftables is in
    // place (and gains it before the rules go): never a moment without.
    let (unisolate, settings): (Vec<Change>, Vec<Change>) = settings.into_iter().partition(|c| {
        matches!(
            c,
            Change::SetIsolated {
                isolated: false,
                ..
            }
        )
    });

    // The table `ip wanderlan` follows the client tunnels.
    let unwanted = desired.learn.is_empty() && desired.fixed.is_empty() && !desired.notrack;
    let learn_changes = match &actual.learn {
        Some(_) if unwanted => vec![Change::DeleteLearn],
        _ if unwanted => Vec::new(),
        have => {
            let port = desired.tunnel_port;
            let same = have.as_ref().is_some_and(|l| {
                l.port == (!desired.learn.is_empty()).then_some(port)
                    && l.fixed_port == (!desired.fixed.is_empty()).then_some(port)
                    && l.rules == desired.learn
                    && l.notrack == desired.notrack
                    && l.fixed == desired.fixed
            });
            if same {
                Vec::new()
            } else {
                vec![Change::SetLearn {
                    port,
                    rules: desired.learn.clone(),
                    notrack: desired.notrack,
                    fixed: desired.fixed.clone(),
                }]
            }
        }
    };

    let mut out = deletes;
    out.extend(tunnel_changes);
    out.extend(bridge_changes);
    out.extend(creates);
    out.extend(learn_changes);
    out.extend(settings);
    out.extend(vlan_dels);
    // The split horizon covers a new VLAN before the VLAN reaches the ports.
    out.extend(nft_changes);
    out.extend(vlan_adds);
    out.extend(tc_changes);
    out.extend(unisolate);
    out.extend(ups);
    Ok(out)
}

/// VLAN translation rules: every trunk gets exactly the desired rules;
/// rules of ours on other interfaces (e.g. a former trunk) are removed.
fn tc_changes(desired: &Desired, actual: &Actual) -> Vec<Change> {
    let mut dels = Vec::new();
    let mut adds = Vec::new();
    let mut qdisc_adds = Vec::new();
    let mut qdisc_dels = Vec::new();
    let mut trunks: BTreeSet<u32> = BTreeSet::new();

    for spec in &desired.trunks {
        let Some(trunk) = actual.links.get(&spec.name) else {
            continue;
        };
        trunks.insert(trunk.index);
        let wanted: BTreeSet<TcRule> = spec.tc.iter().copied().collect();
        let current = actual.tc.get(&trunk.index).cloned().unwrap_or_default();
        let mut have: BTreeSet<TcRule> = current.ours.iter().copied().collect();
        // Rules of an older version (one priority each) are put in place
        // anew, all in one priority.
        let gone: Vec<TcRule> = if current.legacy {
            std::mem::take(&mut have).into_iter().collect()
        } else {
            have.difference(&wanted).copied().collect()
        };
        for rule in gone {
            dels.push(Change::DelTcRule {
                dev: trunk.name.clone(),
                rule,
            });
        }
        let missing: Vec<&TcRule> = wanted.difference(&have).collect();
        if !missing.is_empty() && !current.clsact {
            qdisc_adds.push(Change::AddClsact {
                dev: trunk.name.clone(),
            });
        }
        for rule in missing {
            adds.push(Change::AddTcRule {
                dev: trunk.name.clone(),
                rule: *rule,
            });
        }
        if wanted.is_empty() && !current.ours.is_empty() && current.foreign == 0 {
            qdisc_dels.push(Change::DelClsact {
                dev: trunk.name.clone(),
            });
        }
    }

    for (index, state) in &actual.tc {
        if trunks.contains(index) || state.ours.is_empty() {
            continue;
        }
        let Some(link) = actual.by_index(*index) else {
            continue;
        };
        for rule in &state.ours {
            dels.push(Change::DelTcRule {
                dev: link.name.clone(),
                rule: *rule,
            });
        }
        if state.foreign == 0 {
            qdisc_dels.push(Change::DelClsact {
                dev: link.name.clone(),
            });
        }
    }

    let mut out = dels;
    out.extend(qdisc_dels);
    out.extend(qdisc_adds);
    out.extend(adds);
    out
}

struct PortWanted<'a> {
    name: &'a str,
    mtu: Option<u32>,
    isolated: bool,
    up: bool,
    up_reason: Option<String>,
    vlans: &'a PortVlans,
}

#[allow(clippy::too_many_arguments)]
fn port_changes(
    want: PortWanted<'_>,
    link: &LinkState,
    bridge_index: Option<u32>,
    bridge: &str,
    actual: &Actual,
    settings: &mut Vec<Change>,
    vlan_dels: &mut Vec<Change>,
    vlan_adds: &mut Vec<Change>,
    ups: &mut Vec<Change>,
) {
    let name = want.name.to_string();
    if let Some(mtu) = want.mtu
        && link.mtu != mtu
    {
        settings.push(Change::SetMtu {
            name: name.clone(),
            mtu,
        });
    }
    let attached = bridge_index.is_some() && link.master == bridge_index;
    if !attached {
        settings.push(Change::Attach {
            name: name.clone(),
            bridge: bridge.to_string(),
        });
    }
    if attached {
        // Port settings and VLANs only exist once the link is a bridge port.
        if link.isolated.unwrap_or(false) != want.isolated {
            settings.push(Change::SetIsolated {
                name: name.clone(),
                isolated: want.isolated,
            });
        }
        let current = actual.vlans_of(link);
        let dels: Vec<u16> = current
            .keys()
            .filter(|vid| !want.vlans.contains_key(vid))
            .copied()
            .collect();
        let adds: Vec<(u16, VlanFlags)> = want
            .vlans
            .iter()
            .filter(|(vid, flags)| current.get(vid) != Some(flags))
            .map(|(vid, flags)| (*vid, *flags))
            .collect();
        if !dels.is_empty() {
            vlan_dels.push(Change::DelVlans {
                port: name.clone(),
                vids: dels,
            });
        }
        if !adds.is_empty() {
            vlan_adds.push(Change::AddVlans {
                port: name.clone(),
                vlans: adds,
            });
        }
    }
    if link.admin_up != want.up {
        ups.push(Change::SetUp {
            name,
            up: want.up,
            reason: want.up_reason,
        });
    }
}

/// Changes that remove everything wanderlan created: all links marked as
/// ours. Deleting the bridge also detaches the trunk.
pub fn plan_teardown(actual: &Actual) -> Vec<Change> {
    let mut tc = Vec::new();
    for (index, state) in &actual.tc {
        let Some(link) = actual.by_index(*index) else {
            continue;
        };
        if state.ours.is_empty() || is_owned(link) {
            continue;
        }
        for rule in &state.ours {
            tc.push(Change::DelTcRule {
                dev: link.name.clone(),
                rule: *rule,
            });
        }
        if state.foreign == 0 {
            tc.push(Change::DelClsact {
                dev: link.name.clone(),
            });
        }
    }
    let links: Vec<Change> = actual
        .links
        .values()
        .filter(|l| is_owned(l))
        .map(|l| Change::DeleteLink {
            name: l.name.clone(),
            reason: "wanderlan is stopping".into(),
        })
        .collect();
    // The tunnels first (while the split horizon still holds), then the
    // nftables table and the tc rules, the bridges last (that detaches the
    // trunk).
    let (tunnels, bridges): (Vec<Change>, Vec<Change>) = links
        .into_iter()
        .partition(|c| matches!(c, Change::DeleteLink { name, .. } if is_tunnel_name(name)));
    let mut out = tunnels;
    if actual.nft.is_some() {
        out.push(Change::DeleteNftTable);
    }
    if actual.learn.is_some() {
        out.push(Change::DeleteLearn);
    }
    out.extend(tc);
    out.extend(bridges);
    out
}

/// Error from [`reconcile`] or [`teardown`].
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// The desired state is not reachable.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// A system operation failed.
    #[error(transparent)]
    Sys(#[from] SysError),
    /// Changes kept coming back.
    #[error("the system state did not converge after {MAX_ROUNDS} rounds; still pending: {0}")]
    NoConvergence(String),
    /// The teardown removed what it could; these steps failed.
    #[error("not everything could be removed: {0}")]
    Incomplete(String),
}

/// Applies one change.
pub async fn apply<N: System>(net: &N, actual: &Actual, change: &Change) -> Result<(), SysError> {
    let index_of =
        |name: &str| -> Result<u32, SysError> {
            actual.links.get(name).map(|l| l.index).ok_or_else(|| {
                SysError::new(format!("{change}"), format!("interface {name} vanished"))
            })
        };
    match change {
        Change::CreateBridge { name, options } => {
            net.create_bridge(name, options).await?;
        }
        Change::SetBridgeOptions { name, options } => {
            net.set_bridge_options(index_of(name)?, name, options)
                .await?;
        }
        Change::CreateTunnel { name, params, .. } => {
            net.create_vxlan(name, params, TUNNEL_MTU).await?;
        }
        Change::SetRemote { name, remote } => {
            let link = actual
                .links
                .get(name)
                .ok_or_else(|| SysError::new(format!("{change}"), "tunnel vanished"))?;
            let LinkKind::Vxlan(params) = link.kind else {
                return Err(SysError::new(format!("{change}"), "not a VXLAN interface"));
            };
            net.set_tunnel_remote(link.index, name, params.port, *remote)
                .await?;
        }
        Change::DeleteLink { name, .. } => net.delete_link(index_of(name)?).await?,
        Change::SetMtu { name, mtu } => {
            let update = LinkUpdate {
                mtu: Some(*mtu),
                ..LinkUpdate::default()
            };
            net.update_link(index_of(name)?, &update).await?;
        }
        Change::Attach { name, bridge } => {
            let update = LinkUpdate {
                master: Some(Some(index_of(bridge)?)),
                ..LinkUpdate::default()
            };
            net.update_link(index_of(name)?, &update).await?;
        }
        Change::Detach { name } => {
            let update = LinkUpdate {
                master: Some(None),
                ..LinkUpdate::default()
            };
            net.update_link(index_of(name)?, &update).await?;
        }
        Change::SetIsolated { name, isolated } => {
            net.set_port_isolated(index_of(name)?, *isolated).await?;
        }
        Change::SetUp { name, up, .. } => {
            let update = LinkUpdate {
                up: Some(*up),
                ..LinkUpdate::default()
            };
            net.update_link(index_of(name)?, &update).await?;
        }
        Change::AddVlans { port, vlans } => {
            let link = actual
                .links
                .get(port)
                .ok_or_else(|| SysError::new(format!("{change}"), "port vanished"))?;
            let is_bridge = matches!(link.kind, LinkKind::Bridge(_));
            net.add_vlans(link.index, is_bridge, vlans).await?;
        }
        Change::DelVlans { port, vids } => {
            let link = actual
                .links
                .get(port)
                .ok_or_else(|| SysError::new(format!("{change}"), "port vanished"))?;
            let is_bridge = matches!(link.kind, LinkKind::Bridge(_));
            net.del_vlans(link.index, is_bridge, vids).await?;
        }
        Change::AddClsact { dev } => net.add_clsact(index_of(dev)?).await?,
        Change::DelClsact { dev } => net.del_clsact(index_of(dev)?).await?,
        Change::AddTcRule { dev, rule } => net.add_tc_rule(index_of(dev)?, rule).await?,
        Change::DelTcRule { dev, rule } => net.del_tc_rule(index_of(dev)?, rule).await?,
        Change::SetNft { mss, split } => net.set_nft_rules(mss, split).await?,
        Change::DeleteNftTable => net.delete_table().await?,
        Change::SetLearn {
            port,
            rules,
            notrack,
            fixed,
        } => net.set_learn(*port, rules, *notrack, fixed).await?,
        Change::DeleteLearn => net.delete_learn().await?,
    }
    Ok(())
}

/// Returns true if the change creates or deletes a link, which invalidates
/// interface indices of the current snapshot.
fn changes_links(change: &Change) -> bool {
    matches!(
        change,
        Change::CreateBridge { .. } | Change::CreateTunnel { .. } | Change::DeleteLink { .. }
    )
}

/// Brings the system into the desired state. Calls `log` for every change
/// before it is applied and returns all applied changes.
pub async fn reconcile<N: System>(
    net: &N,
    desired: &Desired,
    mut log: impl FnMut(&Change),
) -> Result<Vec<Change>, ReconcileError> {
    let mut applied = Vec::new();
    // nftables failing only costs the MSS clamping (without split horizon):
    // everything else is applied, the error is returned at the end.
    let mut nft_failed: Option<SysError> = None;
    let is_nft = |c: &Change| matches!(c, Change::SetNft { .. } | Change::DeleteNftTable);
    // The table `ip wanderlan` only concerns the clients behind NAT: the
    // rest goes on without it.
    let is_nat = |c: &Change| matches!(c, Change::SetLearn { .. } | Change::DeleteLearn);
    let mut nat_failed: Option<SysError> = None;
    for _ in 0..MAX_ROUNDS {
        let mut actual = Actual::observe(net).await?;
        let mut changes = plan(desired, &actual)?;
        if nft_failed.is_some() {
            changes.retain(|c| !is_nft(c));
        }
        if nat_failed.is_some() {
            changes.retain(|c| !is_nat(c));
        }
        if changes.is_empty() {
            return match (nft_failed, actual.nft_error, nat_failed) {
                (Some(e), _, _) | (None, None, Some(e)) => Err(e.into()),
                (None, Some(e), _) => {
                    Err(SysError::new("read nftables table bridge wanderlan", e).into())
                }
                (None, None, None) => Ok(applied),
            };
        }
        for change in changes {
            log(&change);
            match apply(net, &actual, &change).await {
                Ok(()) => {}
                Err(e) if is_nft(&change) && desired.split.is_empty() => {
                    nft_failed = Some(e);
                    continue;
                }
                Err(e) if is_nat(&change) => {
                    nat_failed = Some(e);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            if changes_links(&change) {
                if let Change::CreateBridge { name, .. } | Change::CreateTunnel { name, .. } =
                    &change
                {
                    mark_owned(net, name).await?;
                }
                actual = Actual::observe(net).await?;
            }
            applied.push(change);
        }
    }
    let pending = plan(desired, &Actual::observe(net).await?)?;
    if pending.is_empty() {
        Ok(applied)
    } else {
        let list: Vec<String> = pending.iter().map(ToString::to_string).collect();
        Err(ReconcileError::NoConvergence(list.join("; ")))
    }
}

/// Sets the owner alias on a freshly created link. If that fails the link is
/// removed again so that no unmarked interface is left behind.
async fn mark_owned<N: System>(net: &N, name: &str) -> Result<(), SysError> {
    let link = net
        .links()
        .await?
        .into_iter()
        .find(|l| l.name == name)
        .ok_or_else(|| {
            SysError::new(
                format!("mark {name} as managed"),
                "interface not found after creation",
            )
        })?;
    let update = LinkUpdate {
        alias: Some(OWNER_ALIAS.to_string()),
        ..LinkUpdate::default()
    };
    if let Err(e) = net.update_link(link.index, &update).await {
        let _ = net.delete_link(link.index).await;
        return Err(e);
    }
    Ok(())
}

/// Returns true if the changes alter which tunnel ports exist or how they
/// forward (creation, deletion, isolation, attaching).
pub fn ports_changed(changes: &[Change]) -> bool {
    changes.iter().any(|c| match c {
        Change::CreateTunnel { .. }
        | Change::SetIsolated { .. }
        | Change::Attach { .. }
        | Change::Detach { .. } => true,
        Change::DeleteLink { name, .. } | Change::SetUp { name, .. } => is_tunnel_name(name),
        _ => false,
    })
}

/// Flushes the learned MAC addresses of the bridge. After a topology change
/// entries can point to a port through which the address is no longer
/// reachable (split horizon would drop the frames there); flushing makes the
/// bridge flood once and learn the new paths.
pub async fn flush_learned<N: System>(net: &N, bridge: &str) -> Result<(), SysError> {
    let links = net.links().await?;
    if let Some(link) = links.iter().find(|l| l.name == bridge && is_owned(l)) {
        net.flush_fdb(link.index, bridge).await?;
    }
    Ok(())
}

/// Removes everything wanderlan created. A step that fails does not stop
/// the others: the remaining tunnels, rules and bridges are still removed,
/// the failed step is tried once more, and the error then lists what failed.
pub async fn teardown<N: System>(
    net: &N,
    mut log: impl FnMut(&Change),
) -> Result<Vec<Change>, ReconcileError> {
    let mut applied = Vec::new();
    let mut failed = Vec::new();
    let mut last = observe_for_teardown(net).await?;
    for _ in 0..2 {
        let changes = plan_teardown(&last.0);
        if changes.is_empty() {
            break;
        }
        failed.clear();
        // Deleting links does not change the indices of the remaining ones.
        for change in changes {
            log(&change);
            match apply(net, &last.0, &change).await {
                Ok(()) => applied.push(change),
                Err(e) => failed.push(e.to_string()),
            }
        }
        last = observe_for_teardown(net).await?;
    }
    let (actual, problems) = last;
    let pending = plan_teardown(&actual);
    if pending.is_empty() {
        // What failed before went in the second round.
        failed.clear();
    }
    failed.extend(problems);
    if !failed.is_empty() {
        return Err(ReconcileError::Incomplete(failed.join("; ")));
    }
    if pending.is_empty() {
        Ok(applied)
    } else {
        let list: Vec<String> = pending.iter().map(ToString::to_string).collect();
        Err(ReconcileError::NoConvergence(list.join("; ")))
    }
}

/// What the teardown needs to know, and what could not be read. The links
/// say what is wanderlan's; the VLAN entries are not needed for removing,
/// and without the tc state the rest is still removed (only rules on the
/// trunk may then stay, which is reported).
async fn observe_for_teardown<N: System>(net: &N) -> Result<(Actual, Vec<String>), SysError> {
    let links = net
        .links()
        .await?
        .into_iter()
        .map(|l| (l.name.clone(), l))
        .collect();
    let mut problems = Vec::new();
    let tc = net.tc_state().await.unwrap_or_else(|e| {
        problems.push(e.to_string());
        BTreeMap::new()
    });
    let (nft, nft_error) = match net.nft_state().await {
        Ok(nft) => (nft, None),
        Err(e) => (None, Some(e.to_string())),
    };
    let (learn, nat_error) = match net.learn_state().await {
        Ok(learn) => (learn, None),
        Err(e) => (None, Some(e.to_string())),
    };
    if let Some(e) = &nat_error {
        problems.push(e.clone());
    }
    let actual = Actual {
        links,
        vlans: BTreeMap::new(),
        tc,
        nft,
        vlan_stats_supported: net.vlan_stats_supported(),
        nft_error,
        remotes: BTreeMap::new(),
        learn,
        nat_error,
    };
    Ok((actual, problems))
}

#[cfg(test)]
mod tests;
