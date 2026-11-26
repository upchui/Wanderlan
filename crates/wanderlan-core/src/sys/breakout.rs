//! The local internet breakout in the kernel (see [`crate::breakout`]).
//!
//! Its veth pairs are links like the others (created, attached, given their
//! VLAN by the reconciliation). Everything else is one piece, written and
//! compared as a whole ([`BreakoutState`]):
//! * the nftables tables `bridge wanderlan-breakout` (which frames go to a
//!   router, and that nothing of a router reaches a tunnel) and `inet
//!   wanderlan-breakout` (marks, MSS, masquerade), written in the syntax of
//!   `nft -f`, recognised by the hash in their comment;
//! * the routing rules and routes back to the routers (and out of a fixed
//!   uplink), recognised by their protocol ([`crate::breakout::RT_PROTOCOL`]);
//! * the address of each router ([`crate::breakout::ROUTER_ADDRESS`]) and the
//!   static forwarding entry of its MAC address on its bridge port;
//! * the sysctls: forwarding on the routers and the uplinks, IPv6
//!   forwarding. What wanderlan changes on interfaces that are not its own
//!   (and globally) is written down first (with the boot, in
//!   [`RESTORE_FILE`]) and put back when it is no longer needed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use ipnet::IpNet;
use sha2::{Digest, Sha256};

use super::{SysError, SysResult};
use crate::breakout::{
    BACK_TABLE_BASE, MARK_BASE, MARK_MASK, OUT_TABLE_BASE, mac_text, not_public_v4, not_public_v6,
};

/// Name of both nftables tables (families bridge and inet).
pub const TABLE: &str = "wanderlan-breakout";
/// Start of the comment of the tables; the hash of their rules follows.
const COMMENT_PREFIX: &str = "wanderlan breakout ";
/// Where the sysctls are.
pub const SYSCTL_ROOT: &str = "/proc/sys/net";
/// What wanderlan changed on interfaces that are not its own, to put back.
pub const RESTORE_FILE: &str = "/var/lib/wanderlan/breakout-sysctl";
/// The boot the restore file belongs to (after a reboot the kernel has its
/// own values again).
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// How the traffic of one family of a breakout leaves the box.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Egress {
    /// Like the box's own traffic: by the main routing table, which sends
    /// it out `dev` now (the default route).
    Main {
        /// The interface the default route leaves by.
        dev: String,
    },
    /// Out of `dev`, whatever the main table says: via `gateway` (the
    /// gateway of the default route of `dev`), or straight out of a
    /// point-to-point interface.
    Fixed {
        /// The interface.
        dev: String,
        /// The next hop; `None`: point to point.
        gateway: Option<IpAddr>,
    },
}

impl Egress {
    /// The interface it leaves by.
    pub fn dev(&self) -> &str {
        match self {
            Self::Main { dev } | Self::Fixed { dev, .. } => dev,
        }
    }
}

/// One VLAN of a segment with a local breakout.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BreakoutSpec {
    /// The bridge of the segment.
    pub bridge: String,
    /// The VLAN (in the network).
    pub vid: u16,
    /// Its number in marks and tables ([`crate::breakout::id`]).
    pub id: u16,
    /// The router end (in the IP stack).
    pub router: String,
    /// The bridge port end.
    pub port: String,
    /// The MAC address of the router end.
    pub mac: [u8; 6],
    /// The trunk ports of the segment: only frames from them break out.
    pub trunks: Vec<String>,
    /// Destinations that still go through the tunnel.
    pub except: BTreeSet<IpNet>,
    /// How IPv4 leaves, or why it does not break out.
    pub v4: Result<Egress, String>,
    /// How IPv6 leaves, or why it does not break out.
    pub v6: Result<Egress, String>,
    /// The sysctls it needs (path below [`SYSCTL_ROOT`] → value).
    pub sysctls: BTreeMap<String, String>,
}

impl BreakoutSpec {
    /// The connection mark of its connections (and the number of its way
    /// back).
    pub fn mark(&self) -> u32 {
        MARK_BASE | u32::from(self.id)
    }

    /// True if at least one family breaks out.
    pub fn active(&self) -> bool {
        self.v4.is_ok() || self.v6.is_ok()
    }
}

