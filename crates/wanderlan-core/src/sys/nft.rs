//! TCP MSS clamping, where a site passes VLANs on between server tunnels
//! the split horizon, and where both sites of a VLAN mapping are in one
//! switch network broadcast DHCP replies, with nftables (table `bridge
//! wanderlan`).
//!
//! The rules are applied through the JSON interface of nftables (`nft -j`),
//! using the `nftables` crate. Every rule carries a comment that encodes it,
//! e.g. `wl mss oif vx-hub2 ip 1318`, so the current state can be read back
//! and compared without parsing expressions.
//!
//! Every bridged frame of the host passes the base chain, so it holds as
//! little as possible: the split horizon (sets) and one gate that sends TCP
//! SYNs to the chain with the MSS rules. All other frames leave after it.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nftables::batch::Batch;
use nftables::expr::{
    Expression, Meta, MetaKey, NamedExpression, Payload, PayloadBase, PayloadField, PayloadRaw,
    Range, SetItem, TcpOption,
};
use nftables::helper;
use nftables::schema::{Chain, NfListObject, NfObject, Rule, Table};
use nftables::stmt::{JumpTarget, Mangle, Match, Operator, Statement};
use nftables::types::{NfChainPolicy, NfChainType, NfFamily, NfHook};
use sha2::{Digest, Sha256};

use super::{SysError, SysResult};

/// The `>` operator. nftables 0.6.3 serialises `Operator::LT` as `">"` and
/// `Operator::GT` as `"<"` (the names are swapped in the crate); the test
/// `batch_json_contains_the_expected_rule` checks the JSON so that a fix in
/// the crate is noticed.
const GREATER_THAN: Operator = Operator::LT;

/// Name of the nftables table (family bridge).
pub const TABLE: &str = "wanderlan";
/// Name of the base chain in that table (hook forward).
pub const CHAIN: &str = "forward";
/// Name of the chain with the MSS rules, reached only by TCP SYNs.
pub const MSS_CHAIN: &str = "wl-mss";
/// Comment of the rule that sends TCP SYNs to [`MSS_CHAIN`].
const GATE: &str = "wl mss gate";

/// Whether a rule matches frames leaving or entering through a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PortDir {
    /// `iifname`.
    In,
    /// `oifname`.
    Out,
}

/// One MSS clamping rule: TCP SYNs with a larger MSS that enter or leave
/// through `port` get their MSS lowered to `mss`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MssRule {
    /// Tunnel interface.
    pub port: String,
    /// Direction.
    pub dir: PortDir,
    /// IPv6 instead of IPv4 TCP.
    pub ipv6: bool,
    /// MSS to clamp to.
    pub mss: u16,
}

impl MssRule {
    /// The comment that identifies the rule.
    pub fn comment(&self) -> String {
        format!(
            "wl mss {} {} {} {}",
            match self.dir {
                PortDir::In => "iif",
                PortDir::Out => "oif",
            },
            self.port,
            if self.ipv6 { "ip6" } else { "ip" },
            self.mss
        )
    }

    /// Parses a comment produced by [`MssRule::comment`].
    pub fn from_comment(comment: &str) -> Option<Self> {
        let mut parts = comment.split_whitespace();
        if parts.next() != Some("wl") || parts.next() != Some("mss") {
            return None;
        }
        let dir = match parts.next()? {
            "iif" => PortDir::In,
            "oif" => PortDir::Out,
            _ => return None,
        };
        let port = parts.next()?.to_string();
        let ipv6 = match parts.next()? {
            "ip" => false,
            "ip6" => true,
            _ => return None,
        };
        let mss = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            port,
            dir,
            ipv6,
            mss,
        })
    }

    fn statements(&self) -> Vec<Statement<'static>> {
        let iface_key = match self.dir {
            PortDir::In => MetaKey::Iifname,
            PortDir::Out => MetaKey::Oifname,
        };
        let maxseg = || {
            Expression::Named(NamedExpression::TcpOption(TcpOption {
                name: Cow::Borrowed("maxseg"),
                field: Some(Cow::Borrowed("size")),
            }))
        };
        vec![
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Protocol,
                })),
                right: Expression::String(Cow::Borrowed(if self.ipv6 { "ip6" } else { "ip" })),
                op: Operator::EQ,
            }),
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta { key: iface_key })),
                right: Expression::String(Cow::Owned(self.port.clone())),
                op: Operator::EQ,
            }),
            syn(),
            Statement::Match(Match {
                left: maxseg(),
                right: Expression::Number(u32::from(self.mss)),
                op: GREATER_THAN,
            }),
            Statement::Mangle(Mangle {
                key: maxseg(),
                value: Expression::Number(u32::from(self.mss)),
            }),
        ]
    }
}

