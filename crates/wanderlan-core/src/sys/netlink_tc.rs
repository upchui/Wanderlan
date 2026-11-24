//! [`TcOps`] implemented with raw rtnetlink tc messages.
//!
//! netlink-packet-route models flower filters but not the `vlan` and `gact`
//! actions; their parameters are built as raw attributes here.

use std::collections::BTreeMap;

use futures_util::StreamExt as _;
use rtnetlink::packet_core::{
    DefaultNla, NLM_F_ACK, NLM_F_CREATE, NLM_F_DUMP, NLM_F_EXCL, NLM_F_REQUEST, NetlinkMessage,
    NetlinkPayload,
};
use rtnetlink::packet_route::RouteNetlinkMessage;
use rtnetlink::packet_route::tc::{
    TcAction as NlAction, TcActionAttribute, TcActionOption, TcAttribute, TcFilterFlowerOption,
    TcHandle, TcMessage, TcOption,
};

use super::netlink::Netlink;
use super::tc::{Direction, IfTc, TcAction, TcOps, TcRule};
use super::{SysError, SysResult};

/// `TC_H_CLSACT`: parent of the clsact qdisc.
const TC_H_CLSACT: u32 = 0xFFFF_FFF1;
/// Handle of the clsact qdisc (`ffff:`).
const CLSACT_HANDLE: u32 = 0xFFFF_0000;
/// Parent of ingress filters on clsact (`ffff:fff2`).
const PARENT_INGRESS: u32 = 0xFFFF_FFF2;
/// Parent of egress filters on clsact (`ffff:fff3`).
const PARENT_EGRESS: u32 = 0xFFFF_FFF3;
const ETH_P_8021Q: u16 = 0x8100;
const TCA_VLAN_PARMS: u16 = 2;
const TCA_VLAN_PUSH_VLAN_ID: u16 = 3;
const TCA_VLAN_ACT_MODIFY: i32 = 3;
const TCA_GACT_PARMS: u16 = 2;
const TC_ACT_SHOT: i32 = 2;
const TC_ACT_PIPE: i32 = 3;

fn parent(direction: Direction) -> u32 {
    match direction {
        Direction::Ingress => PARENT_INGRESS,
        Direction::Egress => PARENT_EGRESS,
    }
}

fn info(prio: u16) -> u32 {
    (u32::from(prio) << 16) | u32::from(ETH_P_8021Q.to_be())
}

