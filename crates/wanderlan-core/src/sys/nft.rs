//! TCP MSS clamping with nftables (table `bridge wanderlan`).
//!
//! The rules are applied through the JSON interface of nftables (`nft -j`),
//! using the `nftables` crate. Every rule carries a comment that encodes it,
//! e.g. `wl mss oif gt-hub2 ip 1318`, so the current state can be read back
//! and compared without parsing expressions.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;

use nftables::batch::Batch;
use nftables::expr::{
    Expression, Meta, MetaKey, NamedExpression, Payload, PayloadField, TcpOption,
};
use nftables::helper;
use nftables::schema::{Chain, NfListObject, NfObject, Rule, Table};
use nftables::stmt::{Mangle, Match, Operator, Statement};
use nftables::types::{NfChainPolicy, NfChainType, NfFamily, NfHook};

use super::{SysError, SysResult};

/// The `>` operator. nftables 0.6.3 serialises `Operator::LT` as `">"` and
/// `Operator::GT` as `"<"` (the names are swapped in the crate); the test
/// `batch_json_contains_the_expected_rule` checks the JSON so that a fix in
/// the crate is noticed.
const GREATER_THAN: Operator = Operator::LT;

/// Name of the nftables table (family bridge).
pub const TABLE: &str = "wanderlan";
/// Name of the chain in that table.
pub const CHAIN: &str = "forward";

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
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                    PayloadField {
                        protocol: Cow::Borrowed("tcp"),
                        field: Cow::Borrowed("flags"),
                    },
                ))),
                right: Expression::String(Cow::Borrowed("syn")),
                op: Operator::IN,
            }),
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

/// nftables operations.
pub trait NftOps: Send + Sync {
    /// The MSS rules currently installed, or `None` if the table is absent.
    fn mss_rules(&self) -> impl Future<Output = SysResult<Option<BTreeSet<MssRule>>>> + Send;

    /// Replaces the table contents with the given rules in one transaction.
    fn set_mss_rules(
        &self,
        rules: &BTreeSet<MssRule>,
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
pub fn replace_batch(rules: &BTreeSet<MssRule>) -> nftables::schema::Nftables<'static> {
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
    for rule in rules {
        batch.add(NfListObject::Rule(Rule {
            family: NfFamily::Bridge,
            table: Cow::Borrowed(TABLE),
            chain: Cow::Borrowed(CHAIN),
            expr: Cow::Owned(rule.statements()),
            comment: Some(Cow::Owned(rule.comment())),
            ..Rule::default()
        }));
    }
    batch.to_nftables()
}

fn nft_error(op: &str, e: impl std::error::Error + Send + Sync + 'static) -> SysError {
    SysError::new(op, e)
}

impl NftOps for NftCli {
    async fn mss_rules(&self) -> SysResult<Option<BTreeSet<MssRule>>> {
        let tables = helper::get_current_ruleset_with_args_async(
            helper::DEFAULT_NFT,
            ["list", "tables", "bridge"],
        )
        .await
        .map_err(|e| nft_error("list nftables tables", e))?;
        let exists = tables
            .objects
            .iter()
            .any(|o| matches!(o, NfObject::ListObject(NfListObject::Table(t)) if t.name == TABLE));
        if !exists {
            return Ok(None);
        }
        let ruleset = helper::get_current_ruleset_with_args_async(
            helper::DEFAULT_NFT,
            ["list", "table", "bridge", TABLE],
        )
        .await
        .map_err(|e| nft_error("list nftables table bridge wanderlan", e))?;
        let mut rules = BTreeSet::new();
        for obj in ruleset.objects.iter() {
            if let NfObject::ListObject(NfListObject::Rule(rule)) = obj {
                match rule.comment.as_deref().and_then(MssRule::from_comment) {
                    Some(r) => {
                        rules.insert(r);
                    }
                    // An unknown rule in our table: report a state that never
                    // matches the desired one, so the table gets rewritten.
                    None => {
                        rules.insert(MssRule {
                            port: String::from("?"),
                            dir: PortDir::In,
                            ipv6: false,
                            mss: 0,
                        });
                    }
                }
            }
        }
        Ok(Some(rules))
    }

    async fn set_mss_rules(&self, rules: &BTreeSet<MssRule>) -> SysResult<()> {
        helper::apply_ruleset_async(&replace_batch(rules))
            .await
            .map_err(|e| nft_error("apply nftables table bridge wanderlan", e))
    }

    async fn delete_table(&self) -> SysResult<()> {
        let mut batch = Batch::new();
        batch.delete(NfListObject::Table(table()));
        helper::apply_ruleset_async(&batch.to_nftables())
            .await
            .map_err(|e| nft_error("delete nftables table bridge wanderlan", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_roundtrip() {
        let rule = MssRule {
            port: "gt-hub2".into(),
            dir: PortDir::Out,
            ipv6: true,
            mss: 1298,
        };
        assert_eq!(rule.comment(), "wl mss oif gt-hub2 ip6 1298");
        assert_eq!(MssRule::from_comment(&rule.comment()), Some(rule));
        assert_eq!(MssRule::from_comment("something else"), None);
        assert_eq!(MssRule::from_comment("wl mss oif gt-x ip 1 extra"), None);
    }

    #[test]
    fn batch_json_contains_the_expected_rule() {
        let rules = BTreeSet::from([MssRule {
            port: "gt-hub2".into(),
            dir: PortDir::Out,
            ipv6: false,
            mss: 1318,
        }]);
        let json = serde_json::to_string(&replace_batch(&rules)).unwrap();
        for needle in [
            r#""family":"bridge""#,
            r#""flush":{"table""#,
            r#""hook":"forward""#,
            r#""tcp option":{"name":"maxseg","field":"size"}"#,
            r#"{"match":{"left":{"tcp option":{"name":"maxseg","field":"size"}},"right":1318,"op":">"}}"#,
            r#""mangle""#,
            r#""comment":"wl mss oif gt-hub2 ip 1318""#,
            r#""oifname""#,
        ] {
            assert!(json.contains(needle), "{needle} missing in {json}");
        }
    }
}