impl fmt::Display for MssRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "clamp the TCP MSS of {} SYNs {} {} to {}",
            if self.ipv6 { "IPv6" } else { "IPv4" },
            match self.dir {
                PortDir::In => "arriving from",
                PortDir::Out => "leaving through",
            },
            self.port,
            self.mss
        )
    }
}

/// Split horizon in nftables: frames of `vlans` that enter through one of
/// `ports` are not passed on through another of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SplitRule {
    /// Tunnel interfaces (at least two).
    pub ports: BTreeSet<String>,
    /// VLAN ranges.
    pub vlans: Vec<(u16, u16)>,
}

impl SplitRule {
    /// The comment that identifies the rule (its content is too long for a
    /// comment, so a hash of it).
    pub fn comment(&self) -> String {
        let text = format!("{:?} {:?}", self.ports, self.vlans);
        let hash = Sha256::digest(text.as_bytes());
        let hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("wl split {hex}")
    }

    fn statements(&self) -> Vec<Statement<'static>> {
        let ports = || {
            Expression::Named(NamedExpression::Set(
                self.ports
                    .iter()
                    .map(|p| SetItem::Element(Expression::String(Cow::Owned(p.clone()))))
                    .collect(),
            ))
        };
        let vlans = Expression::Named(NamedExpression::Set(
            self.vlans
                .iter()
                .map(|&(a, b)| {
                    SetItem::Element(if a == b {
                        Expression::Number(u32::from(a))
                    } else {
                        Expression::Range(Box::new(Range {
                            range: [
                                Expression::Number(u32::from(a)),
                                Expression::Number(u32::from(b)),
                            ],
                        }))
                    })
                })
                .collect(),
        ));
        let meta = |key| Expression::Named(NamedExpression::Meta(Meta { key }));
        vec![
            Statement::Match(Match {
                left: meta(MetaKey::Iifname),
                right: ports(),
                op: Operator::EQ,
            }),
            Statement::Match(Match {
                left: meta(MetaKey::Oifname),
                right: ports(),
                op: Operator::EQ,
            }),
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                    PayloadField {
                        protocol: Cow::Borrowed("vlan"),
                        field: Cow::Borrowed("id"),
                    },
                ))),
                right: vlans,
                op: Operator::EQ,
            }),
            Statement::Drop(None),
        ]
    }
}

impl fmt::Display for SplitRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let vlans: Vec<String> = self
            .vlans
            .iter()
            .map(|&(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}-{b}")
                }
            })
            .collect();
        let ports: Vec<&str> = self.ports.iter().map(String::as_str).collect();
        write!(
            f,
            "VLANs {} not between {}",
            vlans.join(","),
            ports.join(", ")
        )
    }
}

/// Broadcast DHCP replies: DHCP requests (IPv4) that enter through `port`
/// in VLAN `vid` ask for a broadcast reply (the broadcast flag of BOOTP).
///
/// Where both sites of a VLAN mapping are in one switch network, the switch
/// sees a device twice: in the mapped VLAN at the port it is at, in the
/// source VLAN at the port of the site the tunnel leads to. A switch that
/// binds a device to one port (DHCP snooping) may then send a reply meant
/// for it to the wrong port; a broadcast reaches every port.
///
/// The rule sets the flags field and clears the UDP checksum: a raw change
/// of the payload keeps the old checksum, and zero means none (IPv4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DhcpRule {
    /// Trunk interface the requests enter through.
    pub port: String,
    /// The VLAN in the bridge (the source VLAN of the mapping).
    pub vid: u16,
}

/// Offset of the BOOTP flags in bits from the UDP header (8 bytes UDP header,
/// then op, htype, hlen, hops, xid, secs: 10 bytes).
const BOOTP_FLAGS_BITS: u32 = (8 + 10) * 8;
/// The broadcast flag (the other bits must be zero, RFC 2131).
const BOOTP_BROADCAST: u32 = 0x8000;

impl DhcpRule {
    /// The comment that identifies the rule.
    pub fn comment(&self) -> String {
        format!("wl dhcp-bcast iif {} vlan {}", self.port, self.vid)
    }

