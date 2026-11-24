//! [`NetOps`] implemented with rtnetlink.

use std::collections::BTreeMap;

use futures_util::TryStreamExt as _;
use rtnetlink::packet_core::{DefaultNla, Nla as _};
use rtnetlink::packet_route::AddressFamily;
use rtnetlink::packet_route::link::{
    BridgeStpState, BridgeVlanInfoFlags, InfoBridge, InfoBridgePort, InfoData, InfoGre,
    InfoPortData, LinkAttribute, LinkExtentMask, LinkFlags, LinkInfo, LinkMessage, State,
};
use rtnetlink::{Handle, LinkBridge, LinkBridgePort, LinkBridgeVlan, LinkGre, LinkUnspec};

use super::{
    BridgeOptions, GretapParams, LinkKind, LinkState, LinkUpdate, NetOps, PortVlans, SysError,
    SysResult, VlanFlags, vlan_ranges,
};

/// `IFLA_GRE_IGNORE_DF`, not modelled by netlink-packet-route.
const IFLA_GRE_IGNORE_DF: u16 = 19;

/// Network operations over an rtnetlink connection.
#[derive(Debug, Clone)]
pub struct Netlink {
    handle: Handle,
}

impl Netlink {
    /// Opens a netlink connection. Must be called inside a tokio runtime;
    /// the connection task is spawned onto it.
    pub fn connect() -> SysResult<Self> {
        let (connection, handle, _) =
            rtnetlink::new_connection().map_err(|e| SysError::new("open rtnetlink socket", e))?;
        tokio::spawn(connection);
        Ok(Self { handle })
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
    for attr in &msg.attributes {
        match attr {
            LinkAttribute::IfName(n) => name.clone_from(n),
            LinkAttribute::IfAlias(a) => alias = Some(a.clone()),
            LinkAttribute::Controller(m) => master = Some(*m),
            LinkAttribute::Mtu(m) => mtu = *m,
            LinkAttribute::OperState(s) => oper_up = matches!(s, State::Up | State::Unknown),
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
    }
}

fn parse_bridge(opts: &[InfoBridge]) -> BridgeOptions {
    let mut out = BridgeOptions {
        vlan_filtering: false,
        stp: false,
        default_pvid: 1,
        nf_call: false,
    };
    for opt in opts {
        match opt {
            InfoBridge::VlanFiltering(v) => out.vlan_filtering = *v,
            InfoBridge::StpState(s) => out.stp = !matches!(s, BridgeStpState::Disabled),
            InfoBridge::VlanDefaultPvid(p) => out.default_pvid = *p,
            InfoBridge::NfCallIpTables(v) => out.nf_call |= *v,
            InfoBridge::NfCallIp6Tables(v) => out.nf_call |= *v,
            InfoBridge::NfCallArpTables(v) => out.nf_call |= *v,
            _ => {}
        }
    }
    out
}

fn parse_gretap(opts: &[InfoGre]) -> GretapParams {
    let mut out = GretapParams {
        local: std::net::Ipv4Addr::UNSPECIFIED,
        remote: std::net::Ipv4Addr::UNSPECIFIED,
        ignore_df: false,
        pmtudisc: true,
    };
    for opt in opts {
        match opt {
            InfoGre::Local(a) => out.local = *a,
            InfoGre::Remote(a) => out.remote = *a,
            InfoGre::PathMTUDiscovery(v) => out.pmtudisc = *v,
            InfoGre::Other(nla) if nla.kind() == IFLA_GRE_IGNORE_DF => {
                let mut buf = vec![0u8; nla.value_len()];
                nla.emit_value(&mut buf);
                out.ignore_df = buf.first().is_some_and(|b| *b != 0);
            }
            _ => {}
        }
    }
    out
}

fn bridge_info(options: &BridgeOptions) -> Vec<InfoBridge> {
    vec![
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
    ]
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
        let mut builder = LinkBridge::new(name);
        for info in bridge_info(options) {
            builder = builder.append_info_data(info);
        }
        self.handle
            .link()
            .add(builder.build())
            .execute()
            .await
            .map_err(|e| SysError::new(format!("create bridge {name}"), e))
    }

    async fn set_bridge_options(
        &self,
        index: u32,
        name: &str,
        options: &BridgeOptions,
    ) -> SysResult<()> {
        let mut builder = LinkBridge::new(name).index(index);
        for info in bridge_info(options) {
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
        let data = InfoData::GreTap(vec![
            InfoGre::Local(params.local),
            InfoGre::Remote(params.remote),
            InfoGre::Ttl(0),
            InfoGre::PathMTUDiscovery(params.pmtudisc),
            InfoGre::Other(DefaultNla::new(
                IFLA_GRE_IGNORE_DF,
                vec![u8::from(params.ignore_df)],
            )),
        ]);
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
    use rtnetlink::packet_route::link::InfoKind;

    #[test]
    fn kind_names_match_kernel() {
        assert_eq!(InfoKind::GreTap.to_string(), "gretap");
        assert_eq!(InfoKind::Bridge.to_string(), "bridge");
    }
}