/// A routing rule of the breakout (priority [`crate::breakout::RULE_PRIORITY`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuleSpec {
    /// IPv6 instead of IPv4.
    pub v6: bool,
    /// Packets with this mark (the answers of a breakout).
    pub fwmark: Option<u32>,
    /// Packets from this interface (a router with a fixed uplink).
    pub iif: Option<String>,
    /// The table they are looked up in.
    pub table: u32,
}

/// A default route of the breakout.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RouteSpec {
    /// IPv6 instead of IPv4.
    pub v6: bool,
    /// The table.
    pub table: u32,
    /// The interface.
    pub dev: String,
    /// The next hop; `None`: straight out of `dev`.
    pub gateway: Option<IpAddr>,
}

/// The static forwarding entry of a router's MAC address on its port.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StaticFdb {
    /// The bridge port.
    pub port: String,
    /// The MAC address of the router.
    pub mac: [u8; 6],
    /// The VLAN.
    pub vid: u16,
}

/// A sysctl wanderlan changed: what it was and what wanderlan set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Changed {
    /// The value before.
    pub original: String,
    /// The value wanderlan wrote.
    pub set: String,
}

/// Everything of the breakouts besides their veth pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BreakoutState {
    /// The hash of the nftables tables; `None` without them.
    pub nft: Option<String>,
    /// The routing rules.
    pub rules: BTreeSet<RuleSpec>,
    /// The routes.
    pub routes: BTreeSet<RouteSpec>,
    /// The routers with their address.
    pub addresses: BTreeSet<String>,
    /// The static forwarding entries.
    pub fdb: BTreeSet<StaticFdb>,
    /// The values of the sysctls asked for.
    pub sysctls: BTreeMap<String, String>,
    /// What wanderlan changed outside its own interfaces, to put back.
    pub changed: BTreeMap<String, Changed>,
}

impl BreakoutState {
    /// True if nothing of the breakout is left outside its own interfaces.
    pub fn is_empty(&self) -> bool {
        self.nft.is_none()
            && self.rules.is_empty()
            && self.routes.is_empty()
            && self.changed.is_empty()
    }

    /// True if `self` (observed) is what `want` asks for: the same rules,
    /// and nothing changed that `want` no longer needs.
    pub fn satisfies(&self, want: &Self) -> bool {
        self.nft == want.nft
            && self.rules == want.rules
            && self.routes == want.routes
            && self.addresses == want.addresses
            && self.fdb == want.fdb
            && want
                .sysctls
                .iter()
                .all(|(path, value)| self.sysctls.get(path) == Some(value))
            && self
                .changed
                .keys()
                .all(|path| want.sysctls.contains_key(path))
    }
}

/// What the system should hold for `specs` (the breakouts whose veth pairs
/// are in place).
pub fn wanted(specs: &[BreakoutSpec]) -> BreakoutState {
    let mut state = BreakoutState {
        nft: (!specs.is_empty()).then(|| nft_hash(&nft_body(specs))),
        ..BreakoutState::default()
    };
    for spec in specs {
        state.addresses.insert(spec.router.clone());
        state.fdb.insert(StaticFdb {
            port: spec.port.clone(),
            mac: spec.mac,
            vid: spec.vid,
        });
        state.sysctls.extend(spec.sysctls.clone());
        for (v6, egress) in [(false, &spec.v4), (true, &spec.v6)] {
            let Ok(egress) = egress else { continue };
            let back = BACK_TABLE_BASE | u32::from(spec.id);
            state.rules.insert(RuleSpec {
                v6,
                fwmark: Some(spec.mark()),
                iif: None,
                table: back,
            });
            state.routes.insert(RouteSpec {
                v6,
                table: back,
                dev: spec.router.clone(),
                gateway: None,
            });
            if let Egress::Fixed { dev, gateway } = egress {
                let out = OUT_TABLE_BASE | u32::from(spec.id);
                state.rules.insert(RuleSpec {
                    v6,
                    fwmark: None,
                    iif: Some(spec.router.clone()),
                    table: out,
                });
                state.routes.insert(RouteSpec {
                    v6,
                    table: out,
                    dev: dev.clone(),
                    gateway: *gateway,
                });
            }
        }
    }
    state
}

/// A set of interface names in nft syntax.
fn names<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let unique: BTreeSet<&str> = names.into_iter().collect();
    let quoted: Vec<String> = unique.iter().map(|n| format!("\"{n}\"")).collect();
    quoted.join(", ")
}