    /// Parses a comment produced by [`DhcpRule::comment`].
    pub fn from_comment(comment: &str) -> Option<Self> {
        let mut parts = comment.split_whitespace();
        if parts.next() != Some("wl") || parts.next() != Some("dhcp-bcast") {
            return None;
        }
        if parts.next() != Some("iif") {
            return None;
        }
        let port = parts.next()?.to_string();
        if parts.next() != Some("vlan") {
            return None;
        }
        let vid = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self { port, vid })
    }

    fn statements(&self) -> Vec<Statement<'static>> {
        let field = |protocol: &'static str, field: &'static str| {
            Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                PayloadField {
                    protocol: Cow::Borrowed(protocol),
                    field: Cow::Borrowed(field),
                },
            )))
        };
        let is = |left: Expression<'static>, right: Expression<'static>| {
            Statement::Match(Match {
                left,
                right,
                op: Operator::EQ,
            })
        };
        let meta = |key| Expression::Named(NamedExpression::Meta(Meta { key }));
        vec![
            is(
                meta(MetaKey::Iifname),
                Expression::String(Cow::Owned(self.port.clone())),
            ),
            is(field("vlan", "id"), Expression::Number(u32::from(self.vid))),
            is(
                meta(MetaKey::Protocol),
                Expression::String(Cow::Borrowed("ip")),
            ),
            is(field("udp", "dport"), Expression::Number(67)),
            Statement::Mangle(Mangle {
                key: Expression::Named(NamedExpression::Payload(Payload::PayloadRaw(PayloadRaw {
                    base: PayloadBase::TH,
                    offset: BOOTP_FLAGS_BITS,
                    len: 16,
                }))),
                value: Expression::Number(BOOTP_BROADCAST),
            }),
            Statement::Mangle(Mangle {
                key: field("udp", "checksum"),
                value: Expression::Number(0),
            }),
        ]
    }
}

impl fmt::Display for DhcpRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DHCP requests from {} in VLAN {} ask for broadcast replies",
            self.port, self.vid
        )
    }
}

/// The table as read back: the MSS rules, the comments of the split horizon
/// rules and the DHCP rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NftState {
    /// MSS clamping.
    pub mss: BTreeSet<MssRule>,
    /// [`SplitRule::comment`] of every split horizon rule.
    pub split: BTreeSet<String>,
    /// Broadcast DHCP replies.
    pub dhcp: BTreeSet<DhcpRule>,
}

impl NftState {
    /// The state the given rules produce.
    pub fn of(
        mss: &BTreeSet<MssRule>,
        split: &BTreeSet<SplitRule>,
        dhcp: &BTreeSet<DhcpRule>,
    ) -> Self {
        Self {
            mss: mss.clone(),
            split: split.iter().map(SplitRule::comment).collect(),
            dhcp: dhcp.clone(),
        }
    }

    /// True if nothing is in it.
    pub fn is_empty(&self) -> bool {
        self.mss.is_empty() && self.split.is_empty() && self.dhcp.is_empty()
    }
}

/// nftables operations.
pub trait NftOps: Send + Sync {
    /// The rules currently installed, or `None` if the table is absent.
    fn nft_state(&self) -> impl Future<Output = SysResult<Option<NftState>>> + Send;

    /// Replaces the table contents with the given rules in one transaction.
    fn set_nft_rules(
        &self,
        mss: &BTreeSet<MssRule>,
        split: &BTreeSet<SplitRule>,
        dhcp: &BTreeSet<DhcpRule>,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Deletes the table.
    fn delete_table(&self) -> impl Future<Output = SysResult<()>> + Send;
}

/// [`NftOps`] using the `nft` binary through its JSON interface.
#[derive(Debug, Clone, Default)]
pub struct NftCli;

fn table() -> Table<'static> {
    Table {
        family: NfFamily::Bridge,
        name: Cow::Borrowed(TABLE),
        ..Table::default()
    }
}

