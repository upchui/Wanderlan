//! VLAN translation rules on the trunk (tc clsact + flower).
//!
//! Every rule matches one VLAN ID with `flower vlan_id` and either rewrites
//! it with `action vlan modify` or drops the frame with `action gact drop`.
//! wanderlan owns the filters it created; they are recognised by an action
//! cookie that encodes the rule (e.g. `wl:i130>30`), so no state file is
//! needed and foreign filters are never touched.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use super::SysResult;

/// First tc priority used by wanderlan; a rule uses `TC_PRIO_BASE + vid`.
pub const TC_PRIO_BASE: u16 = 0xC000;
/// Prefix of the action cookies of wanderlan's rules.
pub const COOKIE_PREFIX: &str = "wl:";

/// Direction of a tc rule on the trunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// Frames received on the trunk (local → shared).
    Ingress,
    /// Frames sent on the trunk (shared → local).
    Egress,
}

/// What a rule does with a matching frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TcAction {
    /// Rewrite the VLAN ID.
    Translate(u16),
    /// Drop the frame.
    Drop,
}

/// One VLAN rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TcRule {
    /// Ingress or egress.
    pub direction: Direction,
    /// VLAN ID to match.
    pub vid: u16,
    /// Action.
    pub action: TcAction,
}

impl TcRule {
    /// tc priority of the rule (unique per direction).
    pub fn prio(&self) -> u16 {
        TC_PRIO_BASE.saturating_add(self.vid)
    }

    /// Action cookie identifying the rule.
    pub fn cookie(&self) -> String {
        let dir = match self.direction {
            Direction::Ingress => 'i',
            Direction::Egress => 'e',
        };
        match self.action {
            TcAction::Translate(to) => format!("{COOKIE_PREFIX}{dir}{}>{to}", self.vid),
            TcAction::Drop => format!("{COOKIE_PREFIX}{dir}{}x", self.vid),
        }
    }

    /// Parses a cookie produced by [`TcRule::cookie`].
    pub fn from_cookie(cookie: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(cookie)
            .ok()?
            .strip_prefix(COOKIE_PREFIX)?;
        let mut chars = text.chars();
        let direction = match chars.next()? {
            'i' => Direction::Ingress,
            'e' => Direction::Egress,
            _ => return None,
        };
        let rest = chars.as_str();
        if let Some(vid) = rest.strip_suffix('x') {
            return Some(Self {
                direction,
                vid: vid.parse().ok()?,
                action: TcAction::Drop,
            });
        }
        let (vid, to) = rest.split_once('>')?;
        Some(Self {
            direction,
            vid: vid.parse().ok()?,
            action: TcAction::Translate(to.parse().ok()?),
        })
    }
}

impl fmt::Display for TcRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.direction, self.action) {
            (Direction::Ingress, TcAction::Translate(to)) => write!(
                f,
                "translate local VLAN {} to shared VLAN {to} (incoming)",
                self.vid
            ),
            (Direction::Egress, TcAction::Translate(to)) => write!(
                f,
                "translate shared VLAN {} back to local VLAN {to} (outgoing)",
                self.vid
            ),
            (Direction::Ingress, TcAction::Drop) => write!(
                f,
                "drop incoming frames tagged with VLAN {} (here shared VLAN {} is reached through a translated local VLAN)",
                self.vid, self.vid
            ),
            (Direction::Egress, TcAction::Drop) => {
                write!(f, "drop outgoing frames tagged with VLAN {}", self.vid)
            }
        }
    }
}

/// tc state of one interface as far as wanderlan cares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IfTc {
    /// The interface has a clsact qdisc.
    pub clsact: bool,
    /// wanderlan's rules, recognised by their cookie.
    pub ours: Vec<TcRule>,
    /// Number of other filters on the clsact qdisc.
    pub foreign: usize,
}

/// tc operations needed for VLAN translation.
pub trait TcOps: Send + Sync {
    /// tc state of all interfaces that have a clsact qdisc, by index.
    fn tc_state(&self) -> impl Future<Output = SysResult<BTreeMap<u32, IfTc>>> + Send;

    /// Adds a clsact qdisc.
    fn add_clsact(&self, index: u32) -> impl Future<Output = SysResult<()>> + Send;

    /// Removes the clsact qdisc (and with it all its filters).
    fn del_clsact(&self, index: u32) -> impl Future<Output = SysResult<()>> + Send;

    /// Adds a rule.
    fn add_tc_rule(&self, index: u32, rule: &TcRule) -> impl Future<Output = SysResult<()>> + Send;

    /// Removes a rule.
    fn del_tc_rule(&self, index: u32, rule: &TcRule) -> impl Future<Output = SysResult<()>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_roundtrip_and_fit_the_kernel_limit() {
        let rules = [
            TcRule {
                direction: Direction::Ingress,
                vid: 4094,
                action: TcAction::Translate(4093),
            },
            TcRule {
                direction: Direction::Egress,
                vid: 30,
                action: TcAction::Translate(130),
            },
            TcRule {
                direction: Direction::Ingress,
                vid: 30,
                action: TcAction::Drop,
            },
        ];
        for rule in rules {
            let cookie = rule.cookie();
            assert!(
                cookie.len() <= 16,
                "{cookie} is longer than TC_COOKIE_MAX_SIZE"
            );
            assert_eq!(TcRule::from_cookie(cookie.as_bytes()), Some(rule));
        }
        assert_eq!(TcRule::from_cookie(b"other"), None);
        assert_eq!(TcRule::from_cookie(b"wl:q1>2"), None);
        assert_eq!(rules[0].prio(), 0xC000 + 4094);
    }
}