/// The destinations of a breakout that go through the tunnel, per family.
fn keep(spec: &BreakoutSpec) -> (Vec<String>, Vec<String>) {
    let mut v4: Vec<String> = not_public_v4().iter().map(ToString::to_string).collect();
    let mut v6: Vec<String> = not_public_v6().iter().map(ToString::to_string).collect();
    for net in &spec.except {
        match net {
            IpNet::V4(n) => v4.push(n.to_string()),
            IpNet::V6(n) => v6.push(n.to_string()),
        }
    }
    (v4, v6)
}

/// The rules of both tables, without their comment.
fn nft_body(specs: &[BreakoutSpec]) -> String {
    let mut out = String::new();
    let trunks = names(
        specs
            .iter()
            .flat_map(|s| s.trunks.iter().map(String::as_str)),
    );
    let ports = names(specs.iter().map(|s| s.port.as_str()));
    let routers = names(specs.iter().map(|s| s.router.as_str()));
    let portmacs: Vec<String> = specs
        .iter()
        .map(|s| format!("\"{}\" . {}", s.port, mac_text(&s.mac)))
        .collect();

    // The bridge: which frames go to a router, and that nothing of a
    // router reaches a tunnel (or a router anything but its own frames).
    let _ = writeln!(out, "table bridge {TABLE} {{");
    let _ = writeln!(out, "  comment \"@COMMENT@\"");
    if !trunks.is_empty() {
        let _ = writeln!(
            out,
            "  set trunks {{ type ifname; elements = {{ {trunks} }} }}"
        );
    } else {
        let _ = writeln!(out, "  set trunks {{ type ifname; }}");
    }
    let _ = writeln!(
        out,
        "  set ports {{ type ifname; elements = {{ {ports} }} }}"
    );
    let _ = writeln!(
        out,
        "  set portmacs {{ type ifname . ether_addr; elements = {{ {} }} }}",
        portmacs.join(", ")
    );
    for spec in specs {
        let (v4, v6) = keep(spec);
        let _ = writeln!(
            out,
            "  set keep4_{} {{ type ipv4_addr; flags interval; auto-merge; elements = {{ {} }} }}",
            spec.id,
            v4.join(", ")
        );
        let _ = writeln!(
            out,
            "  set keep6_{} {{ type ipv6_addr; flags interval; auto-merge; elements = {{ {} }} }}",
            spec.id,
            v6.join(", ")
        );
    }
    // Before the VLAN of an untagged frame is known (its port's PVID) and
    // before the forwarding lookup.
    out.push_str("  chain prerouting {\n");
    out.push_str("    type filter hook prerouting priority -200; policy accept;\n");
    out.push_str("    iifname != @trunks accept\n");
    out.push_str("    meta pkttype { broadcast, multicast } accept\n");
    for spec in specs.iter().filter(|s| s.active()) {
        let _ = writeln!(
            out,
            "    meta ibrname \"{}\" vlan id {} jump bo_{}",
            spec.bridge, spec.vid, spec.id
        );
        let _ = writeln!(
            out,
            "    meta ibrname \"{}\" ether type != 8021q meta ibrpvid {} jump bo_{}",
            spec.bridge, spec.vid, spec.id
        );
    }
    out.push_str("  }\n");
    for spec in specs.iter().filter(|s| s.active()) {
        let _ = writeln!(out, "  chain bo_{} {{", spec.id);
        let mac = mac_text(&spec.mac);
        if spec.v4.is_ok() {
            let _ = writeln!(
                out,
                "    meta protocol ip ip daddr != @keep4_{} ether daddr set {mac} accept",
                spec.id
            );
        }
        if spec.v6.is_ok() {
            let _ = writeln!(
                out,
                "    meta protocol ip6 ip6 daddr != @keep6_{} ether daddr set {mac} accept",
                spec.id
            );
        }
        out.push_str("  }\n");
    }
    out.push_str("  chain forward {\n");
    out.push_str("    type filter hook forward priority 0; policy accept;\n");
    out.push_str("    iifname @ports jump from_port\n");
    out.push_str("    oifname @ports jump to_port\n");
    out.push_str("  }\n");
    // What a router sends goes to the trunk only: its answers, ARP and
    // neighbour discovery; never into a tunnel.
    out.push_str("  chain from_port {\n");
    out.push_str("    oifname != @trunks drop\n");
    out.push_str("    meta pkttype broadcast meta protocol != arp drop\n");
    out.push_str("    meta pkttype multicast meta l4proto != 58 drop\n");
    out.push_str(
        "    meta pkttype multicast icmpv6 type != { nd-neighbor-solicit, nd-neighbor-advert } drop\n",
    );
    out.push_str("  }\n");
    // A router gets only the frames for it, from the trunk.
    out.push_str("  chain to_port {\n");
    out.push_str("    iifname != @trunks drop\n");
    out.push_str("    oifname . ether daddr != @portmacs drop\n");
    out.push_str("  }\n");
    out.push_str("}\n");

    // The IP stack: marks for the way back, MSS, masquerade.
    let _ = writeln!(out, "table inet {TABLE} {{");
    let _ = writeln!(out, "  comment \"@COMMENT@\"");
    let _ = writeln!(
        out,
        "  set routers {{ type ifname; elements = {{ {routers} }} }}"
    );
    for spec in specs {
        let (v4, v6) = keep(spec);
        let _ = writeln!(
            out,
            "  set keep4_{} {{ type ipv4_addr; flags interval; auto-merge; elements = {{ {} }} }}",
            spec.id,
            v4.join(", ")
        );
        let _ = writeln!(
            out,
            "  set keep6_{} {{ type ipv6_addr; flags interval; auto-merge; elements = {{ {} }} }}",
            spec.id,
            v6.join(", ")
        );
    }
    out.push_str("  chain prerouting {\n");
    out.push_str("    type filter hook prerouting priority mangle; policy accept;\n");
    for spec in specs {
        let _ = writeln!(
            out,
            "    iifname \"{}\" ct state new ct mark set 0x{:08x}",
            spec.router,
            spec.mark()
        );
    }
    let _ = writeln!(
        out,
        "    ct direction reply ct mark and 0x{MARK_MASK:08x} == 0x{MARK_BASE:08x} meta mark set ct mark"
    );
    out.push_str("  }\n");
    out.push_str("  chain forward {\n");
    out.push_str("    type filter hook forward priority filter; policy accept;\n");
    out.push_str("    iifname @routers oifname @routers drop\n");
    // Only to the public destinations: a device of the VLAN never reaches
    // the networks of the box this way.
    for spec in specs {
        let _ = writeln!(
            out,
            "    iifname \"{}\" ip daddr @keep4_{} drop",
            spec.router, spec.id
        );
        let _ = writeln!(
            out,
            "    iifname \"{}\" ip6 daddr @keep6_{} drop",
            spec.router, spec.id
        );
    }
    out.push_str("    iifname @routers tcp flags syn tcp option maxseg size set rt mtu\n");
    out.push_str("    oifname @routers tcp flags syn tcp option maxseg size set rt mtu\n");
    out.push_str("  }\n");
    // The box itself is not reachable from the VLAN (only its neighbour
    // discovery answers).
    out.push_str("  chain input {\n");
    out.push_str("    type filter hook input priority filter; policy accept;\n");
    out.push_str(
        "    iifname @routers icmpv6 type { nd-neighbor-solicit, nd-neighbor-advert } accept\n",
    );
    out.push_str("    iifname @routers drop\n");
    out.push_str("  }\n");
    out.push_str("  chain postrouting {\n");
    out.push_str("    type nat hook postrouting priority srcnat; policy accept;\n");
    let _ = writeln!(
        out,
        "    ct mark and 0x{MARK_MASK:08x} == 0x{MARK_BASE:08x} oifname != @routers masquerade"
    );
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

/// The hash of the rules (16 hex digits).
fn nft_hash(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The `nft -f` script that replaces both tables by those of `specs`.
pub fn nft_script(specs: &[BreakoutSpec]) -> String {
    let body = nft_body(specs);
    let comment = format!("{COMMENT_PREFIX}{}", nft_hash(&body));
    let mut out = delete_script();
    out.push_str(&body.replace("@COMMENT@", &comment));
    out
}

/// The script that removes both tables (whether they exist or not).
pub fn delete_script() -> String {
    format!(
        "table bridge {TABLE}\ndelete table bridge {TABLE}\ntable inet {TABLE}\ndelete table inet {TABLE}\n"
    )
}

/// The hash of the tables from `nft -j list tables`: `None` without them,
/// `"?"` if they do not belong together.
pub fn parse_tables(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for object in value.get("nftables")?.as_array()? {
        let Some(table) = object.get("table") else {
            continue;
        };
        if table.get("name").and_then(|n| n.as_str()) != Some(TABLE) {
            continue;
        }
        let family = table
            .get("family")
            .and_then(|f| f.as_str())
            .unwrap_or_default()
            .to_string();
        let hash = table
            .get("comment")
            .and_then(|c| c.as_str())
            .and_then(|c| c.strip_prefix(COMMENT_PREFIX))
            .unwrap_or("?")
            .to_string();
        found.insert(family, hash);
    }
    if found.is_empty() {
        return None;
    }
    match (found.get("bridge"), found.get("inet")) {
        (Some(a), Some(b)) if a == b => Some(a.clone()),
        _ => Some("?".into()),
    }
}

/// The interfaces the uplinks can be: what the box knows about each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uplinks {
    /// Every interface by name.
    pub by_name: BTreeMap<String, Uplink>,
    /// The interface of the default route for IPv4 (as the kernel routes a
    /// public address), if there is one.
    pub auto4: Option<String>,
    /// The same for IPv6.
    pub auto6: Option<String>,
}

/// One interface as an uplink.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uplink {
    /// Operationally up.
    pub up: bool,
    /// A point-to-point interface (WireGuard, PPP, ...): routes need no
    /// gateway.
    pub pointopoint: bool,
    /// Its IPv4 addresses.
    pub v4: Vec<Ipv4Addr>,
    /// Its global IPv6 addresses.
    pub v6: Vec<Ipv6Addr>,
    /// Its default route in the main table (the one with the smallest
    /// metric): `Some(None)` without a gateway.
    pub default4: Option<Option<Ipv4Addr>>,
    /// The same for IPv6.
    pub default6: Option<Option<Ipv6Addr>>,
}

