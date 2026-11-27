//! [`BreakoutOps`] for Linux: rtnetlink for the uplinks, rules, routes,
//! addresses and forwarding entries, `nft` for the tables, procfs for the
//! sysctls.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use futures_util::{StreamExt as _, TryStreamExt as _};
use rtnetlink::packet_core::{
    NLM_F_ACK, NLM_F_CREATE, NLM_F_REPLACE, NLM_F_REQUEST, NetlinkMessage, NetlinkPayload,
};
use rtnetlink::packet_route::address::{AddressAttribute, AddressFlags, AddressScope};
use rtnetlink::packet_route::link::{LinkAttribute, LinkFlags, State};
use rtnetlink::packet_route::neighbour::{
    NeighbourAttribute, NeighbourFlags, NeighbourMessage, NeighbourState,
};
use rtnetlink::packet_route::route::{
    RouteAddress, RouteAttribute, RouteHeader, RouteMessage, RouteProtocol, RouteScope, RouteVia,
};
use rtnetlink::packet_route::rule::{RuleAction, RuleAttribute, RuleMessage};
use rtnetlink::packet_route::{AddressFamily, RouteNetlinkMessage};
use rtnetlink::{IpVersion, RouteMessageBuilder};

use super::breakout::{
    BreakoutOps, BreakoutSpec, BreakoutState, RouteSpec, RuleSpec, StaticFdb, TABLE, Uplink,
    Uplinks, delete_script, nft_script, parse_tables, wanted,
};
use super::linux::LinuxSystem;
use super::netlink::Netlink;
use super::nft_ip::apply_script;
use super::{SysError, SysResult};
use crate::breakout::{ROUTER_ADDRESS, RT_PROTOCOL, RULE_PRIORITY, is_breakout_name};

/// Any public address: where the default routes lead (never contacted).
const PROBE4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
/// The same for IPv6.
const PROBE6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
/// `ENOENT`.
const ENOENT: i32 = 2;
/// `EEXIST`.
const EEXIST: i32 = 17;

fn code(e: &rtnetlink::Error) -> Option<i32> {
    match e {
        rtnetlink::Error::NetlinkError(err) => err.code.map(|c| -c.get()),
        _ => None,
    }
}

/// The table of a route or rule message (the attribute for large IDs).
fn route_table(msg: &RouteMessage) -> u32 {
    msg.attributes
        .iter()
        .find_map(|a| match a {
            RouteAttribute::Table(t) => Some(*t),
            _ => None,
        })
        .unwrap_or(u32::from(msg.header.table))
}

fn rule_table(msg: &RuleMessage) -> u32 {
    msg.attributes
        .iter()
        .find_map(|a| match a {
            RuleAttribute::Table(t) => Some(*t),
            _ => None,
        })
        .unwrap_or(u32::from(msg.header.table))
}

fn route_gateway(msg: &RouteMessage) -> Option<IpAddr> {
    msg.attributes.iter().find_map(|a| match a {
        RouteAttribute::Gateway(RouteAddress::Inet(g)) => Some(IpAddr::V4(*g)),
        RouteAttribute::Gateway(RouteAddress::Inet6(g)) => Some(IpAddr::V6(*g)),
        RouteAttribute::Via(RouteVia::Inet6(g)) => Some(IpAddr::V6(*g)),
        _ => None,
    })
}

fn route_oif(msg: &RouteMessage) -> Option<u32> {
    msg.attributes.iter().find_map(|a| match a {
        RouteAttribute::Oif(i) => Some(*i),
        _ => None,
    })
}

fn route_priority(msg: &RouteMessage) -> u32 {
    msg.attributes
        .iter()
        .find_map(|a| match a {
            RouteAttribute::Priority(p) => Some(*p),
            _ => None,
        })
        .unwrap_or(0)
}

fn is_default(msg: &RouteMessage) -> bool {
    msg.header.destination_prefix_length == 0
        && msg.header.kind == rtnetlink::packet_route::route::RouteType::Unicast
}