/// Builds the transaction that replaces the table contents.
pub fn replace_batch(
    rules: &BTreeSet<MssRule>,
    split: &BTreeSet<SplitRule>,
    dhcp: &BTreeSet<DhcpRule>,
) -> nftables::schema::Nftables<'static> {
    let mut batch = Batch::new();
    batch.add(NfListObject::Table(table()));
    batch.add_cmd(nftables::schema::NfCmd::Flush(
        nftables::schema::FlushObject::Table(table()),
    ));
    batch.add(NfListObject::Chain(Chain {
        family: NfFamily::Bridge,
        table: Cow::Borrowed(TABLE),
        name: Cow::Borrowed(CHAIN),
        _type: Some(NfChainType::Filter),
        hook: Some(NfHook::Forward),
        prio: Some(0),
        policy: Some(NfChainPolicy::Accept),
        ..Chain::default()
    }));
    batch.add(NfListObject::Chain(Chain {
        family: NfFamily::Bridge,
        table: Cow::Borrowed(TABLE),
        name: Cow::Borrowed(MSS_CHAIN),
        ..Chain::default()
    }));
    // The split horizon first: what is dropped needs no clamping.
    for rule in split {
        batch.add(NfListObject::Rule(Rule {
            family: NfFamily::Bridge,
            table: Cow::Borrowed(TABLE),
            chain: Cow::Borrowed(CHAIN),
            expr: Cow::Owned(rule.statements()),
            comment: Some(Cow::Owned(rule.comment())),
            ..Rule::default()
        }));
    }
    // DHCP requests ask for broadcast replies (a frame matches no verdict
    // here and goes on).
    for rule in dhcp {
        batch.add(NfListObject::Rule(Rule {
            family: NfFamily::Bridge,
            table: Cow::Borrowed(TABLE),
            chain: Cow::Borrowed(CHAIN),
            expr: Cow::Owned(rule.statements()),
            comment: Some(Cow::Owned(rule.comment())),
            ..Rule::default()
        }));
    }
    // Only TCP SYNs go on to the MSS rules (one rule per port, direction
    // and IP version); every other frame stops here.
    if !rules.is_empty() {
        batch.add(NfListObject::Rule(Rule {
            family: NfFamily::Bridge,
            table: Cow::Borrowed(TABLE),
            chain: Cow::Borrowed(CHAIN),
            expr: Cow::Owned(vec![
                syn(),
                Statement::Jump(JumpTarget {
                    target: Cow::Borrowed(MSS_CHAIN),
                }),
            ]),
            comment: Some(Cow::Borrowed(GATE)),
            ..Rule::default()
        }));
    }
    for rule in rules {
        batch.add(NfListObject::Rule(Rule {
            family: NfFamily::Bridge,
            table: Cow::Borrowed(TABLE),
            chain: Cow::Borrowed(MSS_CHAIN),
            expr: Cow::Owned(rule.statements()),
            comment: Some(Cow::Owned(rule.comment())),
            ..Rule::default()
        }));
    }
    batch.to_nftables()
}

/// `tcp flags syn`: a TCP SYN (also a SYN-ACK).
fn syn() -> Statement<'static> {
    Statement::Match(Match {
        left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
            PayloadField {
                protocol: Cow::Borrowed("tcp"),
                field: Cow::Borrowed("flags"),
            },
        ))),
        right: Expression::String(Cow::Borrowed("syn")),
        op: Operator::IN,
    })
}

fn nft_error(op: &str, e: impl std::error::Error + Send + Sync + 'static) -> SysError {
    SysError::new(op, e)
}

/// How long one `nft` call may take (a stuck lock would block everything).
const NFT_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs an `nft` call with [`NFT_TIMEOUT`].
async fn timed<T, E: std::error::Error + Send + Sync + 'static>(
    op: &str,
    call: impl Future<Output = Result<T, E>>,
) -> SysResult<T> {
    match tokio::time::timeout(NFT_TIMEOUT, call).await {
        Ok(result) => result.map_err(|e| nft_error(op, e)),
        Err(_) => Err(SysError::new(
            op,
            format!("nft did not answer within {} s", NFT_TIMEOUT.as_secs()),
        )),
    }
}

impl NftOps for NftCli {
    async fn nft_state(&self) -> SysResult<Option<NftState>> {
        let tables = timed(
            "list nftables tables",
            helper::get_current_ruleset_with_args_async(
                helper::DEFAULT_NFT,
                ["list", "tables", "bridge"],
            ),
        )
        .await?;
        let exists = tables
            .objects
            .iter()
            .any(|o| matches!(o, NfObject::ListObject(NfListObject::Table(t)) if t.name == TABLE));
        if !exists {
            return Ok(None);
        }
        let ruleset = timed(
            "list nftables table bridge wanderlan",
            helper::get_current_ruleset_with_args_async(
                helper::DEFAULT_NFT,
                ["list", "table", "bridge", TABLE],
            ),
        )
        .await?;
        let rules: Vec<(&str, &str)> = ruleset
            .objects
            .iter()
            .filter_map(|obj| match obj {
                NfObject::ListObject(NfListObject::Rule(rule)) => Some((
                    rule.chain.as_ref(),
                    rule.comment.as_deref().unwrap_or_default(),
                )),
                _ => None,
            })
            .collect();
        Ok(Some(parse_rules(&rules)))
    }

