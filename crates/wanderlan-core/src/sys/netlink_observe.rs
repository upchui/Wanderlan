//! [`ObserveOps`] with rtnetlink (RTM_GETNEIGH dumps and RTNLGRP_NEIGH).

use futures_util::StreamExt as _;
use rtnetlink::packet_core::{NLM_F_DUMP, NLM_F_REQUEST, NetlinkMessage, NetlinkPayload};
use rtnetlink::packet_route::neighbour::{
    NeighbourAddress, NeighbourAttribute, NeighbourFlags, NeighbourMessage, NeighbourState,
};
use rtnetlink::packet_route::{AddressFamily, RouteNetlinkMessage};
use rtnetlink::{MulticastGroup, new_multicast_connection};
use tokio::sync::mpsc;

use super::linux::LinuxSystem;
use super::netlink::Netlink;
use super::observe::{FdbEntry, FdbEvent, ObserveOps};
use super::{SysError, SysResult};

/// Kernel ticks per second of `NDA_CACHEINFO` (USER_HZ).
const USER_HZ: u64 = 100;

/// Parses a bridge FDB message; `None` for other neighbour messages.
fn parse(msg: &NeighbourMessage) -> Option<FdbEntry> {
    if msg.header.family != AddressFamily::Bridge {
        return None;
    }
    let mut mac = None;
    let mut vlan = None;
    let mut bridge = None;
    let mut updated_secs = None;
    for attr in &msg.attributes {
        match attr {
            NeighbourAttribute::LinkLayerAddress(addr) => {
                mac = <[u8; 6]>::try_from(addr.as_slice()).ok();
            }
            NeighbourAttribute::Destination(NeighbourAddress::Other(addr)) if mac.is_none() => {
                mac = <[u8; 6]>::try_from(addr.as_slice()).ok();
            }
            NeighbourAttribute::Vlan(v) => vlan = Some(*v),
            NeighbourAttribute::Controller(m) => bridge = Some(*m),
            NeighbourAttribute::CacheInfo(ci) => {
                updated_secs = Some(u64::from(ci.updated) / USER_HZ);
            }
            _ => {}
        }
    }
    // Entries of the port device itself (NTF_SELF) are not bridge entries.
    if bridge.is_none() && !msg.header.flags.contains(NeighbourFlags::Controller) {
        return None;
    }
    Some(FdbEntry {
        mac: mac?,
        vlan,
        port: msg.header.ifindex,
        bridge,
        permanent: matches!(
            msg.header.state,
            NeighbourState::Permanent | NeighbourState::Noarp
        ),
        updated_secs,
    })
}

impl ObserveOps for Netlink {
    async fn fdb(&self) -> SysResult<Vec<FdbEntry>> {
        let _dump = self.dump_guard().await;
        let mut msg = NeighbourMessage::default();
        msg.header.family = AddressFamily::Bridge;
        let mut req = NetlinkMessage::from(RouteNetlinkMessage::GetNeighbour(msg));
        req.header.flags = NLM_F_REQUEST | NLM_F_DUMP;
        let mut handle = self.handle().clone();
        let mut responses = handle
            .request(req)
            .map_err(|e| SysError::new("list bridge FDB", e))?;
        let mut out = Vec::new();
        while let Some(resp) = responses.next().await {
            match resp.payload {
                NetlinkPayload::Error(err) if err.code.is_some() => {
                    return Err(SysError::new(
                        "list bridge FDB",
                        rtnetlink::Error::NetlinkError(err),
                    ));
                }
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(m)) => {
                    out.extend(parse(&m));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn watch_fdb(&self) -> SysResult<mpsc::UnboundedReceiver<FdbEvent>> {
        let (connection, handle, mut messages) = new_multicast_connection(&[MulticastGroup::Neigh])
            .map_err(|e| SysError::new("watch bridge FDB", e))?;
        let (tx, rx) = mpsc::unbounded_channel();
        // The connection runs in the same task as the forwarding, so both end
        // together once the receiver is dropped.
        tokio::spawn(async move {
            let _handle = handle;
            let forward = async {
                while let Some((msg, _)) = messages.next().await {
                    let event = match msg.payload {
                        NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(m)) => {
                            parse(&m).map(FdbEvent::Learned)
                        }
                        NetlinkPayload::InnerMessage(RouteNetlinkMessage::DelNeighbour(m)) => {
                            parse(&m).map(|e| FdbEvent::Removed {
                                mac: e.mac,
                                vlan: e.vlan,
                            })
                        }
                        _ => None,
                    };
                    if let Some(event) = event
                        && tx.send(event).is_err()
                    {
                        break;
                    }
                }
            };
            tokio::select! {
                () = connection => {}
                () = forward => {}
            }
        });
        Ok(rx)
    }
}

impl ObserveOps for LinuxSystem {
    async fn fdb(&self) -> SysResult<Vec<FdbEntry>> {
        self.net.fdb().await
    }

    fn watch_fdb(&self) -> SysResult<mpsc::UnboundedReceiver<FdbEvent>> {
        self.net.watch_fdb()
    }
}