impl Netlink {
    /// Interface names by index, and the up/point-to-point flags.
    async fn link_flags(&self) -> SysResult<BTreeMap<u32, (String, bool, bool)>> {
        let _dump = self.dump_guard().await;
        let mut stream = self.handle().link().get().execute();
        let mut out = BTreeMap::new();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| SysError::new("list network interfaces", e))?
        {
            let mut name = String::new();
            let mut up = false;
            for attr in &msg.attributes {
                match attr {
                    LinkAttribute::IfName(n) => name.clone_from(n),
                    LinkAttribute::OperState(s) => up = matches!(s, State::Up | State::Unknown),
                    _ => {}
                }
            }
            let p2p = msg.header.flags.contains(LinkFlags::Pointopoint)
                || msg.header.flags.contains(LinkFlags::Noarp);
            out.insert(msg.header.index, (name, up, p2p));
        }
        Ok(out)
    }

    async fn dump_routes(&self, v6: bool) -> SysResult<Vec<RouteMessage>> {
        let _dump = self.dump_guard().await;
        let msg = if v6 {
            RouteMessageBuilder::<Ipv6Addr>::new().build()
        } else {
            RouteMessageBuilder::<Ipv4Addr>::new().build()
        };
        let mut stream = self.handle().route().get(msg).execute();
        let mut out = Vec::new();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| SysError::new("list routes", e))?
        {
            out.push(msg);
        }
        Ok(out)
    }

    async fn dump_rules(&self, v6: bool) -> SysResult<Vec<RuleMessage>> {
        let _dump = self.dump_guard().await;
        let version = if v6 { IpVersion::V6 } else { IpVersion::V4 };
        let mut stream = self.handle().rule().get(version).execute();
        let mut out = Vec::new();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| SysError::new("list routing rules", e))?
        {
            out.push(msg);
        }
        Ok(out)
    }

    /// The interface a packet to `dst` leaves by (the policy rules count).
    async fn route_to(&self, dst: IpAddr) -> Option<u32> {
        let msg = match dst {
            IpAddr::V4(a) => RouteMessageBuilder::<Ipv4Addr>::new()
                .destination_prefix(a, 32)
                .build(),
            IpAddr::V6(a) => RouteMessageBuilder::<Ipv6Addr>::new()
                .destination_prefix(a, 128)
                .build(),
        };
        let mut stream = self.handle().route().get(msg).execute();
        match stream.next().await {
            Some(Ok(m)) if m.header.kind == rtnetlink::packet_route::route::RouteType::Unicast => {
                route_oif(&m)
            }
            _ => None,
        }
    }

    /// The uplinks: every interface with its addresses and default route.
    pub async fn uplinks(&self) -> SysResult<Uplinks> {
        let links = self.link_flags().await?;
        let mut by_index: BTreeMap<u32, Uplink> = links
            .iter()
            .map(|(i, (_, up, p2p))| {
                (
                    *i,
                    Uplink {
                        up: *up,
                        pointopoint: *p2p,
                        ..Uplink::default()
                    },
                )
            })
            .collect();
        {
            let _dump = self.dump_guard().await;
            let mut stream = self.handle().address().get().execute();
            while let Some(msg) = stream
                .try_next()
                .await
                .map_err(|e| SysError::new("list addresses", e))?
            {
                let Some(uplink) = by_index.get_mut(&msg.header.index) else {
                    continue;
                };
                let flags = msg.attributes.iter().find_map(|a| match a {
                    AddressAttribute::Flags(f) => Some(*f),
                    _ => None,
                });
                let unusable = flags.is_some_and(|f| {
                    f.contains(AddressFlags::Tentative) || f.contains(AddressFlags::Dadfailed)
                });
                let address = msg.attributes.iter().find_map(|a| match a {
                    AddressAttribute::Local(ip) => Some(*ip),
                    _ => None,
                });
                let address = address.or_else(|| {
                    msg.attributes.iter().find_map(|a| match a {
                        AddressAttribute::Address(ip) => Some(*ip),
                        _ => None,
                    })
                });
                match address {
                    Some(IpAddr::V4(a)) if msg.header.scope == AddressScope::Universe => {
                        uplink.v4.push(a);
                    }
                    // A global unicast address (2000::/3): a ULA alone
                    // does not reach the internet.
                    Some(IpAddr::V6(a))
                        if msg.header.scope == AddressScope::Universe
                            && !unusable
                            && a.segments()[0] & 0xe000 == 0x2000 =>
                    {
                        uplink.v6.push(a);
                    }
                    _ => {}
                }
            }
        }
        for v6 in [false, true] {
            let mut best: BTreeMap<u32, (u32, Option<IpAddr>)> = BTreeMap::new();
            for msg in self.dump_routes(v6).await? {
                if route_table(&msg) != u32::from(RouteHeader::RT_TABLE_MAIN) || !is_default(&msg) {
                    continue;
                }
                let Some(oif) = route_oif(&msg) else { continue };
                let metric = route_priority(&msg);
                if best.get(&oif).is_none_or(|(m, _)| metric < *m) {
                    best.insert(oif, (metric, route_gateway(&msg)));
                }
            }
            for (oif, (_, gateway)) in best {
                let Some(uplink) = by_index.get_mut(&oif) else {
                    continue;
                };
                if v6 {
                    uplink.default6 = Some(gateway.and_then(|g| match g {
                        IpAddr::V6(g) => Some(g),
                        IpAddr::V4(_) => None,
                    }));
                } else {
                    uplink.default4 = Some(gateway.and_then(|g| match g {
                        IpAddr::V4(g) => Some(g),
                        IpAddr::V6(_) => None,
                    }));
                }
            }
        }
        let name_of = |i: Option<u32>| i.and_then(|i| links.get(&i)).map(|(n, _, _)| n.clone());
        let auto4 = name_of(self.route_to(IpAddr::V4(PROBE4)).await);
        let auto6 = name_of(self.route_to(IpAddr::V6(PROBE6)).await);
        Ok(Uplinks {
            by_name: by_index
                .into_iter()
                .filter_map(|(i, u)| links.get(&i).map(|(n, _, _)| (n.clone(), u)))
                .collect(),
            auto4,
            auto6,
        })
    }

    /// The rules and routes of the breakout (by their protocol), with the
    /// messages to delete them.
    async fn ours(
        &self,
        names: &BTreeMap<u32, (String, bool, bool)>,
    ) -> SysResult<(Vec<(RuleSpec, RuleMessage)>, Vec<(RouteSpec, RouteMessage)>)> {
        let mut rules = Vec::new();
        let mut routes = Vec::new();
        for v6 in [false, true] {
            for msg in self.dump_rules(v6).await? {
                let ours = msg.attributes.iter().any(|a| {
                    matches!(a, RuleAttribute::Protocol(RouteProtocol::Other(p)) if *p == RT_PROTOCOL)
                });
                if !ours {
                    continue;
                }
                let fwmark = msg.attributes.iter().find_map(|a| match a {
                    RuleAttribute::FwMark(m) => Some(*m),
                    _ => None,
                });
                let iif = msg.attributes.iter().find_map(|a| match a {
                    RuleAttribute::Iifname(n) => Some(n.clone()),
                    _ => None,
                });
                let table = rule_table(&msg);
                rules.push((
                    RuleSpec {
                        v6,
                        fwmark,
                        iif,
                        table,
                    },
                    msg,
                ));
            }
            for msg in self.dump_routes(v6).await? {
                if msg.header.protocol != RouteProtocol::Other(RT_PROTOCOL) {
                    continue;
                }
                let dev = route_oif(&msg)
                    .and_then(|i| names.get(&i))
                    .map(|(n, _, _)| n.clone())
                    .unwrap_or_default();
                routes.push((
                    RouteSpec {
                        v6,
                        table: route_table(&msg),
                        dev,
                        gateway: route_gateway(&msg),
                    },
                    msg,
                ));
            }
        }
        Ok((rules, routes))
    }

    /// The routers that have their address.
    async fn router_addresses(
        &self,
        names: &BTreeMap<u32, (String, bool, bool)>,
    ) -> SysResult<BTreeSet<String>> {
        let _dump = self.dump_guard().await;
        let mut stream = self.handle().address().get().execute();
        let mut out = BTreeSet::new();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| SysError::new("list addresses", e))?
        {
            let Some((name, _, _)) = names.get(&msg.header.index) else {
                continue;
            };
            let is_router_address = msg.attributes.iter().any(|a| {
                matches!(a, AddressAttribute::Local(IpAddr::V4(ip)) | AddressAttribute::Address(IpAddr::V4(ip)) if *ip == ROUTER_ADDRESS)
            });
            if is_breakout_name(name) && is_router_address && msg.header.prefix_len == 32 {
                out.insert(name.clone());
            }
        }
        Ok(out)
    }

    /// True if the static entry of `fdb` is on its port.
    async fn has_static_fdb(&self, port: u32, fdb: &StaticFdb) -> SysResult<bool> {
        let mut msg = NeighbourMessage::default();
        msg.header.family = AddressFamily::Bridge;
        msg.header.ifindex = port;
        msg.header.flags = NeighbourFlags::Controller;
        msg.attributes
            .push(NeighbourAttribute::LinkLayerAddress(fdb.mac.to_vec()));
        msg.attributes.push(NeighbourAttribute::Vlan(fdb.vid));
        let mut req = NetlinkMessage::from(RouteNetlinkMessage::GetNeighbour(msg));
        req.header.flags = NLM_F_REQUEST;
        let mut handle = self.handle().clone();
        let mut responses = handle
            .request(req)
            .map_err(|e| SysError::new(format!("read the forwarding entry on {}", fdb.port), e))?;
        let mut found = false;
        while let Some(resp) = responses.next().await {
            match resp.payload {
                NetlinkPayload::Error(err) if err.code.is_some() => return Ok(false),
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(m)) => {
                    found = m.header.ifindex == port
                        && matches!(
                            m.header.state,
                            NeighbourState::Noarp | NeighbourState::Permanent
                        );
                }
                _ => {}
            }
        }
        Ok(found)
    }

    async fn add_static_fdb(&self, port: u32, fdb: &StaticFdb) -> SysResult<()> {
        let mut msg = NeighbourMessage::default();
        msg.header.family = AddressFamily::Bridge;
        msg.header.ifindex = port;
        msg.header.flags = NeighbourFlags::Controller;
        msg.header.state = NeighbourState::Noarp;
        msg.attributes
            .push(NeighbourAttribute::LinkLayerAddress(fdb.mac.to_vec()));
        msg.attributes.push(NeighbourAttribute::Vlan(fdb.vid));
        let mut req = NetlinkMessage::from(RouteNetlinkMessage::NewNeighbour(msg));
        req.header.flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE;
        let fail = |e| {
            SysError::new(
                format!(
                    "add the forwarding entry of {} on {} (VLAN {})",
                    crate::breakout::mac_text(&fdb.mac),
                    fdb.port,
                    fdb.vid
                ),
                e,
            )
        };
        let mut handle = self.handle().clone();
        let mut responses = handle.request(req).map_err(fail)?;
        while let Some(resp) = responses.next().await {
            if let NetlinkPayload::Error(err) = resp.payload
                && err.code.is_some()
            {
                return Err(fail(rtnetlink::Error::NetlinkError(err)));
            }
        }
        Ok(())
    }

    async fn add_rule(&self, rule: &RuleSpec) -> SysResult<()> {
        let mut req = self
            .handle()
            .rule()
            .add()
            .table_id(rule.table)
            .priority(RULE_PRIORITY)
            .action(RuleAction::ToTable);
        if let Some(mark) = rule.fwmark {
            req = req.fw_mark(mark);
        }
        if let Some(iif) = &rule.iif {
            req = req.input_interface(iif.clone());
        }
        req.message_mut()
            .attributes
            .push(RuleAttribute::Protocol(RouteProtocol::Other(RT_PROTOCOL)));
        let result = if rule.v6 {
            req.v6().execute().await
        } else {
            req.v4().execute().await
        };
        match result {
            Err(e) if code(&e) == Some(EEXIST) => Ok(()),
            r => r.map_err(|e| SysError::new(format!("add routing rule {rule:?}"), e)),
        }
    }

    async fn add_route(&self, route: &RouteSpec, dev: u32) -> SysResult<()> {
        let protocol = RouteProtocol::Other(RT_PROTOCOL);
        let msg = match (route.v6, route.gateway) {
            (false, gateway) => {
                let mut b = RouteMessageBuilder::<Ipv4Addr>::new()
                    .table_id(route.table)
                    .output_interface(dev)
                    .protocol(protocol);
                match gateway {
                    Some(IpAddr::V4(g)) => b = b.gateway(g),
                    _ => b = b.scope(RouteScope::Link),
                }
                b.build()
            }
            (true, gateway) => {
                let mut b = RouteMessageBuilder::<Ipv6Addr>::new()
                    .table_id(route.table)
                    .output_interface(dev)
                    .protocol(protocol);
                if let Some(IpAddr::V6(g)) = gateway {
                    b = b.gateway(g);
                }
                b.build()
            }
        };
        match self.handle().route().add(msg).replace().execute().await {
            Err(e) if code(&e) == Some(EEXIST) => Ok(()),
            r => r.map_err(|e| SysError::new(format!("add route {route:?}"), e)),
        }
    }

    async fn del_rule(&self, msg: RuleMessage) -> SysResult<()> {
        match self.handle().rule().del(msg).execute().await {
            Err(e) if code(&e) == Some(ENOENT) => Ok(()),
            r => r.map_err(|e| SysError::new("delete a routing rule of the breakout", e)),
        }
    }

    async fn del_route(&self, msg: RouteMessage) -> SysResult<()> {
        match self.handle().route().del(msg).execute().await {
            Err(e) if code(&e) == Some(ENOENT) || code(&e) == Some(3) => Ok(()),
            r => r.map_err(|e| SysError::new("delete a route of the breakout", e)),
        }
    }

    async fn add_router_address(&self, index: u32, name: &str) -> SysResult<()> {
        match self
            .handle()
            .address()
            .add(index, IpAddr::V4(ROUTER_ADDRESS), 32)
            .execute()
            .await
        {
            Err(e) if code(&e) == Some(EEXIST) => Ok(()),
            r => r.map_err(|e| SysError::new(format!("add {ROUTER_ADDRESS}/32 to {name}"), e)),
        }
    }
}