    async fn set_nft_rules(
        &self,
        rules: &BTreeSet<MssRule>,
        split: &BTreeSet<SplitRule>,
        dhcp: &BTreeSet<DhcpRule>,
    ) -> SysResult<()> {
        timed(
            "apply nftables table bridge wanderlan",
            helper::apply_ruleset_async(&replace_batch(rules, split, dhcp)),
        )
        .await
    }

    async fn delete_table(&self) -> SysResult<()> {
        let mut batch = Batch::new();
        batch.delete(NfListObject::Table(table()));
        timed(
            "delete nftables table bridge wanderlan",
            helper::apply_ruleset_async(&batch.to_nftables()),
        )
        .await
    }
}

/// The state from the rules of the table, as (chain, comment). Anything not
/// in the place this version puts it (an MSS rule in the base chain of an
/// older version, a missing gate, an unknown rule) yields a state that never
/// matches the desired one, so the table is written anew.
fn parse_rules(rules: &[(&str, &str)]) -> NftState {
    let mut mss = BTreeSet::new();
    let mut split = BTreeSet::new();
    let mut dhcp = BTreeSet::new();
    let mut gate = false;
    let mut misplaced = false;
    for &(chain, comment) in rules {
        match (chain, MssRule::from_comment(comment)) {
            (CHAIN, _) if comment.starts_with("wl split ") => {
                split.insert(comment.to_string());
            }
            (CHAIN, _) if DhcpRule::from_comment(comment).is_some() => {
                dhcp.extend(DhcpRule::from_comment(comment));
            }
            (CHAIN, _) if comment == GATE => gate = true,
            (MSS_CHAIN, Some(rule)) => {
                mss.insert(rule);
            }
            _ => misplaced = true,
        }
    }
    if misplaced || gate == mss.is_empty() {
        mss.insert(MssRule {
            port: String::from("?"),
            dir: PortDir::In,
            ipv6: false,
            mss: 0,
        });
    }
    NftState { mss, split, dhcp }
}

/// The rules as last read or written (`None`: no table), and when.
type CachedRules = (Instant, Option<NftState>);

/// [`NftOps`] that remembers the rules. Reading them runs `nft` twice, so a
/// read within `ttl` of the last read or write answers from memory; writes
/// always go through and update the memory. A table removed behind
/// wanderlan's back (e.g. a firewall reload) is noticed within `ttl`; while
/// it holds split horizon rules (which prevent loops), on every read.
#[derive(Debug, Clone)]
pub struct CachedNft<N> {
    inner: N,
    ttl: Duration,
    cache: Arc<Mutex<Option<CachedRules>>>,
}

impl<N> CachedNft<N> {
    /// Wraps `inner`.
    pub fn new(inner: N, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    fn cached(&self) -> Option<Option<NftState>> {
        let guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .filter(|(at, state)| {
                at.elapsed() < self.ttl && state.as_ref().is_none_or(|s| s.split.is_empty())
            })
            .map(|(_, rules)| rules.clone())
    }

    fn store(&self, rules: Option<Option<NftState>>) {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = rules.map(|r| (Instant::now(), r));
    }
}

impl<N: NftOps + Sync> NftOps for CachedNft<N> {
    async fn nft_state(&self) -> SysResult<Option<NftState>> {
        if let Some(rules) = self.cached() {
            return Ok(rules);
        }
        let rules = self.inner.nft_state().await?;
        self.store(Some(rules.clone()));
        Ok(rules)
    }

    async fn set_nft_rules(
        &self,
        rules: &BTreeSet<MssRule>,
        split: &BTreeSet<SplitRule>,
        dhcp: &BTreeSet<DhcpRule>,
    ) -> SysResult<()> {
        let result = self.inner.set_nft_rules(rules, split, dhcp).await;
        self.store(
            result
                .is_ok()
                .then(|| Some(NftState::of(rules, split, dhcp))),
        );
        result
    }

