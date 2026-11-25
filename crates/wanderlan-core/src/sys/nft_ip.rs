//! Where the clients of this site are: the nftables table `ip wanderlan`.
//!
//! A client behind NAT sends its tunnel packets (VXLAN) from a port the NAT
//! chooses; to answer, its server needs that port. Every few seconds the
//! client sends a keepalive frame through its tunnel (untagged, ethertype
//! `0x88b5`, see [`crate::nat`]); a rule in prerouting puts the source
//! address and port of such a packet into the set of that client, recognised
//! by the VNI of its tunnel. The daemon reads the sets.
//!
//! Cheap for the traffic: every tunnel data frame carries a VLAN tag
//! (`0x8100`), so the inner ethertype compare stops it after one look; only
//! the keepalives (one per client every 15 s) reach the set update. The
//! sets survive a rewrite of the rules (only the chains are flushed). Where
//! connection tracking is loaded, the tunnel packets skip it (`notrack`):
//! tracking them would cost every packet and gain nothing.
//!
//! Through NAT both ends of a tunnel must send from the tunnel port: a NAT
//! keeps one mapping per source port, and the answers must reach the port
//! the tunnel listens on. A VXLAN interface cannot send from one fixed port
//! (since Linux fixed an off-by-one in March 2026 its port range includes
//! the upper end, and a range of one port means "any"), so it gets the
//! range of the tunnel port and the one above, and a rule in output puts
//! the tunnel port back into the packets of these tunnels (by their VNI;
//! one compare for the other packets, none where the kernel only uses the
//! lower end).
//!
//! The table is written in the syntax of `nft -f` (the JSON of nftables
//! 1.0, Debian 12, knows no dynamic sets); it is read back as JSON.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use nftables::helper;
use tokio::io::AsyncWriteExt as _;

use super::{SysError, SysResult};

/// Name of the table (family ip).
pub const TABLE: &str = "wanderlan";
/// The base chain (prerouting).
const CHAIN: &str = "pre";
/// The chain with one rule per client, reached by keepalives only.
const LEARN_CHAIN: &str = "wl-learn";
/// The base chain of the packets this site sends (output), for `notrack`.
const OUT_CHAIN: &str = "out";
/// Comment of the rules that exempt the tunnel packets from tracking.
const NOTRACK: &str = "wl notrack";
/// Comment prefix of the rules that set the source port: port and VNI.
const FIXED_PREFIX: &str = "wl fixed ";
/// Prefix of the set of one client.
const SET_PREFIX: &str = "ep-";
/// How long a learned endpoint stays without a new keepalive.
pub const ENDPOINT_TIMEOUT_SECS: u32 = 90;
/// Endpoints kept per client (a NAT that rebinds leaves the old one).
const SET_SIZE: u32 = 8;
/// Ethertype of the keepalive frames (IEEE local experimental).
pub const KEEPALIVE_ETHERTYPE: u16 = 0x88b5;
/// The flags of a VXLAN header: the VNI is valid.
const VXLAN_FLAGS: u8 = 0x08;
// Offsets in bits from the start of the UDP header: 8 bytes UDP, the VXLAN
// header (1 byte flags, 3 reserved, 3 bytes VNI, 1 reserved), then the
// inner Ethernet header (two MAC addresses before the ethertype).
const OFFSET_FLAGS: u32 = 64;
const OFFSET_VNI: u32 = 96;
const OFFSET_ETHERTYPE: u32 = 224;

/// One client whose endpoint is learned: by the VNI of its tunnel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LearnRule {
    /// The client site.
    pub client: String,
    /// The VNI of its tunnel.
    pub vni: u32,
}

impl LearnRule {
    fn comment(&self) -> String {
        format!("wl learn {} {}", self.client, self.vni)
    }

    fn from_comment(comment: &str) -> Option<Self> {
        let mut parts = comment.split_whitespace();
        if parts.next() != Some("wl") || parts.next() != Some("learn") {
            return None;
        }
        let client = parts.next()?.to_string();
        let vni = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some(Self { client, vni })
    }

    fn set(&self) -> String {
        format!("{SET_PREFIX}{}", self.client)
    }
}