/// `nft -j list tables`: the hash of the breakout tables.
async fn nft_hash() -> SysResult<Option<String>> {
    let tables = nftables::helper::get_current_ruleset_raw_async(
        nftables::helper::DEFAULT_NFT,
        ["list", "tables"],
    )
    .await
    .map_err(|e| SysError::new("list nftables tables", e))?;
    Ok(parse_tables(&tables))
}

impl BreakoutOps for LinuxSystem {
    async fn uplinks(&self) -> SysResult<Uplinks> {
        self.net.uplinks().await
    }

    async fn breakout_state(&self, sysctls: &[String]) -> SysResult<BreakoutState> {
        let names = self.net.link_flags().await?;
        let (rules, routes) = self.net.ours(&names).await?;
        let mut fdb = BTreeSet::new();
        for (index, (name, _, _)) in &names {
            if !crate::breakout::is_port_name(name) {
                continue;
            }
            // The entry this port should have: the MAC and VLAN follow from
            // its name (segment and VLAN ID).
            if let Some(entry) = expected_fdb(name)
                && self.net.has_static_fdb(*index, &entry).await?
            {
                fdb.insert(entry);
            }
        }
        Ok(BreakoutState {
            nft: nft_hash().await?,
            rules: rules.into_iter().map(|(r, _)| r).collect(),
            routes: routes.into_iter().map(|(r, _)| r).collect(),
            addresses: self.net.router_addresses(&names).await?,
            fdb,
            sysctls: sysctls
                .iter()
                .filter_map(|p| self.sysctls.read(p).map(|v| (p.clone(), v)))
                .collect(),
            changed: self.sysctls.changed(),
        })
    }

