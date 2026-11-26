//! [`NetOps`] implemented with rtnetlink.

use std::collections::BTreeMap;

use std::net::{Ipv4Addr, SocketAddrV4};

use futures_util::{StreamExt as _, TryStreamExt as _};
use rtnetlink::packet_core::{
    NLM_F_ACK, NLM_F_APPEND, NLM_F_CREATE, NLM_F_DUMP, NLM_F_REQUEST, NetlinkMessage,
    NetlinkPayload,
};
use rtnetlink::packet_route::link::{
    BridgeStpState, BridgeVlanInfoFlags, InfoBridge, InfoBridgePort, InfoData, InfoPortData,
    InfoVxlan, LinkAttribute, LinkExtentMask, LinkFlags, LinkInfo, LinkMessage, State, VxlanDf,
};
use rtnetlink::packet_route::neighbour::{
    NeighbourAddress, NeighbourAttribute, NeighbourFlags, NeighbourMessage, NeighbourState,
};
use rtnetlink::packet_route::{AddressFamily, RouteNetlinkMessage};
use rtnetlink::{Handle, LinkBridge, LinkBridgePort, LinkBridgeVlan, LinkUnspec, LinkVxlan};

use super::{
    BridgeOptions, LinkKind, LinkState, LinkUpdate, NetOps, PortVlans, SysError, SysResult,
    VlanFlags, VxlanParams, vlan_ranges,
};

/// The MAC address of the default FDB entry of a VXLAN interface: frames to
/// any address without an entry of their own go there.
const ALL_ZEROS_MAC: [u8; 6] = [0; 6];
/// `ENOENT`: what the kernel answers for an entry that is not there.
const ENOENT: i32 = 2;
/// `ENODEV`: the interface is gone.
const ENODEV: i32 = 19;

/// Network operations over an rtnetlink connection.
///
/// A netlink socket can run only one dump at a time (a second one fails with
/// EBUSY), so all dumps on this connection are serialised.
#[derive(Debug, Clone)]
pub struct Netlink {
    handle: Handle,
    dump: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Set once the kernel turned out to have no per-VLAN counters.
    no_vlan_stats: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The task running the connection, until someone takes it to watch it.
    connection: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Netlink {
    /// Opens a netlink connection. Must be called inside a tokio runtime;
    /// the connection task is spawned onto it.
    pub fn connect() -> SysResult<Self> {
        let (connection, handle, _) =
            rtnetlink::new_connection().map_err(|e| SysError::new("open rtnetlink socket", e))?;
        let task = tokio::spawn(connection);
        Ok(Self {
            handle,
            dump: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            no_vlan_stats: std::sync::Arc::default(),
            connection: std::sync::Arc::new(std::sync::Mutex::new(Some(task))),
        })
    }

    /// The task of the netlink connection (once). When it ends, every
    /// request fails from then on: whoever runs for long should watch it.
    pub fn take_connection(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Remembers that the kernel has no per-VLAN counters per port.
    fn no_vlan_stats(&self, bridge: &str, why: &str) {
        if !self
            .no_vlan_stats
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                "bridge {bridge}: the kernel has no per-VLAN counters per port ({why}); traffic per VLAN is not shown"
            );
        }
    }