/// The kernel parts of the breakout.
pub trait BreakoutOps: Send + Sync {
    /// The interfaces with their addresses and default routes.
    fn uplinks(&self) -> impl Future<Output = SysResult<Uplinks>> + Send;

    /// What is in place, with the values of the sysctls `sysctls`.
    fn breakout_state(
        &self,
        sysctls: &[String],
    ) -> impl Future<Output = SysResult<BreakoutState>> + Send;

    /// Puts everything of `specs` in place (their veth pairs exist and are
    /// attached) and removes the rest: sysctls first, the nftables tables
    /// last (frames only go to routers that can take them).
    fn set_breakout(&self, specs: &[BreakoutSpec]) -> impl Future<Output = SysResult<()>> + Send;

    /// Removes everything of the breakouts (but their veth pairs) and puts
    /// the sysctls back.
    fn delete_breakout(&self) -> impl Future<Output = SysResult<()>> + Send;
}

/// The sysctls under a root (normally [`SYSCTL_ROOT`]) and the record of
/// what was changed.
#[derive(Debug, Clone)]
pub struct Sysctls {
    root: PathBuf,
    record: PathBuf,
    boot_id: PathBuf,
}

impl Default for Sysctls {
    fn default() -> Self {
        Self::new(SYSCTL_ROOT, RESTORE_FILE, BOOT_ID)
    }
}