    async fn set_breakout(&self, specs: &[BreakoutSpec]) -> SysResult<()> {
        let want = wanted(specs);
        // The sysctls first: a router forwards before frames reach it.
        if let Err(e) = self.sysctls.apply(&want.sysctls) {
            // Nothing goes to a router that cannot forward.
            let _ = apply_script("delete nftables tables of the breakout", &delete_script()).await;
            return Err(e);
        }
        let names = self.net.link_flags().await?;
        let index_of = |name: &str| -> SysResult<u32> {
            names
                .iter()
                .find(|(_, (n, _, _))| n == name)
                .map(|(i, _)| *i)
                .ok_or_else(|| {
                    SysError::new("set up the breakout", format!("interface {name} vanished"))
                })
        };
        for spec in specs {
            self.net
                .add_router_address(index_of(&spec.router)?, &spec.router)
                .await?;
        }
        for entry in &want.fdb {
            self.net
                .add_static_fdb(index_of(&entry.port)?, entry)
                .await?;
        }
        let (rules, routes) = self.net.ours(&names).await?;
        for (spec, msg) in routes {
            if !want.routes.contains(&spec) {
                self.net.del_route(msg).await?;
            }
        }
        for (spec, msg) in rules {
            if !want.rules.contains(&spec) {
                self.net.del_rule(msg).await?;
            }
        }
        for route in &want.routes {
            self.net.add_route(route, index_of(&route.dev)?).await?;
        }
        for rule in &want.rules {
            self.net.add_rule(rule).await?;
        }
        // The tables last.
        if specs.is_empty() {
            apply_script("delete nftables tables of the breakout", &delete_script()).await
        } else if nft_hash().await? != want.nft {
            apply_script(
                &format!("apply nftables tables {TABLE} (bridge, inet)"),
                &nft_script(specs),
            )
            .await
        } else {
            Ok(())
        }
    }

