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

use crate::config::VlanSet;
use crate::desired::{Desired, GRETAP_MTU, OWNER_ALIAS};
use crate::sys::nft::{MssRule, NftState, SplitRule};
use crate::sys::tc::{IfTc, TcRule};
use crate::sys::{
    BridgeOptions, GretapParams, LinkKind, LinkState, LinkUpdate, PortVlans, SysError, System,
    VlanFlags,
};
use crate::topology::GRETAP_PREFIX;

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
}

impl Actual {
    /// Reads the current state from the system.
    pub async fn observe<N: System>(net: &N) -> Result<Self, SysError> {
        let links = net
            .links()
            .await?
            .into_iter()
            .map(|l| (l.name.clone(), l))
            .collect();
        let vlans = net.port_vlans().await?;
        let tc = net.tc_state().await?;
        let nft = net.nft_state().await?;
        Ok(Self {
            links,
            vlans,
            tc,
            nft,
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
    /// Create a GRETAP tunnel.
    CreateGretap {
        name: String,
        peer: String,
        params: GretapParams,
    },
    /// Delete a link created by wanderlan.
    DeleteLink { name: String, reason: String },
    /// Set the MTU of a link.
    SetMtu { name: String, mtu: u32 },
    /// Attach a link to the bridge.
    Attach { name: String, bridge: String },
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
            Self::CreateBridge { name, .. } => write!(
                f,
                "Creating bridge {name} with VLAN filtering, STP off and no default VLAN"
            ),
            Self::SetBridgeOptions { name, options } => write!(
                f,
                "Setting bridge {name} options (VLAN filtering {}, STP {}, default VLAN {})",
                on_off(options.vlan_filtering),
                on_off(options.stp),
                options.default_pvid
            ),
            Self::CreateGretap { name, peer, params } => write!(
                f,
                "Creating GRETAP tunnel {name} to site \"{peer}\" ({} -> {})",
                params.local, params.remote
            ),
            Self::DeleteLink { name, reason } => write!(f, "Deleting interface {name} ({reason})"),
            Self::SetMtu { name, mtu } => write!(f, "Setting MTU of {name} to {mtu}"),
            Self::Attach { name, bridge } => write!(f, "Attaching {name} to bridge {bridge}"),
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
}

/// Computes the changes needed to get from `actual` to `desired`.
pub fn plan(desired: &Desired, actual: &Actual) -> Result<Vec<Change>, PlanError> {
    let mut deletes = Vec::new();
    let mut bridge_changes = Vec::new();
    let mut creates = Vec::new();
    let mut settings = Vec::new();
    let mut vlan_dels = Vec::new();
    let mut vlan_adds = Vec::new();
    let mut ups = Vec::new();

    // Bridges: one per trunk (segment).
    let wanted_bridges: BTreeSet<&str> = desired.bridges().collect();
    let mut bridge_index: BTreeMap<&str, u32> = BTreeMap::new();
    for name in desired.bridges() {
        let create = Change::CreateBridge {
            name: name.to_string(),
            options: desired.bridge_options,
        };
        match actual.links.get(name) {
            Some(link) if !is_owned(link) => return Err(PlanError::Foreign(link.name.clone())),
            Some(link) => match &link.kind {
                LinkKind::Bridge(options) => {
                    bridge_index.insert(name, link.index);
                    if *options != desired.bridge_options {
                        bridge_changes.push(Change::SetBridgeOptions {
                            name: link.name.clone(),
                            options: desired.bridge_options,
                        });
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
        if !is_owned(link) || !link.name.starts_with(GRETAP_PREFIX) {
            continue;
        }
        match (wanted.get(link.name.as_str()), &link.kind) {
            (None, _) => deletes.push(Change::DeleteLink {
                name: link.name.clone(),
                reason: "the site is no longer a peer".into(),
            }),
            (Some(spec), LinkKind::Gretap(params)) if *params != spec.params => {
                deletes.push(Change::DeleteLink {
                    name: link.name.clone(),
                    reason: format!(
                        "tunnel endpoints changed to {} -> {}",
                        spec.params.local, spec.params.remote
                    ),
                });
            }
            (Some(_), LinkKind::Gretap(_)) => {}
            (Some(_), _) => deletes.push(Change::DeleteLink {
                name: link.name.clone(),
                reason: "it is not a GRETAP interface".into(),
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

    // Trunks.
    for spec in &desired.trunks {
        let trunk = actual
            .links
            .get(&spec.name)
            .ok_or_else(|| PlanError::TrunkMissing(spec.name.clone()))?;
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
                up: true,
                up_reason: None,
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
                    mtu: Some(GRETAP_MTU),
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
            _ => creates.push(Change::CreateGretap {
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
    let nft_changes = match (&actual.nft, want.is_empty()) {
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

    let mut out = deletes;
    out.extend(bridge_changes);
    out.extend(creates);
    out.extend(settings);
    out.extend(vlan_dels);
    out.extend(vlan_adds);
    out.extend(tc_changes);
    out.extend(nft_changes);
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
        let have: BTreeSet<TcRule> = current.ours.iter().copied().collect();
        for rule in have.difference(&wanted) {
            dels.push(Change::DelTcRule {
                dev: trunk.name.clone(),
                rule: *rule,
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
        if wanted.is_empty() && !have.is_empty() && current.foreign == 0 {
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
    if actual.nft.is_some() {
        tc.push(Change::DeleteNftTable);
    }
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
    let mut links: Vec<Change> = actual
        .links
        .values()
        .filter(|l| is_owned(l))
        .map(|l| Change::DeleteLink {
            name: l.name.clone(),
            reason: "wanderlan is stopping".into(),
        })
        .collect();
    // Delete tunnels before the bridge so the trunk is detached last.
    links.sort_by_key(|c| match c {
        Change::DeleteLink { name, .. } if name.starts_with(GRETAP_PREFIX) => 0,
        _ => 1,
    });
    tc.extend(links);
    tc
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
        Change::CreateGretap { name, params, .. } => {
            net.create_gretap(name, params, GRETAP_MTU).await?;
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
    }
    Ok(())
}

/// Returns true if the change creates or deletes a link, which invalidates
/// interface indices of the current snapshot.
fn changes_links(change: &Change) -> bool {
    matches!(
        change,
        Change::CreateBridge { .. } | Change::CreateGretap { .. } | Change::DeleteLink { .. }
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
    for _ in 0..MAX_ROUNDS {
        let mut actual = Actual::observe(net).await?;
        let changes = plan(desired, &actual)?;
        if changes.is_empty() {
            return Ok(applied);
        }
        for change in changes {
            log(&change);
            apply(net, &actual, &change).await?;
            if changes_links(&change) {
                if let Change::CreateBridge { name, .. } | Change::CreateGretap { name, .. } =
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
        Change::CreateGretap { .. } | Change::SetIsolated { .. } | Change::Attach { .. } => true,
        Change::DeleteLink { name, .. } => name.starts_with(GRETAP_PREFIX),
        Change::SetUp { name, .. } => name.starts_with(GRETAP_PREFIX),
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

/// Removes everything wanderlan created.
pub async fn teardown<N: System>(
    net: &N,
    mut log: impl FnMut(&Change),
) -> Result<Vec<Change>, ReconcileError> {
    let mut applied = Vec::new();
    for _ in 0..2 {
        let actual = Actual::observe(net).await?;
        let changes = plan_teardown(&actual);
        if changes.is_empty() {
            return Ok(applied);
        }
        // Deleting links does not change the indices of the remaining ones.
        for change in changes {
            log(&change);
            apply(net, &actual, &change).await?;
            applied.push(change);
        }
    }
    let pending = plan_teardown(&Actual::observe(net).await?);
    if pending.is_empty() {
        Ok(applied)
    } else {
        let list: Vec<String> = pending.iter().map(ToString::to_string).collect();
        Err(ReconcileError::NoConvergence(list.join("; ")))
    }
}

#[cfg(test)]
mod tests;