/// What the table holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LearnState {
    /// The UDP port the gate watches, if there is a gate.
    pub port: Option<u16>,
    /// The learn rules.
    pub rules: BTreeSet<LearnRule>,
    /// The sets by client.
    pub sets: BTreeSet<String>,
    /// The tunnel packets skip connection tracking.
    pub notrack: bool,
    /// The tunnels (by VNI) whose packets get the tunnel port as source.
    pub fixed: BTreeSet<u32>,
    /// The tunnel port of these rules.
    pub fixed_port: Option<u16>,
}

/// One learned endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Learned {
    /// Source address and port of the client's tunnel packets.
    pub addr: SocketAddrV4,
    /// How long the entry stays (the largest is the newest).
    pub expires: u64,
}

/// The table `ip wanderlan`.
pub trait LearnOps: Send + Sync {
    /// What the table holds, or `None` without it.
    fn learn_state(&self) -> impl Future<Output = SysResult<Option<LearnState>>> + Send;

    /// Writes the gate for `port` and the rules (the sets of other clients
    /// go, the others keep their entries); with `notrack`, the tunnel
    /// packets to and from `port` skip connection tracking; the packets of
    /// the tunnels in `fixed` (VNIs) get `port` as source port.
    fn set_learn(
        &self,
        port: u16,
        rules: &BTreeSet<LearnRule>,
        notrack: bool,
        fixed: &BTreeSet<u32>,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Deletes the table.
    fn delete_learn(&self) -> impl Future<Output = SysResult<()>> + Send;

    /// The endpoints in the sets, by client.
    fn learned(&self) -> impl Future<Output = SysResult<BTreeMap<String, Vec<Learned>>>> + Send;
}

/// The script that writes the gate and the rules. The table, the chains
/// and the sets are created if missing (`add` keeps what exists), the
/// chains are flushed and filled, the sets of clients no longer listed
/// deleted.
pub fn learn_script(
    port: u16,
    rules: &BTreeSet<LearnRule>,
    notrack: bool,
    fixed: &BTreeSet<u32>,
    stale_sets: &BTreeSet<String>,
) -> String {
    let t = format!("ip {TABLE}");
    let mut out = String::new();
    let _ = writeln!(out, "add table {t}");
    // The raw priority: before connection tracking.
    let _ = writeln!(
        out,
        "add chain {t} {CHAIN} {{ type filter hook prerouting priority -300; policy accept; }}"
    );
    let _ = writeln!(
        out,
        "add chain {t} {OUT_CHAIN} {{ type filter hook output priority -300; policy accept; }}"
    );
    let _ = writeln!(out, "add chain {t} {LEARN_CHAIN}");
    for chain in [CHAIN, OUT_CHAIN, LEARN_CHAIN] {
        let _ = writeln!(out, "flush chain {t} {chain}");
    }
    for set in stale_sets {
        let _ = writeln!(out, "delete set {t} {set}");
    }
    for rule in rules {
        let _ = writeln!(
            out,
            "add set {t} {} {{ type ipv4_addr . inet_service; flags dynamic,timeout; timeout {ENDPOINT_TIMEOUT_SECS}s; size {SET_SIZE}; }}",
            rule.set()
        );
    }
    if notrack {
        let _ = writeln!(
            out,
            "add rule {t} {CHAIN} udp dport {port} notrack comment \"{NOTRACK}\""
        );
        let _ = writeln!(
            out,
            "add rule {t} {OUT_CHAIN} udp sport {port} notrack comment \"{NOTRACK}\""
        );
    }
    // The source port of the tunnels through NAT (see the module doc).
    let upper = port.saturating_add(1);
    for vni in fixed {
        let _ = writeln!(
            out,
            "add rule {t} {OUT_CHAIN} udp sport {upper} @th,{OFFSET_FLAGS},8 {VXLAN_FLAGS:#04x} @th,{OFFSET_VNI},24 {vni} udp sport set {port} comment \"{FIXED_PREFIX}{port} {vni}\""
        );
    }
    // The gate: tunnel packets to the port whose inner frame is a keepalive.
    if !rules.is_empty() {
        let _ = writeln!(
            out,
            "add rule {t} {CHAIN} udp dport {port} @th,{OFFSET_ETHERTYPE},16 {KEEPALIVE_ETHERTYPE:#06x} jump {LEARN_CHAIN} comment \"wl learn gate {port}\""
        );
    }
    for rule in rules {
        let _ = writeln!(
            out,
            "add rule {t} {LEARN_CHAIN} @th,{OFFSET_FLAGS},8 {VXLAN_FLAGS:#04x} @th,{OFFSET_VNI},24 {} update @{} {{ ip saddr . udp sport }} comment \"{}\"",
            rule.vni,
            rule.set(),
            rule.comment()
        );
    }
    out
}

/// How long one `nft` call may take.
const NFT_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `nft -f -` with `script`.
async fn apply_script(what: &str, script: &str) -> SysResult<()> {
    let run = async {
        let mut child = tokio::process::Command::new(helper::DEFAULT_NFT.unwrap_or("nft"))
            .args(["-f", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes()).await?;
        }
        child.wait_with_output().await
    };
    let output = tokio::time::timeout(NFT_TIMEOUT, run)
        .await
        .map_err(|_| {
            SysError::new(
                what,
                format!("nft did not answer within {} s", NFT_TIMEOUT.as_secs()),
            )
        })?
        .map_err(|e| SysError::new(what, e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(SysError::new(
            what,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// Reads the state from `nft -j list table ip wanderlan`.
pub fn parse_state(json: &str) -> Option<LearnState> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut state = LearnState::default();
    for object in value.get("nftables")?.as_array()? {
        if let Some(rule) = object.get("rule") {
            let comment = rule.get("comment").and_then(|c| c.as_str()).unwrap_or("");
            if let Some(port) = comment.strip_prefix("wl learn gate ") {
                state.port = port.trim().parse().ok();
            } else if comment == NOTRACK {
                state.notrack = true;
            } else if let Some(rest) = comment.strip_prefix(FIXED_PREFIX) {
                let mut parts = rest.split_whitespace();
                if let (Some(port), Some(vni)) = (
                    parts.next().and_then(|p| p.parse().ok()),
                    parts.next().and_then(|v| v.parse().ok()),
                ) {
                    state.fixed_port = Some(port);
                    state.fixed.insert(vni);
                }
            } else if let Some(rule) = LearnRule::from_comment(comment) {
                state.rules.insert(rule);
            }
        } else if let Some(set) = object.get("set")
            && let Some(name) = set.get("name").and_then(|n| n.as_str())
            && name.starts_with(SET_PREFIX)
        {
            state.sets.insert(name.to_string());
        }
    }
    Some(state)
}

/// Reads the learned endpoints from `nft -j list table ip wanderlan`. The
/// shape of an element differs between nftables versions; what cannot be
/// read is left out.
pub fn parse_learned(json: &str) -> BTreeMap<String, Vec<Learned>> {
    let mut out: BTreeMap<String, Vec<Learned>> = BTreeMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return out;
    };
    let objects = value.get("nftables").and_then(|o| o.as_array());
    for set in objects.into_iter().flatten().filter_map(|o| o.get("set")) {
        let Some(client) = set
            .get("name")
            .and_then(|n| n.as_str())
            .and_then(|n| n.strip_prefix(SET_PREFIX))
        else {
            continue;
        };
        let elems = set.get("elem").and_then(|e| e.as_array());
        for elem in elems.into_iter().flatten() {
            // {"elem": {"val": {"concat": [..]}, "expires": n}} or the
            // value itself.
            let (val, expires) = match elem.get("elem") {
                Some(inner) => (
                    inner.get("val").unwrap_or(inner),
                    inner.get("expires").and_then(serde_json::Value::as_u64),
                ),
                None => (elem, None),
            };
            let Some(parts) = val.get("concat").and_then(|c| c.as_array()) else {
                continue;
            };
            let ip = parts
                .first()
                .and_then(|p| p.as_str())
                .and_then(|p| p.parse::<Ipv4Addr>().ok());
            let port = parts
                .get(1)
                .and_then(serde_json::Value::as_u64)
                .and_then(|p| u16::try_from(p).ok());
            if let (Some(ip), Some(port)) = (ip, port) {
                out.entry(client.to_string()).or_default().push(Learned {
                    addr: SocketAddrV4::new(ip, port),
                    expires: expires.unwrap_or(0),
                });
            }
        }
    }
    out
}

/// How long the state read from (or written to) the table is trusted; the
/// endpoints are always read anew.
const STATE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// The state as last read or written (`None`: no table), and when.
type CachedState = (std::time::Instant, Option<LearnState>);

/// [`LearnOps`] with the `nft` binary. The state is remembered for a while
/// (every reconcile asks for it, and a read runs `nft` twice).
#[derive(Debug, Clone, Default)]
pub struct LearnCli {
    cache: std::sync::Arc<std::sync::Mutex<Option<CachedState>>>,
}

impl LearnCli {
    fn cached(&self) -> Option<Option<LearnState>> {
        let guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .filter(|(at, _)| at.elapsed() < STATE_TTL)
            .map(|(_, state)| state.clone())
    }

    fn store(&self, state: Option<Option<LearnState>>) {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = state.map(|s| (std::time::Instant::now(), s));
    }
}

async fn list() -> SysResult<Option<String>> {
    let tables =
        helper::get_current_ruleset_raw_async(helper::DEFAULT_NFT, ["list", "tables", "ip"])
            .await
            .map_err(|e| SysError::new("list nftables tables", e))?;
    if !tables.contains(&format!("\"name\": \"{TABLE}\""))
        && !tables.contains(&format!("\"name\":\"{TABLE}\""))
    {
        return Ok(None);
    }
    helper::get_current_ruleset_raw_async(helper::DEFAULT_NFT, ["list", "table", "ip", TABLE])
        .await
        .map(Some)
        .map_err(|e| SysError::new("list nftables table ip wanderlan", e))
}

impl LearnOps for LearnCli {
    async fn learn_state(&self) -> SysResult<Option<LearnState>> {
        if let Some(state) = self.cached() {
            return Ok(state);
        }
        let state = list().await?.as_deref().and_then(parse_state);
        self.store(Some(state.clone()));
        Ok(state)
    }

    async fn set_learn(
        &self,
        port: u16,
        rules: &BTreeSet<LearnRule>,
        notrack: bool,
        fixed: &BTreeSet<u32>,
    ) -> SysResult<()> {
        let current = list()
            .await?
            .as_deref()
            .and_then(parse_state)
            .unwrap_or_default();
        let wanted: BTreeSet<String> = rules.iter().map(LearnRule::set).collect();
        let stale: BTreeSet<String> = current.sets.difference(&wanted).cloned().collect();
        let script = learn_script(port, rules, notrack, fixed, &stale);
        let result = apply_script("apply nftables table ip wanderlan", &script).await;
        self.store(result.is_ok().then(|| {
            Some(LearnState {
                port: (!rules.is_empty()).then_some(port),
                rules: rules.clone(),
                sets: wanted,
                notrack,
                fixed: fixed.clone(),
                fixed_port: (!fixed.is_empty()).then_some(port),
            })
        }));
        result
    }

    async fn delete_learn(&self) -> SysResult<()> {
        let result = apply_script(
            "delete nftables table ip wanderlan",
            &format!("delete table ip {TABLE}\n"),
        )
        .await;
        self.store(result.is_ok().then_some(None));
        result
    }

    async fn learned(&self) -> SysResult<BTreeMap<String, Vec<Learned>>> {
        Ok(list()
            .await?
            .as_deref()
            .map(parse_learned)
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> BTreeSet<LearnRule> {
        [LearnRule {
            client: "lab".into(),
            vni: 0x12_3456,
        }]
        .into()
    }

    #[test]
    fn the_script_matches_the_keepalives_of_each_client() {
        let fixed: BTreeSet<u32> = [0x12_3456].into();
        let script = learn_script(
            51822,
            &rules(),
            true,
            &fixed,
            &["ep-old".to_string()].into(),
        );
        // Prerouting at the raw priority, notrack both ways, the gate on the
        // port and the inner ethertype, the VNI at its offset, the update.
        for needle in [
            "add chain ip wanderlan pre { type filter hook prerouting priority -300; policy accept; }",
            "add chain ip wanderlan out { type filter hook output priority -300; policy accept; }",
            "flush chain ip wanderlan pre",
            "delete set ip wanderlan ep-old",
            "add set ip wanderlan ep-lab { type ipv4_addr . inet_service; flags dynamic,timeout; timeout 90s; size 8; }",
            "add rule ip wanderlan pre udp dport 51822 notrack comment \"wl notrack\"",
            "add rule ip wanderlan out udp sport 51822 notrack comment \"wl notrack\"",
            "add rule ip wanderlan pre udp dport 51822 @th,224,16 0x88b5 jump wl-learn comment \"wl learn gate 51822\"",
            "add rule ip wanderlan wl-learn @th,64,8 0x08 @th,96,24 1193046 update @ep-lab { ip saddr . udp sport } comment \"wl learn lab 1193046\"",
            // The tunnel port as source port again.
            "add rule ip wanderlan out udp sport 51823 @th,64,8 0x08 @th,96,24 1193046 udp sport set 51822 comment \"wl fixed 51822 1193046\"",
        ] {
            assert!(script.contains(needle), "{needle} not in\n{script}");
        }
        // The sets keep their entries: they are only added, never flushed.
        assert!(!script.contains("flush set"), "{script}");
        assert!(!script.contains("flush table"), "{script}");
        let (no_vnis, no_sets) = (BTreeSet::new(), BTreeSet::new());
        assert!(!learn_script(51822, &rules(), false, &no_vnis, &no_sets).contains("notrack"));
        // A client: only its source port, no gate.
        let client = learn_script(51822, &BTreeSet::new(), false, &fixed, &no_sets);
        assert!(client.contains("udp sport set 51822"), "{client}");
        assert!(!client.contains("learn gate"), "{client}");
    }

    #[test]
    fn the_state_and_the_endpoints_read_back() {
        // As nft 1.0/1.1 print it.
        let json = r#"{"nftables": [
            {"metainfo": {"version": "1.1.3", "json_schema_version": 1}},
            {"table": {"family": "ip", "name": "wanderlan", "handle": 1}},
            {"set": {"family": "ip", "name": "ep-lab", "table": "wanderlan", "type": ["ipv4_addr", "inet_service"], "handle": 3, "size": 8, "flags": ["timeout", "dynamic"], "timeout": 90,
              "elem": [{"elem": {"val": {"concat": ["203.0.113.7", 40001]}, "timeout": 90, "expires": 61}},
                       {"elem": {"val": {"concat": ["203.0.113.7", 40500]}, "timeout": 90, "expires": 87}}]}},
            {"set": {"family": "ip", "name": "ep-lab2", "table": "wanderlan", "type": ["ipv4_addr", "inet_service"], "handle": 4}},
            {"rule": {"family": "ip", "table": "wanderlan", "chain": "pre", "handle": 5, "comment": "wl learn gate 51822", "expr": []}},
            {"rule": {"family": "ip", "table": "wanderlan", "chain": "wl-learn", "handle": 6, "comment": "wl learn lab 1193046", "expr": []}},
            {"rule": {"family": "ip", "table": "wanderlan", "chain": "pre", "handle": 7, "comment": "wl notrack", "expr": []}},
            {"rule": {"family": "ip", "table": "wanderlan", "chain": "out", "handle": 8, "comment": "wl fixed 51822 1193046", "expr": []}}
        ]}"#;
        let state = parse_state(json).unwrap();
        assert_eq!(state.port, Some(51822));
        assert_eq!(state.fixed_port, Some(51822));
        assert_eq!(state.fixed, [0x12_3456].into());
        assert_eq!(state.rules, rules());
        assert!(state.notrack);
        assert_eq!(
            state.sets,
            ["ep-lab".to_string(), "ep-lab2".to_string()].into()
        );
        let learned = parse_learned(json);
        let lab = &learned["lab"];
        assert_eq!(lab.len(), 2);
        assert_eq!(lab[1].addr, "203.0.113.7:40500".parse().unwrap());
        assert_eq!(lab[1].expires, 87);
        assert!(!learned.contains_key("lab2"));
        assert!(parse_learned("not json").is_empty());
    }
}