    async fn delete_breakout(&self) -> SysResult<()> {
        // Every step even if one fails (also one that reads): what is left
        // of the breakout must not stay after wanderlan.
        let mut errors = Vec::new();
        // The tables first: no frame goes to a router any more. Deleting
        // them is harmless without them.
        if !matches!(nft_hash().await, Ok(None))
            && let Err(e) =
                apply_script("delete nftables tables of the breakout", &delete_script()).await
        {
            errors.push(e.to_string());
        }
        let ours = match self.net.link_flags().await {
            Ok(names) => self.net.ours(&names).await,
            Err(e) => Err(e),
        };
        let (rules, routes) = ours.unwrap_or_else(|e| {
            errors.push(e.to_string());
            Default::default()
        });
        for (_, msg) in rules {
            if let Err(e) = self.net.del_rule(msg).await {
                errors.push(e.to_string());
            }
        }
        for (_, msg) in routes {
            if let Err(e) = self.net.del_route(msg).await {
                errors.push(e.to_string());
            }
        }
        if let Err(e) = self.sysctls.restore() {
            errors.push(e.to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(SysError::new("remove the breakout", errors.join("; ")))
        }
    }
}

/// The static forwarding entry a breakout port has (from its name).
fn expected_fdb(port: &str) -> Option<StaticFdb> {
    let rest = port.strip_prefix(crate::breakout::PORT_PREFIX)?;
    let (vid, segment) = match rest.split_once('s') {
        Some((vid, segment)) => (vid.parse().ok()?, segment.parse().ok()?),
        None => (rest.parse().ok()?, 0),
    };
    Some(StaticFdb {
        port: port.to_string(),
        mac: crate::breakout::mac(segment, vid),
        vid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_of_a_port_follows_from_its_name() {
        let e = expected_fdb("wlbp10").unwrap();
        assert_eq!((e.vid, e.mac), (10, crate::breakout::mac(0, 10)));
        let e = expected_fdb("wlbp4094s3").unwrap();
        assert_eq!((e.vid, e.mac), (4094, crate::breakout::mac(3, 4094)));
        assert!(expected_fdb("wlbo10").is_none());
    }
}
