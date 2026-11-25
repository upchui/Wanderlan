//! [`ObserveOps`] with rtnetlink (RTM_GETNEIGH dumps and RTNLGRP_NEIGH,
//! RTM_GETSTATS dumps for the per-VLAN counters).

use futures_util::StreamExt as _;
use rtnetlink::packet_core::{NLM_F_DUMP, NLM_F_REQUEST, NetlinkMessage, NetlinkPayload};
use rtnetlink::packet_route::neighbour::{
    NeighbourAddress, NeighbourAttribute, NeighbourFlags, NeighbourMessage, NeighbourState,
};
use rtnetlink::packet_route::stats::{
    BridgeXstat, LinkXstatGroup, StatsAttribute, StatsFilterMask, StatsMessage,
};
use rtnetlink::packet_route::{AddressFamily, RouteNetlinkMessage};
use rtnetlink::sys::AsyncSocket as _;
use rtnetlink::{MulticastGroup, new_multicast_connection};
use tokio::sync::mpsc;

use super::linux::LinuxSystem;
use super::netlink::Netlink;
use super::observe::{FdbEntry, FdbEvent, ObserveOps, VlanCounters};
use super::{SysError, SysResult};

/// Kernel ticks per second of `NDA_CACHEINFO` (USER_HZ).
const USER_HZ: u64 = 100;
/// Receive buffer of the FDB watch socket.
const FDB_RX_BUFFER: usize = 4 * 1024 * 1024;

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

/// The per-VLAN counters of one bridge port in a stats message.
fn parse_vlan_stats(msg: &StatsMessage) -> Vec<VlanCounters> {
    let port = msg.header.ifindex;
    msg.attributes
        .iter()
        .filter_map(|a| match a {
            StatsAttribute::LinkXstatsPort(groups) => Some(groups),
            _ => None,
        })
        .flatten()
        .filter_map(|g| match g {
            LinkXstatGroup::Bridge(x) => Some(x),
            _ => None,
        })
        .flatten()
        .filter_map(|x| match x {
            BridgeXstat::Vlan(v) => Some(VlanCounters {
                port,
                vid: v.vid,
                rx_bytes: v.rx_bytes,
                rx_packets: v.rx_packets,
                tx_bytes: v.tx_bytes,
                tx_packets: v.tx_packets,
            }),
            _ => None,
        })
        .collect()
}

impl ObserveOps for Netlink {
    async fn vlan_stats(&self) -> SysResult<Vec<VlanCounters>> {
        let _dump = self.dump_guard().await;
        let mut msg = StatsMessage::default();
        msg.header.filter_mask = StatsFilterMask::LinkXstatsPort;
        let mut req = NetlinkMessage::from(RouteNetlinkMessage::GetStats(msg));
        req.header.flags = NLM_F_REQUEST | NLM_F_DUMP;
        let mut handle = self.handle().clone();
        let mut responses = handle
            .request(req)
            .map_err(|e| SysError::new("read the VLAN counters", e))?;
        let mut out = Vec::new();
        while let Some(resp) = responses.next().await {
            match resp.payload {
                NetlinkPayload::Error(err) if err.code.is_some() => {
                    return Err(SysError::new(
                        "read the VLAN counters",
                        rtnetlink::Error::NetlinkError(err),
                    ));
                }
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewStats(m)) => {
                    out.extend(parse_vlan_stats(&m));
                }
                _ => {}
            }
        }
        Ok(out)
    }

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
        let (mut connection, handle, mut messages) =
            new_multicast_connection(&[MulticastGroup::Neigh])
                .map_err(|e| SysError::new("watch bridge FDB", e))?;
        // A broadcast storm produces FDB updates faster than they can be read.
        // A large buffer keeps most of them; without ENOBUFS the kernel drops
        // the rest silently instead of reporting an error.
        let socket = connection.socket_mut().socket_mut();
        if let Err(e) = socket.set_rx_buf_sz(FDB_RX_BUFFER) {
            tracing::warn!("cannot enlarge the FDB watch buffer: {e}");
        }
        if let Err(e) = socket.set_no_enobufs(true) {
            tracing::warn!("cannot set NETLINK_NO_ENOBUFS on the FDB watch socket: {e}");
        }
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

    async fn vlan_stats(&self) -> SysResult<Vec<VlanCounters>> {
        self.net.vlan_stats().await
    }

    fn watch_fdb(&self) -> SysResult<mpsc::UnboundedReceiver<FdbEvent>> {
        self.net.watch_fdb()
    }
}

#[cfg(test)]
mod tests {
    use rtnetlink::packet_route::stats::{
        BridgeVlanXstats, BridgeXstat, LinkXstatGroup, StatsAttribute, StatsMessage,
    };

    use super::{VlanCounters, parse_vlan_stats};

    #[test]
    fn vlan_counters_of_a_port() {
        let mut msg = StatsMessage::default();
        msg.header.ifindex = 7;
        let vlan = |vid: u16, rx: u64| {
            let mut v = BridgeVlanXstats::default();
            v.vid = vid;
            v.rx_bytes = rx;
            v.rx_packets = rx / 100;
            v.tx_bytes = 2 * rx;
            v.tx_packets = rx / 50;
            BridgeXstat::Vlan(v)
        };
        msg.attributes = vec![StatsAttribute::LinkXstatsPort(vec![
            LinkXstatGroup::Bridge(vec![vlan(10, 1000), vlan(20, 5000)]),
        ])];
        let got = parse_vlan_stats(&msg);
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[1],
            VlanCounters {
                port: 7,
                vid: 20,
                rx_bytes: 5000,
                rx_packets: 50,
                tx_bytes: 10000,
                tx_packets: 100,
            }
        );
        // Other interfaces (no bridge port) carry no counters.
        assert!(parse_vlan_stats(&StatsMessage::default()).is_empty());
    }
}