/// The record of changed sysctls.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Record {
    boot_id: String,
    changed: BTreeMap<String, Changed>,
}

impl Sysctls {
    /// Sysctls below `root`, recorded in `record`.
    pub fn new(
        root: impl Into<PathBuf>,
        record: impl Into<PathBuf>,
        boot_id: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root: root.into(),
            record: record.into(),
            boot_id: boot_id.into(),
        }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    /// The value of a sysctl (`ipv4/conf/ens18/forwarding`), if it exists.
    pub fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path(path))
            .ok()
            .map(|v| v.trim().to_string())
    }

    /// True if the sysctls can be written (not in a container or a unit
    /// that makes them read-only).
    pub fn writable(&self) -> bool {
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.path("ipv4/conf/all/forwarding"))
            .is_ok()
    }

    fn write(&self, path: &str, value: &str) -> SysResult<()> {
        std::fs::write(self.path(path), value).map_err(|e| {
            SysError::new(
                format!("set sysctl net.{} = {value}", path.replace('/', ".")),
                e,
            )
        })
    }

    fn boot(&self) -> String {
        std::fs::read_to_string(&self.boot_id)
            .map(|b| b.trim().to_string())
            .unwrap_or_default()
    }

    fn load(&self) -> Record {
        let boot = self.boot();
        std::fs::read_to_string(&self.record)
            .ok()
            .and_then(|text| serde_json::from_str::<Record>(&text).ok())
            // From another boot: the kernel has its own values again.
            .filter(|r| r.boot_id == boot)
            .unwrap_or(Record {
                boot_id: boot,
                changed: BTreeMap::new(),
            })
    }

    fn save(&self, record: &Record) -> SysResult<()> {
        if record.changed.is_empty() {
            return match std::fs::remove_file(&self.record) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(SysError::new(
                    format!("remove {}", self.record.display()),
                    e,
                )),
                _ => Ok(()),
            };
        }
        let text = serde_json::to_string_pretty(record)
            .map_err(|e| SysError::new("write the changed sysctls", e))?;
        let tmp = self.record.with_extension("new");
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, &self.record))
            .map_err(|e| SysError::new(format!("write {}", self.record.display()), e))
    }

    /// What wanderlan changed (and has not put back yet).
    pub fn changed(&self) -> BTreeMap<String, Changed> {
        self.load().changed
    }

    /// Sets `wanted` and puts back what was changed and is no longer
    /// wanted. Sysctls of wanderlan's own interfaces are not recorded (they
    /// go with the interface).
    pub fn apply(&self, wanted: &BTreeMap<String, String>) -> SysResult<()> {
        let mut record = self.load();
        let gone: Vec<String> = record
            .changed
            .keys()
            .filter(|p| !wanted.contains_key(*p))
            .cloned()
            .collect();
        for path in gone {
            if let Some(changed) = record.changed.remove(&path) {
                self.put_back(&path, &changed);
            }
        }
        self.save(&record)?;
        for (path, value) in wanted {
            let current = self.read(path);
            if current.as_deref() == Some(value.as_str()) {
                continue;
            }
            if !own_interface(path) && !record.changed.contains_key(path) {
                record.changed.insert(
                    path.clone(),
                    Changed {
                        original: current.unwrap_or_default(),
                        set: value.clone(),
                    },
                );
                // Written down before it is changed.
                self.save(&record)?;
            }
            self.write(path, value)?;
        }
        Ok(())
    }

    /// Puts back everything that was changed.
    pub fn restore(&self) -> SysResult<()> {
        let record = self.load();
        for (path, changed) in &record.changed {
            self.put_back(path, changed);
        }
        self.save(&Record::default())
    }

    /// A value goes back only if it is still the one wanderlan set (and the
    /// interface is still there).
    fn put_back(&self, path: &str, changed: &Changed) {
        if self.read(path).as_deref() == Some(changed.set.as_str())
            && let Err(e) = self.write(path, &changed.original)
        {
            tracing::warn!("{e}");
        }
    }
}

