//! [`NetOps`] implemented with rtnetlink.

use std::collections::BTreeMap;

use futures_util::TryStreamExt as _;
use rtnetlink::packet_core::{DefaultNla, Nla as _};
use rtnetlink::packet_route::AddressFamily;
use rtnetlink::packet_route::link::{
    BridgeStpState, BridgeVlanInfoFlags, GreEncapFlags, GreEncapType, GreIOFlags, InfoBridge,
    InfoBridgePort, InfoData, InfoGre, InfoPortData, LinkAttribute, LinkExtentMask, LinkFlags,
    LinkInfo, LinkMessage, State,
};
use rtnetlink::{Handle, LinkBridge, LinkBridgePort, LinkBridgeVlan, LinkGre, LinkUnspec};

use super::{
    BridgeOptions, FouEncap, GretapParams, LinkKind, LinkState, LinkUpdate, NetOps, PortVlans,
    SysError, SysResult, VlanFlags, vlan_ranges,
};

/// `IFLA_GRE_IGNORE_DF`, not modelled by netlink-packet-route.
const IFLA_GRE_IGNORE_DF: u16 = 19;

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

    /// The underlying rtnetlink handle (used by other backends).
    pub fn handle(&self) -> &Handle {
        &self.handle
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
                        LinkInfo::Data(InfoData::GreTap(opts)) => {
                            kind = Some(LinkKind::Gretap(parse_gretap(opts)));
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

fn parse_gretap(opts: &[InfoGre]) -> GretapParams {
    let mut out = GretapParams {
        local: std::net::Ipv4Addr::UNSPECIFIED,
        remote: std::net::Ipv4Addr::UNSPECIFIED,
        ignore_df: false,
        pmtudisc: true,
        key: None,
        encap: None,
    };
    let mut keyed = false;
    let mut ikey = 0;
    let mut fou = false;
    let (mut sport, mut dport) = (0, 0);
    for opt in opts {
        match opt {
            InfoGre::Local(a) => out.local = *a,
            InfoGre::Remote(a) => out.remote = *a,
            InfoGre::PathMTUDiscovery(v) => out.pmtudisc = *v,
            InfoGre::IFlags(flags) => keyed = flags.contains(GreIOFlags::Key),
            InfoGre::IKey(k) => ikey = *k,
            InfoGre::EncapType(t) => fou = *t == GreEncapType::Fou,
            InfoGre::SourcePort(p) => sport = *p,
            InfoGre::DestinationPort(p) => dport = *p,
            InfoGre::Other(nla) if nla.kind() == IFLA_GRE_IGNORE_DF => {
                let mut buf = vec![0u8; nla.value_len()];
                nla.emit_value(&mut buf);
                out.ignore_df = buf.first().is_some_and(|b| *b != 0);
            }
            _ => {}
        }
    }
    out.key = keyed.then_some(ikey);
    out.encap = fou.then_some(FouEncap { sport, dport });
    out
}

/// The GRETAP attributes for `params`; a change sends all of them again.
fn gretap_info(params: &GretapParams) -> Vec<InfoGre> {
    let mut info = vec![
        InfoGre::Local(params.local),
        InfoGre::Remote(params.remote),
        InfoGre::Ttl(0),
        InfoGre::PathMTUDiscovery(params.pmtudisc),
        InfoGre::Other(DefaultNla::new(
            IFLA_GRE_IGNORE_DF,
            vec![u8::from(params.ignore_df)],
        )),
    ];
    if let Some(key) = params.key {
        info.extend([
            InfoGre::IFlags(GreIOFlags::Key),
            InfoGre::OFlags(GreIOFlags::Key),
            InfoGre::IKey(key),
            InfoGre::OKey(key),
        ]);
    }
    // Without a UDP checksum (no encap flags): GSO and GRO for UDP tunnels.
    let (kind, sport, dport) = match params.encap {
        Some(e) => (GreEncapType::Fou, e.sport, e.dport),
        None => (GreEncapType::None, 0, 0),
    };
    info.extend([
        InfoGre::EncapType(kind),
        InfoGre::EncapFlags(GreEncapFlags::empty()),
        InfoGre::SourcePort(sport),
        InfoGre::DestinationPort(dport),
    ]);
    info
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

    async fn create_gretap(&self, name: &str, params: &GretapParams, mtu: u32) -> SysResult<()> {
        let data = InfoData::GreTap(gretap_info(params));
        let msg = LinkGre::new_gretap(name)
            .set_info_data(data)
            .mtu(mtu)
            .build();
        self.handle
            .link()
            .add(msg)
            .execute()
            .await
            .map_err(|e| SysError::new(format!("create GRETAP interface {name}"), e))
    }

    async fn change_gretap(
        &self,
        index: u32,
        name: &str,
        params: &GretapParams,
        mtu: u32,
    ) -> SysResult<()> {
        // A change sets every parameter anew (the kernel zeroes what is
        // missing) and keeps the MTU (else it follows the new headers).
        let msg = LinkGre::new_gretap(name)
            .index(index)
            .set_info_data(InfoData::GreTap(gretap_info(params)))
            .mtu(mtu)
            .build();
        self.handle
            .link()
            .change(msg)
            .execute()
            .await
            .map_err(|e| SysError::new(format!("change GRETAP interface {name}"), e))
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
    use rtnetlink::packet_route::link::{InfoBridge, InfoKind};

    use super::{BridgeOptions, bridge_info, parse_bridge};
    use crate::desired::BRIDGE_OPTIONS;

    #[test]
    fn kind_names_match_kernel() {
        assert_eq!(InfoKind::GreTap.to_string(), "gretap");
        assert_eq!(InfoKind::Bridge.to_string(), "bridge");
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