    /// Serialises dumps on this connection.
    pub(crate) async fn dump_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.dump.lock().await
    }

    /// The addresses of the default FDB entries of all VXLAN interfaces,
    /// by index (a dump of every forwarding table: only for a change).
    async fn tunnel_remotes(&self) -> SysResult<super::TunnelRemotes> {
        let _dump = self.dump_guard().await;
        let mut msg = NeighbourMessage::default();
        msg.header.family = AddressFamily::Bridge;
        let mut req = NetlinkMessage::from(RouteNetlinkMessage::GetNeighbour(msg));
        req.header.flags = NLM_F_REQUEST | NLM_F_DUMP;
        let mut handle = self.handle.clone();
        let mut responses = handle
            .request(req)
            .map_err(|e| SysError::new("list the remotes of the VXLAN interfaces", e))?;
        let mut out: BTreeMap<u32, Vec<(Ipv4Addr, Option<u16>)>> = BTreeMap::new();
        while let Some(resp) = responses.next().await {
            match resp.payload {
                NetlinkPayload::Error(err) if err.code.is_some() => {
                    return Err(SysError::new(
                        "list the remotes of the VXLAN interfaces",
                        rtnetlink::Error::NetlinkError(err),
                    ));
                }
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(m)) => {
                    if let Some((index, address, port)) = parse_remote(&m) {
                        out.entry(index).or_default().push((address, port));
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// The underlying rtnetlink handle (used by other backends).
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Sends a neighbour (FDB) change and waits for the kernel's answer.
    async fn neighbour(
        &self,
        msg: RouteNetlinkMessage,
        flags: u16,
    ) -> Result<(), rtnetlink::Error> {
        let mut req = NetlinkMessage::from(msg);
        req.header.flags = NLM_F_REQUEST | NLM_F_ACK | flags;
        let mut handle = self.handle.clone();
        let mut responses = handle.request(req)?;
        while let Some(resp) = responses.next().await {
            if let NetlinkPayload::Error(err) = resp.payload
                && err.code.is_some()
            {
                return Err(rtnetlink::Error::NetlinkError(err));
            }
        }
        Ok(())
    }
}

fn parse_link(msg: &LinkMessage) -> LinkState {
    let mut name = String::new();
    let mut alias = None;
    let mut master = None;
    let mut mtu = 0;
    let mut oper_up = false;
    let mut kind_name = String::new();
    let mut kind = None;
    let mut isolated = None;
    let mut traffic = crate::api::Traffic::default();
    for attr in &msg.attributes {
        match attr {
            LinkAttribute::IfName(n) => name.clone_from(n),
            LinkAttribute::IfAlias(a) => alias = Some(a.clone()),
            LinkAttribute::Controller(m) => master = Some(*m),
            LinkAttribute::Mtu(m) => mtu = *m,
            LinkAttribute::OperState(s) => oper_up = matches!(s, State::Up | State::Unknown),
            LinkAttribute::Stats64(st) => {
                traffic = crate::api::Traffic {
                    rx_bytes: st.rx_bytes,
                    tx_bytes: st.tx_bytes,
                    rx_packets: st.rx_packets,
                    tx_packets: st.tx_packets,
                    rx_errors: st.rx_errors,
                    tx_errors: st.tx_errors,
                    rx_dropped: st.rx_dropped,
                    tx_dropped: st.tx_dropped,
                };
            }
            LinkAttribute::LinkInfo(infos) => {
                for info in infos {
                    match info {
                        LinkInfo::Kind(k) => kind_name = k.to_string(),
                        LinkInfo::Data(InfoData::Bridge(opts)) => {
                            kind = Some(LinkKind::Bridge(parse_bridge(opts)));
                        }
                        LinkInfo::Data(InfoData::Vxlan(opts)) => {
                            kind = Some(LinkKind::Vxlan(parse_vxlan(opts)));
                        }
                        LinkInfo::PortData(InfoPortData::BridgePort(opts)) => {
                            isolated = Some(opts.contains(&InfoBridgePort::Isolated(true)));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // A bridge port without the isolated attribute is not isolated.
    if master.is_some() && isolated.is_none() {
        isolated = Some(false);
    }
    LinkState {
        index: msg.header.index,
        name,
        kind: kind.unwrap_or(LinkKind::Other(kind_name)),
        alias: alias.filter(|a| !a.is_empty()),
        master,
        admin_up: msg.header.flags.contains(LinkFlags::Up),
        oper_up,
        mtu,
        isolated,
        traffic,
    }
}

fn parse_bridge(opts: &[InfoBridge]) -> BridgeOptions {
    let mut out = BridgeOptions {
        vlan_filtering: false,
        stp: false,
        default_pvid: 1,
        nf_call: false,
        vlan_stats: false,
    };
    let (mut stats, mut per_port) = (false, false);
    for opt in opts {
        match opt {
            InfoBridge::VlanFiltering(v) => out.vlan_filtering = *v,
            InfoBridge::StpState(s) => out.stp = !matches!(s, BridgeStpState::Disabled),
            InfoBridge::VlanDefaultPvid(p) => out.default_pvid = *p,
            InfoBridge::NfCallIpTables(v) => out.nf_call |= *v,
            InfoBridge::NfCallIp6Tables(v) => out.nf_call |= *v,
            InfoBridge::NfCallArpTables(v) => out.nf_call |= *v,
            InfoBridge::VlanStatsEnabled(v) => stats = *v,
            InfoBridge::VlanStatsPerPort(v) => per_port = *v,
            _ => {}
        }
    }
    out.vlan_stats = stats && per_port;
    out
}

fn parse_vxlan(opts: &[InfoVxlan]) -> VxlanParams {
    let mut out = VxlanParams {
        vni: 0,
        port: 0,
        source_port: None,
    };
    for opt in opts {
        match opt {
            InfoVxlan::Id(v) => out.vni = *v,
            InfoVxlan::Port(p) => out.port = *p,
            // A range of one port: a fixed source port.
            InfoVxlan::PortRange((low, high))
                if *low != 0 && u32::from(*high) == u32::from(*low) + 1 =>
            {
                out.source_port = Some(*low);
            }
            _ => {}
        }
    }
    out
}

/// The default FDB entry of a VXLAN interface in a bridge FDB message:
/// interface index, remote address and port (`None`: the interface's own).
fn parse_remote(msg: &NeighbourMessage) -> Option<(u32, Ipv4Addr, Option<u16>)> {
    if msg.header.family != AddressFamily::Bridge {
        return None;
    }
    let mut zero_mac = false;
    let mut address = None;
    let mut port = None;
    for attr in &msg.attributes {
        match attr {
            NeighbourAttribute::LinkLayerAddress(mac) => zero_mac = mac.as_slice() == ALL_ZEROS_MAC,
            // In a bridge FDB message the address comes as plain bytes.
            NeighbourAttribute::Destination(NeighbourAddress::Inet(a)) => address = Some(*a),
            NeighbourAttribute::Destination(NeighbourAddress::Other(bytes)) => {
                address = <[u8; 4]>::try_from(bytes.as_slice())
                    .ok()
                    .map(Ipv4Addr::from);
            }
            NeighbourAttribute::Port(p) => port = Some(*p),
            _ => {}
        }
    }
    // Entries of the bridge for the port have no destination.
    zero_mac
        .then_some(())
        .and(address)
        .map(|a| (msg.header.ifindex, a, port))
}

/// A message for the default FDB entry of VXLAN interface `index` (of the
/// interface itself, not of the bridge): to `remote`, or all of it.
fn fdb_message(index: u32, remote: Option<SocketAddrV4>) -> NeighbourMessage {
    let mut msg = NeighbourMessage::default();
    msg.header.family = AddressFamily::Bridge;
    msg.header.ifindex = index;
    msg.header.flags = NeighbourFlags::Own;
    msg.header.state = NeighbourState::Permanent;
    msg.attributes
        .push(NeighbourAttribute::LinkLayerAddress(ALL_ZEROS_MAC.to_vec()));
    if let Some(remote) = remote {
        msg.attributes
            .push(NeighbourAttribute::Destination(NeighbourAddress::Inet(
                *remote.ip(),
            )));
        msg.attributes.push(NeighbourAttribute::Port(remote.port()));
    }
    msg
}

/// An entry that is not there counts as removed.
fn ignore_enoent(e: rtnetlink::Error) -> Result<(), rtnetlink::Error> {
    match e {
        rtnetlink::Error::NetlinkError(ref err) if err.code.is_some_and(|c| c.get() == -ENOENT) => {
            Ok(())
        }
        e => Err(e),
    }
}

/// The bridge attributes for `options`. The per-port counters go only into
/// a new bridge: on one whose ports have VLANs the kernel refuses them
/// (EBUSY), even unchanged.
fn bridge_info(options: &BridgeOptions, create: bool) -> Vec<InfoBridge> {
    let mut info = vec![
        InfoBridge::VlanFiltering(options.vlan_filtering),
        InfoBridge::StpState(if options.stp {
            BridgeStpState::KernelStp
        } else {
            BridgeStpState::Disabled
        }),
        InfoBridge::VlanDefaultPvid(options.default_pvid),
        InfoBridge::NfCallIpTables(options.nf_call),
        InfoBridge::NfCallIp6Tables(options.nf_call),
        InfoBridge::NfCallArpTables(options.nf_call),
        InfoBridge::VlanStatsEnabled(options.vlan_stats),
    ];
    if create {
        info.push(InfoBridge::VlanStatsPerPort(options.vlan_stats));
    }
    info
}

fn vlan_message(index: u32, bridge_self: bool, vlans: &[(u16, VlanFlags)]) -> LinkMessage {
    let mut builder = LinkBridgeVlan::new(index);
    if bridge_self {
        builder = builder.bridge_self();
    }
    for (first, last, flags) in vlan_ranges(vlans) {
        let mut f = BridgeVlanInfoFlags::empty();
        if flags.pvid {
            f |= BridgeVlanInfoFlags::Pvid;
        }
        if flags.untagged {
            f |= BridgeVlanInfoFlags::Untagged;
        }
        builder = if first == last {
            builder.vlan(first, f)
        } else {
            builder.vlan_range_start(first, f).vlan_range_end(last, f)
        };
    }
    builder.build()
}

impl NetOps for Netlink {
    async fn links(&self) -> SysResult<Vec<LinkState>> {
        let _dump = self.dump_guard().await;
        let mut stream = self.handle.link().get().execute();
        let mut out = Vec::new();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| SysError::new("list network interfaces", e))?
        {
            out.push(parse_link(&msg));
        }
        Ok(out)
    }

    async fn port_vlans(&self) -> SysResult<BTreeMap<u32, PortVlans>> {
        let _dump = self.dump_guard().await;
        let mut stream = self
            .handle
            .link()
            .get()
            .set_filter_mask(
                AddressFamily::Bridge,
                vec![LinkExtentMask::BrvlanCompressed],
            )
            .execute();
        let mut out: BTreeMap<u32, PortVlans> = BTreeMap::new();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| SysError::new("list bridge VLANs", e))?
        {
            let mut range_start: Option<u16> = None;
            for attr in &msg.attributes {
                let LinkAttribute::AfSpecBridge(specs) = attr else {
                    continue;
                };
                for spec in specs {
                    let rtnetlink::packet_route::link::AfSpecBridge::VlanInfo(info) = spec else {
                        continue;
                    };
                    let flags = VlanFlags {
                        pvid: info.flags.contains(BridgeVlanInfoFlags::Pvid),
                        untagged: info.flags.contains(BridgeVlanInfoFlags::Untagged),
                    };
                    let entries = out.entry(msg.header.index).or_default();
                    if info.flags.contains(BridgeVlanInfoFlags::RangeBegin) {
                        range_start = Some(info.vid);
                    } else if info.flags.contains(BridgeVlanInfoFlags::RangeEnd) {
                        let first = range_start.take().unwrap_or(info.vid);
                        for vid in first..=info.vid {
                            entries.insert(vid, flags);
                        }
                    } else {
                        entries.insert(info.vid, flags);
                    }
                }
            }
        }
        out.retain(|_, v| !v.is_empty());
        Ok(out)
    }

    async fn create_bridge(&self, name: &str, options: &BridgeOptions) -> SysResult<()> {
        let add = |options: BridgeOptions| {
            let mut builder = LinkBridge::new(name);
            for info in bridge_info(&options, true) {
                builder = builder.append_info_data(info);
            }
            self.handle.link().add(builder.build()).execute()
        };
        let stats = options.vlan_stats && self.vlan_stats_supported();
        let first = add(BridgeOptions {
            vlan_stats: stats,
            ..*options
        })
        .await;
        match first {
            Ok(()) if stats => {
                // An old kernel may ignore the attributes it does not know.
                let created = self.links().await?.into_iter().find(|l| l.name == name);
                if let Some(LinkState {
                    kind: LinkKind::Bridge(got),
                    ..
                }) = created
                    && !got.vlan_stats
                {
                    self.no_vlan_stats(name, "the new bridge came back without them");
                }
                Ok(())
            }
            Ok(()) => Ok(()),
            Err(e) if stats => {
                // Without the counters, if that is what the kernel refused.
                add(BridgeOptions {
                    vlan_stats: false,
                    ..*options
                })
                .await
                .map_err(|again| SysError::new(format!("create bridge {name}"), again))?;
                self.no_vlan_stats(name, &e.to_string());
                Ok(())
            }
            Err(e) => Err(SysError::new(format!("create bridge {name}"), e)),
        }
    }

    fn vlan_stats_supported(&self) -> bool {
        !self
            .no_vlan_stats
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn set_bridge_options(
        &self,
        index: u32,
        name: &str,
        options: &BridgeOptions,
    ) -> SysResult<()> {
        let mut builder = LinkBridge::new(name).index(index);
        for info in bridge_info(options, false) {
            builder = builder.append_info_data(info);
        }
        self.handle
            .link()
            .change(builder.build())
            .execute()
            .await
            .map_err(|e| SysError::new(format!("set options of bridge {name}"), e))
    }

    async fn create_vxlan(&self, name: &str, params: &VxlanParams, mtu: u32) -> SysResult<()> {
        // No learning (the remote is set, the bridge learns the MACs), no
        // UDP checksum and no DF outside: a 1500-byte frame without DF still
        // passes a smaller path, fragmented.
        let mut builder = LinkVxlan::new(name, params.vni)
            .port(params.port)
            .learning(false)
            .udp_csum(false)
            .df(VxlanDf::Unset);
        if let Some(sport) = params.source_port {
            builder = builder.port_range(sport, sport.saturating_add(1));
        }
        self.handle
            .link()
            .add(builder.mtu(mtu).build())
            .execute()
            .await
            .map_err(|e| SysError::new(format!("create VXLAN interface {name}"), e))
    }

    async fn tunnel_remote(&self, index: u32) -> SysResult<Option<(Ipv4Addr, Option<u16>)>> {
        // A get of the default entry of the interface itself (NTF_SELF):
        // answered by the VXLAN driver, with its first address.
        let mut msg = NeighbourMessage::default();
        msg.header.family = AddressFamily::Bridge;
        msg.header.ifindex = index;
        msg.header.flags = NeighbourFlags::Own;
        msg.attributes
            .push(NeighbourAttribute::LinkLayerAddress(ALL_ZEROS_MAC.to_vec()));
        let mut req = NetlinkMessage::from(RouteNetlinkMessage::GetNeighbour(msg));
        req.header.flags = NLM_F_REQUEST;
        let fail = |e| SysError::new(format!("read the remote of VXLAN interface {index}"), e);
        let mut handle = self.handle.clone();
        let mut responses = handle.request(req).map_err(fail)?;
        let mut found = None;
        while let Some(resp) = responses.next().await {
            match resp.payload {
                // No entry, or the interface just went.
                NetlinkPayload::Error(err)
                    if err
                        .code
                        .is_some_and(|c| c.get() == -ENOENT || c.get() == -ENODEV) =>
                {
                    return Ok(None);
                }
                NetlinkPayload::Error(err) if err.code.is_some() => {
                    return Err(fail(rtnetlink::Error::NetlinkError(err)));
                }
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(m)) => {
                    found = parse_remote(&m).map(|(_, address, port)| (address, port));
                }
                _ => {}
            }
        }
        Ok(found)
    }

    async fn set_tunnel_remote(
        &self,
        index: u32,
        name: &str,
        own_port: u16,
        remote: Option<SocketAddrV4>,
    ) -> SysResult<()> {
        let fail = |e: rtnetlink::Error| match remote {
            Some(r) => SysError::new(
                format!("set the remote of VXLAN interface {name} to {r}"),
                e,
            ),
            None => SysError::new(format!("remove the remote of VXLAN interface {name}"), e),
        };
        let Some(remote) = remote else {
            // Without an address: the whole entry goes.
            return self
                .neighbour(
                    RouteNetlinkMessage::DelNeighbour(fdb_message(index, None)),
                    0,
                )
                .await
                .or_else(ignore_enoent)
                .map_err(fail);
        };
        // The kernel never replaces the default entry of a VXLAN interface:
        // the new address is appended, then the old ones go (no gap, at most
        // a frame sent twice).
        let old: Vec<(Ipv4Addr, Option<u16>)> = self
            .tunnel_remotes()
            .await?
            .remove(&index)
            .unwrap_or_default();
        self.neighbour(
            RouteNetlinkMessage::NewNeighbour(fdb_message(index, Some(remote))),
            NLM_F_CREATE | NLM_F_APPEND,
        )
        .await
        .map_err(fail)?;
        for (address, port) in old {
            let gone = SocketAddrV4::new(address, port.unwrap_or(own_port));
            if gone == remote {
                continue;
            }
            self.neighbour(
                RouteNetlinkMessage::DelNeighbour(fdb_message(index, Some(gone))),
                0,
            )
            .await
            .or_else(ignore_enoent)
            .map_err(fail)?;
        }
        Ok(())
    }

    async fn delete_link(&self, index: u32) -> SysResult<()> {
        self.handle
            .link()
            .del(index)
            .execute()
            .await
            .map_err(|e| SysError::new(format!("delete interface #{index}"), e))
    }

    async fn update_link(&self, index: u32, update: &LinkUpdate) -> SysResult<()> {
        let mut builder = LinkUnspec::new_with_index(index);
        match update.up {
            Some(true) => builder = builder.up(),
            Some(false) => builder = builder.down(),
            None => {}
        }
        match update.master {
            Some(Some(master)) => builder = builder.controller(master),
            Some(None) => builder = builder.nocontroller(),
            None => {}
        }
        if let Some(mtu) = update.mtu {
            builder = builder.mtu(mtu);
        }
        if let Some(alias) = &update.alias {
            builder = builder.alias(alias.clone());
        }
        self.handle
            .link()
            .set(builder.build())
            .execute()
            .await
            .map_err(|e| SysError::new(format!("change interface #{index} ({update:?})"), e))
    }

    async fn set_port_isolated(&self, index: u32, isolated: bool) -> SysResult<()> {
        let msg = LinkBridgePort::new(index).isolated(isolated).build();
        self.handle
            .link()
            .set_port(msg)
            .execute()
            .await
            .map_err(|e| SysError::new(format!("set isolation of port #{index}"), e))
    }

    async fn add_vlans(
        &self,
        index: u32,
        bridge_self: bool,
        vlans: &[(u16, VlanFlags)],
    ) -> SysResult<()> {
        if vlans.is_empty() {
            return Ok(());
        }
        self.handle
            .link()
            .set(vlan_message(index, bridge_self, vlans))
            .execute()
            .await
            .map_err(|e| SysError::new(format!("add VLANs to interface #{index}"), e))
    }

    async fn flush_fdb(&self, bridge: u32, name: &str) -> SysResult<()> {
        let msg = LinkBridge::new(name).index(bridge).fdb_flush().build();
        self.handle
            .link()
            .change(msg)
            .execute()
            .await
            .map_err(|e| SysError::new(format!("flush learned MAC addresses of {name}"), e))
    }

    async fn del_vlans(&self, index: u32, bridge_self: bool, vids: &[u16]) -> SysResult<()> {
        if vids.is_empty() {
            return Ok(());
        }
        let entries: Vec<(u16, VlanFlags)> = vids.iter().map(|v| (*v, VlanFlags::TAGGED)).collect();
        self.handle
            .link()
            .del_with_message(vlan_message(index, bridge_self, &entries))
            .execute()
            .await
            .map_err(|e| SysError::new(format!("remove VLANs from interface #{index}"), e))
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use rtnetlink::packet_route::AddressFamily;
    use rtnetlink::packet_route::link::{InfoBridge, InfoKind, InfoVxlan};
    use rtnetlink::packet_route::neighbour::{
        NeighbourAddress, NeighbourAttribute, NeighbourMessage,
    };

    use super::{BridgeOptions, VxlanParams, bridge_info, parse_bridge, parse_remote, parse_vxlan};
    use crate::desired::BRIDGE_OPTIONS;

    #[test]
    fn kind_names_match_kernel() {
        assert_eq!(InfoKind::Vxlan.to_string(), "vxlan");
        assert_eq!(InfoKind::Bridge.to_string(), "bridge");
    }

    #[test]
    fn a_vxlan_dump_gives_the_parameters() {
        let fixed = [
            InfoVxlan::Id(0x00ab_cdef),
            InfoVxlan::Port(51822),
            InfoVxlan::PortRange((51822, 51823)),
            InfoVxlan::Learning(false),
        ];
        assert_eq!(
            parse_vxlan(&fixed),
            VxlanParams {
                vni: 0x00ab_cdef,
                port: 51822,
                source_port: Some(51822),
            }
        );
        // The kernel dumps (0, 0) without a range: a port per flow.
        let per_flow = [
            InfoVxlan::Id(7),
            InfoVxlan::Port(51822),
            InfoVxlan::PortRange((0, 0)),
        ];
        assert_eq!(parse_vxlan(&per_flow).source_port, None);
    }

    #[test]
    fn only_the_default_entry_of_a_vxlan_is_its_remote() {
        let mut own = NeighbourMessage::default();
        own.header.family = AddressFamily::Bridge;
        own.header.ifindex = 9;
        own.attributes = vec![
            NeighbourAttribute::LinkLayerAddress(vec![0; 6]),
            // As the kernel sends it in a bridge message: plain bytes.
            NeighbourAttribute::Destination(NeighbourAddress::Other(vec![10, 0, 0, 2])),
            NeighbourAttribute::Port(4000),
        ];
        assert_eq!(
            parse_remote(&own),
            Some((9, Ipv4Addr::new(10, 0, 0, 2), Some(4000)))
        );
        // A MAC the bridge learned behind the tunnel: no destination.
        let mut learned = own.clone();
        learned.attributes = vec![NeighbourAttribute::LinkLayerAddress(vec![2, 0, 0, 0, 0, 1])];
        assert_eq!(parse_remote(&learned), None);
    }

    #[test]
    fn per_vlan_counters_need_both_options() {
        let created = bridge_info(&BRIDGE_OPTIONS, true);
        assert!(created.contains(&InfoBridge::VlanStatsEnabled(true)));
        assert!(created.contains(&InfoBridge::VlanStatsPerPort(true)));
        assert_eq!(parse_bridge(&created), BRIDGE_OPTIONS);
        // A change never sends the per-port part: the kernel refuses it
        // while ports have VLANs.
        let changed = bridge_info(&BRIDGE_OPTIONS, false);
        assert!(
            !changed
                .iter()
                .any(|i| matches!(i, InfoBridge::VlanStatsPerPort(_)))
        );
        let only_enabled = [InfoBridge::VlanStatsEnabled(true)];
        assert!(!parse_bridge(&only_enabled).vlan_stats);
        let without = BridgeOptions {
            vlan_stats: false,
            ..BRIDGE_OPTIONS
        };
        assert_eq!(parse_bridge(&bridge_info(&without, true)), without);
    }
}