/// True for the sysctls of the breakout's own interfaces.
fn own_interface(path: &str) -> bool {
    path.split('/')
        .nth(2)
        .is_some_and(crate::breakout::is_breakout_name)
}

/// The sysctl path of a key of interface `dev`.
pub fn conf(v6: bool, dev: &str, key: &str) -> String {
    format!("{}/conf/{dev}/{key}", if v6 { "ipv6" } else { "ipv4" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: u16) -> BreakoutSpec {
        BreakoutSpec {
            bridge: "br0".into(),
            vid: id,
            id,
            router: format!("wlbo{id}"),
            port: format!("wlbp{id}"),
            mac: crate::breakout::mac(0, id),
            trunks: vec!["ens19".into()],
            except: ["203.0.113.0/24".parse().unwrap()].into(),
            v4: Ok(Egress::Main {
                dev: "ens18".into(),
            }),
            v6: Err("no IPv6 default route".into()),
            sysctls: [(conf(false, "ens18", "forwarding"), "1".to_string())].into(),
        }
    }

    #[test]
    fn the_script_diverts_only_active_families_and_carries_its_hash() {
        let script = nft_script(&[spec(10)]);
        assert!(script.starts_with(&delete_script()));
        assert!(script.contains("meta ibrname \"br0\" vlan id 10 jump bo_10"));
        assert!(script.contains("ip daddr != @keep4_10 ether daddr set 02:77:6c:00:00:0a"));
        assert!(!script.contains("ip6 daddr != @keep6_10 ether daddr set"));
        assert!(script.contains("203.0.113.0/24"));
        assert!(script.contains("ct mark set 0x5742000a"));
        let hash = nft_hash(&nft_body(&[spec(10)]));
        assert!(script.contains(&format!("comment \"wanderlan breakout {hash}\"")));
        // Another exception: other rules.
        let mut other = spec(10);
        other.except.clear();
        assert_ne!(nft_hash(&nft_body(&[other])), hash);
    }

    #[test]
    fn the_wanted_state_has_a_way_back_per_family() {
        let mut s = spec(10);
        s.v6 = Ok(Egress::Fixed {
            dev: "wg0".into(),
            gateway: None,
        });
        let state = wanted(&[s]);
        assert_eq!(state.rules.len(), 3, "{:?}", state.rules);
        assert!(state.rules.contains(&RuleSpec {
            v6: true,
            fwmark: None,
            iif: Some("wlbo10".into()),
            table: OUT_TABLE_BASE | 10,
        }));
        assert!(state.routes.contains(&RouteSpec {
            v6: true,
            table: OUT_TABLE_BASE | 10,
            dev: "wg0".into(),
            gateway: None,
        }));
        assert!(wanted(&[]).is_empty());
    }

    #[test]
    fn the_tables_are_found_by_their_comment() {
        let json = r#"{"nftables": [{"metainfo": {}},
            {"table": {"family": "bridge", "name": "wanderlan-breakout", "comment": "wanderlan breakout abc"}},
            {"table": {"family": "inet", "name": "wanderlan-breakout", "comment": "wanderlan breakout abc"}}]}"#;
        assert_eq!(parse_tables(json).as_deref(), Some("abc"));
        let half = r#"{"nftables": [{"table": {"family": "inet", "name": "wanderlan-breakout"}}]}"#;
        assert_eq!(parse_tables(half).as_deref(), Some("?"));
        assert_eq!(parse_tables(r#"{"nftables": []}"#), None);
    }

    #[test]
    fn changed_sysctls_are_recorded_and_put_back() {
        let dir = std::env::temp_dir().join(format!("wl-sysctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("net");
        for p in ["ipv4/conf/ens18", "ipv4/conf/wlbo10", "ipv6/conf/all"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        std::fs::write(root.join("ipv4/conf/ens18/forwarding"), "0\n").unwrap();
        std::fs::write(root.join("ipv4/conf/wlbo10/forwarding"), "0\n").unwrap();
        std::fs::write(root.join("ipv6/conf/all/forwarding"), "1\n").unwrap();
        std::fs::write(dir.join("boot"), "b1\n").unwrap();
        let sysctls = Sysctls::new(&root, dir.join("record"), dir.join("boot"));
        let wanted: BTreeMap<String, String> = [
            (conf(false, "ens18", "forwarding"), "1".to_string()),
            (conf(false, "wlbo10", "forwarding"), "1".to_string()),
            (conf(true, "all", "forwarding"), "1".to_string()),
        ]
        .into();
        sysctls.apply(&wanted).unwrap();
        assert_eq!(
            sysctls.read("ipv4/conf/ens18/forwarding").as_deref(),
            Some("1")
        );
        // Only what was changed outside its own interfaces is recorded.
        let changed = sysctls.changed();
        assert_eq!(
            changed.keys().collect::<Vec<_>>(),
            ["ipv4/conf/ens18/forwarding"]
        );
        // No longer wanted: put back.
        sysctls.apply(&BTreeMap::new()).unwrap();
        assert_eq!(
            sysctls.read("ipv4/conf/ens18/forwarding").as_deref(),
            Some("0")
        );
        assert!(!dir.join("record").exists());
        // Changed by someone else in between: left as it is.
        std::fs::write(root.join("ipv4/conf/ens18/rp_filter"), "1\n").unwrap();
        let strict: BTreeMap<String, String> =
            [(conf(false, "ens18", "rp_filter"), "2".to_string())].into();
        sysctls.apply(&strict).unwrap();
        std::fs::write(root.join("ipv4/conf/ens18/rp_filter"), "0\n").unwrap();
        sysctls.restore().unwrap();
        assert_eq!(
            sysctls.read("ipv4/conf/ens18/rp_filter").as_deref(),
            Some("0")
        );
        // After a reboot the record is void.
        sysctls.apply(&wanted).unwrap();
        assert!(!sysctls.changed().is_empty());
        std::fs::write(dir.join("boot"), "b2\n").unwrap();
        assert!(sysctls.changed().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