    async fn delete_table(&self) -> SysResult<()> {
        let result = self.inner.delete_table().await;
        self.store(result.is_ok().then_some(None));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_roundtrip() {
        let rule = MssRule {
            port: "vx-hub2".into(),
            dir: PortDir::Out,
            ipv6: true,
            mss: 1298,
        };
        assert_eq!(rule.comment(), "wl mss oif vx-hub2 ip6 1298");
        assert_eq!(MssRule::from_comment(&rule.comment()), Some(rule));
        assert_eq!(MssRule::from_comment("something else"), None);
        assert_eq!(MssRule::from_comment("wl mss oif vx-x ip 1 extra"), None);
    }

    #[test]
    fn batch_json_contains_the_expected_rule() {
        let rules = BTreeSet::from([MssRule {
            port: "vx-hub2".into(),
            dir: PortDir::Out,
            ipv6: false,
            mss: 1318,
        }]);
        let json =
            serde_json::to_string(&replace_batch(&rules, &BTreeSet::new(), &BTreeSet::new()))
                .unwrap();
        for needle in [
            r#""family":"bridge""#,
            r#""flush":{"table""#,
            r#""hook":"forward""#,
            r#""tcp option":{"name":"maxseg","field":"size"}"#,
            r#"{"match":{"left":{"tcp option":{"name":"maxseg","field":"size"}},"right":1318,"op":">"}}"#,
            r#""mangle""#,
            r#""comment":"wl mss oif vx-hub2 ip 1318""#,
            r#""oifname""#,
            // Only TCP SYNs reach the MSS rules.
            r#""chain":"wl-mss""#,
            r#"{"jump":{"target":"wl-mss"}}"#,
            r#""comment":"wl mss gate""#,
        ] {
            assert!(json.contains(needle), "{needle} missing in {json}");
        }
        // No gate without MSS rules.
        let json = serde_json::to_string(&replace_batch(
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        ))
        .unwrap();
        assert!(!json.contains("wl mss gate"), "{json}");
    }

    #[test]
    fn the_table_of_an_older_version_is_written_anew() {
        let rule = MssRule {
            port: "vx-a".into(),
            dir: PortDir::In,
            ipv6: false,
            mss: 1318,
        };
        let comment = rule.comment();
        let want = NftState::of(&BTreeSet::from([rule]), &BTreeSet::new(), &BTreeSet::new());
        // This version: the gate in the base chain, the rule behind it.
        let now = parse_rules(&[(CHAIN, GATE), (MSS_CHAIN, &comment)]);
        assert_eq!(now, want);
        // An older version: the MSS rule in the base chain, no gate.
        assert_ne!(parse_rules(&[(CHAIN, &comment)]), want);
        // A gate without rules, a rule without the gate.
        assert_ne!(parse_rules(&[(MSS_CHAIN, &comment)]), want);
        assert!(!parse_rules(&[(CHAIN, GATE)]).is_empty());
        // Nothing: the empty state.
        assert!(parse_rules(&[]).is_empty());
    }

    /// Counts the reads of the rules.
    #[derive(Default)]
    struct Counting {
        reads: std::sync::atomic::AtomicUsize,
    }

    impl NftOps for Counting {
        async fn nft_state(&self) -> SysResult<Option<NftState>> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(None)
        }
        async fn set_nft_rules(
            &self,
            _: &BTreeSet<MssRule>,
            _: &BTreeSet<SplitRule>,
            _: &BTreeSet<DhcpRule>,
        ) -> SysResult<()> {
            Ok(())
        }
        async fn delete_table(&self) -> SysResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_rules_are_read_from_nft_at_most_once_per_ttl() {
        let cached = CachedNft::new(Counting::default(), Duration::from_secs(60));
        assert_eq!(cached.nft_state().await.unwrap(), None);
        assert_eq!(cached.nft_state().await.unwrap(), None);
        assert_eq!(
            cached
                .inner
                .reads
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        // A write is remembered.
        let rules = BTreeSet::from([MssRule {
            port: "vx-a".into(),
            dir: PortDir::In,
            ipv6: false,
            mss: 1318,
        }]);
        cached
            .set_nft_rules(&rules, &BTreeSet::new(), &BTreeSet::new())
            .await
            .unwrap();
        assert_eq!(
            cached.nft_state().await.unwrap(),
            Some(NftState::of(&rules, &BTreeSet::new(), &BTreeSet::new()))
        );
        cached.delete_table().await.unwrap();
        assert_eq!(cached.nft_state().await.unwrap(), None);
        assert_eq!(
            cached
                .inner
                .reads
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        // After the ttl it reads again.
        let short = CachedNft::new(Counting::default(), Duration::ZERO);
        short.nft_state().await.unwrap();
        short.nft_state().await.unwrap();
        assert_eq!(
            short.inner.reads.load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    fn split() -> SplitRule {
        SplitRule {
            ports: BTreeSet::from(["vx-a".to_string(), "vx-b".to_string()]),
            vlans: vec![(10, 19), (21, 21)],
        }
    }

    #[test]
    fn split_horizon_rules_drop_between_their_ports() {
        let json = serde_json::to_string(&replace_batch(
            &BTreeSet::new(),
            &BTreeSet::from([split()]),
            &BTreeSet::new(),
        ))
        .unwrap();
        for needle in [
            r#"{"match":{"left":{"meta":{"key":"iifname"}},"right":{"set":["vx-a","vx-b"]},"op":"=="}}"#,
            r#"{"match":{"left":{"meta":{"key":"oifname"}},"right":{"set":["vx-a","vx-b"]},"op":"=="}}"#,
            r#"{"match":{"left":{"payload":{"protocol":"vlan","field":"id"}},"right":{"set":[{"range":[10,19]},21]},"op":"=="}}"#,
            r#"{"drop":null}"#,
        ] {
            assert!(json.contains(needle), "{needle} missing in {json}");
        }
        let comment = split().comment();
        assert!(
            comment.starts_with("wl split ") && comment.len() == 25,
            "{comment}"
        );
        assert_ne!(
            comment,
            SplitRule {
                vlans: vec![(10, 19)],
                ..split()
            }
            .comment()
        );
    }

    #[tokio::test]
    async fn split_horizon_rules_are_read_back_every_time() {
        let cached = CachedNft::new(Counting::default(), Duration::from_secs(60));
        cached
            .set_nft_rules(
                &BTreeSet::new(),
                &BTreeSet::from([split()]),
                &BTreeSet::new(),
            )
            .await
            .unwrap();
        // The write is remembered, but with split horizon rules the next
        // read still asks nft: a removed table must be noticed at once.
        cached.nft_state().await.unwrap();
        assert_eq!(
            cached
                .inner
                .reads
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn dhcp_rules_set_the_broadcast_flag_and_are_read_back() {
        let rule = DhcpRule {
            port: "ens19".into(),
            vid: 14,
        };
        assert_eq!(rule.comment(), "wl dhcp-bcast iif ens19 vlan 14");
        assert_eq!(DhcpRule::from_comment(&rule.comment()), Some(rule.clone()));
        assert_eq!(
            DhcpRule::from_comment("wl dhcp-bcast iif ens19 vlan x"),
            None
        );
        let (mss, split) = (BTreeSet::new(), BTreeSet::new());
        let dhcp = BTreeSet::from([rule.clone()]);
        let json = serde_json::to_string(&replace_batch(&mss, &split, &dhcp)).unwrap();
        for needle in [
            r#"{"match":{"left":{"meta":{"key":"iifname"}},"right":"ens19","op":"=="}}"#,
            r#"{"match":{"left":{"payload":{"protocol":"vlan","field":"id"}},"right":14,"op":"=="}}"#,
            r#"{"match":{"left":{"payload":{"protocol":"udp","field":"dport"}},"right":67,"op":"=="}}"#,
            r#"{"mangle":{"key":{"payload":{"base":"th","offset":144,"len":16}},"value":32768}}"#,
            r#"{"mangle":{"key":{"payload":{"protocol":"udp","field":"checksum"}},"value":0}}"#,
            r#""comment":"wl dhcp-bcast iif ens19 vlan 14""#,
        ] {
            assert!(json.contains(needle), "{needle} missing in {json}");
        }
        // Read back from the base chain; anywhere else the table is written
        // anew.
        let comment = rule.comment();
        assert_eq!(
            parse_rules(&[(CHAIN, &comment)]),
            NftState::of(&mss, &split, &dhcp)
        );
        assert_ne!(
            parse_rules(&[(MSS_CHAIN, &comment)]),
            NftState::of(&mss, &split, &dhcp)
        );
    }
}