/// `struct tc_gen` followed by optional extra fields, native endian.
fn tc_gen(action: i32, extra: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + 4 * extra.len());
    out.extend_from_slice(&0u32.to_ne_bytes()); // index
    out.extend_from_slice(&0u32.to_ne_bytes()); // capab
    out.extend_from_slice(&action.to_ne_bytes()); // action
    out.extend_from_slice(&0i32.to_ne_bytes()); // refcnt
    out.extend_from_slice(&0i32.to_ne_bytes()); // bindcnt
    for v in extra {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

fn action_for(rule: &TcRule) -> NlAction {
    let (kind, options) = match rule.action {
        TcAction::Translate(to) => (
            "vlan",
            vec![
                TcActionOption::Other(DefaultNla::new(
                    TCA_VLAN_PARMS,
                    tc_gen(TC_ACT_PIPE, &[TCA_VLAN_ACT_MODIFY]),
                )),
                TcActionOption::Other(DefaultNla::new(
                    TCA_VLAN_PUSH_VLAN_ID,
                    to.to_ne_bytes().to_vec(),
                )),
            ],
        ),
        TcAction::Drop => (
            "gact",
            vec![TcActionOption::Other(DefaultNla::new(
                TCA_GACT_PARMS,
                tc_gen(TC_ACT_SHOT, &[]),
            ))],
        ),
    };
    let mut action = NlAction::default();
    action.tab = 1;
    action.attributes = vec![
        TcActionAttribute::Kind(kind.to_string()),
        TcActionAttribute::Options(options),
        TcActionAttribute::Cookie(rule.cookie().into_bytes()),
    ];
    action
}

fn header(index: u32, handle: u32, parent: u32, info: u32) -> SysResult<TcMessage> {
    let index = i32::try_from(index).map_err(|e| SysError::new("tc request", e))?;
    let mut msg = TcMessage::with_index(index);
    msg.header.handle = TcHandle::from(handle);
    msg.header.parent = TcHandle::from(parent);
    msg.header.info = info;
    Ok(msg)
}

impl Netlink {
    /// Sends a request and collects the replies, turning netlink errors into
    /// [`SysError`]s.
    async fn tc_request(
        &self,
        op: &str,
        msg: RouteNetlinkMessage,
        flags: u16,
    ) -> SysResult<Vec<TcMessage>> {
        let mut req = NetlinkMessage::from(msg);
        req.header.flags = NLM_F_REQUEST | flags;
        let mut handle = self.handle().clone();
        let mut responses = handle.request(req).map_err(|e| SysError::new(op, e))?;
        let mut out = Vec::new();
        while let Some(resp) = responses.next().await {
            match resp.payload {
                NetlinkPayload::Error(err) if err.code.is_some() => {
                    return Err(SysError::new(op, rtnetlink::Error::NetlinkError(err)));
                }
                NetlinkPayload::InnerMessage(
                    RouteNetlinkMessage::NewQueueDiscipline(m)
                    | RouteNetlinkMessage::NewTrafficFilter(m),
                ) => out.push(m),
                _ => {}
            }
        }
        Ok(out)
    }

    async fn dump_filters(&self, index: u32, direction: Direction) -> SysResult<Vec<TcMessage>> {
        let msg = header(index, 0, parent(direction), 0)?;
        self.tc_request(
            "list tc filters",
            RouteNetlinkMessage::GetTrafficFilter(msg),
            NLM_F_DUMP,
        )
        .await
    }
}

fn kind(msg: &TcMessage) -> Option<&str> {
    msg.attributes.iter().find_map(|a| match a {
        TcAttribute::Kind(k) => Some(k.as_str()),
        _ => None,
    })
}

/// Finds the cookie of the first action of a flower filter.
fn cookie(msg: &TcMessage) -> Option<&[u8]> {
    msg.attributes.iter().find_map(|a| match a {
        TcAttribute::Options(opts) => opts.iter().find_map(|o| match o {
            TcOption::Flower(TcFilterFlowerOption::Actions(actions)) => {
                actions.iter().find_map(|act| {
                    act.attributes.iter().find_map(|aa| match aa {
                        TcActionAttribute::Cookie(c) => Some(c.as_slice()),
                        _ => None,
                    })
                })
            }
            _ => None,
        }),
        _ => None,
    })
}

impl TcOps for Netlink {
    async fn tc_state(&self) -> SysResult<BTreeMap<u32, IfTc>> {
        let qdiscs = self
            .tc_request(
                "list qdiscs",
                RouteNetlinkMessage::GetQueueDiscipline(TcMessage::default()),
                NLM_F_DUMP,
            )
            .await?;
        let mut out = BTreeMap::new();
        for q in qdiscs.iter().filter(|q| kind(q) == Some("clsact")) {
            let Ok(index) = u32::try_from(q.header.index) else {
                continue;
            };
            let mut state = IfTc {
                clsact: true,
                ..IfTc::default()
            };
            for direction in [Direction::Ingress, Direction::Egress] {
                let mut seen_prios = Vec::new();
                for f in self.dump_filters(index, direction).await? {
                    // Each filter is reported twice: once for its priority
                    // (handle 0, no options) and once with its handle.
                    if kind(&f).is_none() || f.header.handle == TcHandle::UNSPEC {
                        continue;
                    }
                    let prio = (f.header.info >> 16) as u16;
                    if seen_prios.contains(&prio) {
                        continue;
                    }
                    seen_prios.push(prio);
                    match cookie(&f).and_then(TcRule::from_cookie) {
                        Some(rule) if rule.direction == direction && rule.prio() == prio => {
                            state.ours.push(rule);
                        }
                        _ => state.foreign += 1,
                    }
                }
            }
            out.insert(index, state);
        }
        Ok(out)
    }

    async fn add_clsact(&self, index: u32) -> SysResult<()> {
        let mut msg = header(index, CLSACT_HANDLE, TC_H_CLSACT, 0)?;
        msg.attributes.push(TcAttribute::Kind("clsact".into()));
        self.tc_request(
            "add clsact qdisc",
            RouteNetlinkMessage::NewQueueDiscipline(msg),
            NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        )
        .await
        .map(drop)
    }

    async fn del_clsact(&self, index: u32) -> SysResult<()> {
        let mut msg = header(index, CLSACT_HANDLE, TC_H_CLSACT, 0)?;
        msg.attributes.push(TcAttribute::Kind("clsact".into()));
        self.tc_request(
            "remove clsact qdisc",
            RouteNetlinkMessage::DelQueueDiscipline(msg),
            NLM_F_ACK,
        )
        .await
        .map(drop)
    }

    async fn add_tc_rule(&self, index: u32, rule: &TcRule) -> SysResult<()> {
        let mut msg = header(index, 0, parent(rule.direction), info(rule.prio()))?;
        msg.attributes.push(TcAttribute::Kind("flower".into()));
        msg.attributes.push(TcAttribute::Options(vec![
            TcOption::Flower(TcFilterFlowerOption::EthType(ETH_P_8021Q)),
            TcOption::Flower(TcFilterFlowerOption::VlanId(rule.vid)),
            TcOption::Flower(TcFilterFlowerOption::Actions(vec![action_for(rule)])),
        ]));
        self.tc_request(
            &format!("add tc rule ({rule})"),
            RouteNetlinkMessage::NewTrafficFilter(msg),
            NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        )
        .await
        .map(drop)
    }

    async fn del_tc_rule(&self, index: u32, rule: &TcRule) -> SysResult<()> {
        let msg = header(index, 0, parent(rule.direction), info(rule.prio()))?;
        self.tc_request(
            &format!("remove tc rule ({rule})"),
            RouteNetlinkMessage::DelTrafficFilter(msg),
            NLM_F_ACK,
        )
        .await
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_gen_layout() {
        assert_eq!(tc_gen(TC_ACT_SHOT, &[]).len(), 20);
        let vlan = tc_gen(TC_ACT_PIPE, &[TCA_VLAN_ACT_MODIFY]);
        assert_eq!(vlan.len(), 24);
        assert_eq!(&vlan[8..12], &TC_ACT_PIPE.to_ne_bytes());
        assert_eq!(&vlan[20..24], &TCA_VLAN_ACT_MODIFY.to_ne_bytes());
    }

    #[test]
    fn filter_info_has_prio_and_protocol() {
        let i = info(0xC000 + 30);
        assert_eq!(i >> 16, 0xC000 + 30);
        assert_eq!((i & 0xFFFF) as u16, ETH_P_8021Q.to_be());
    }
}
