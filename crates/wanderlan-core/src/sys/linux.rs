//! The real system: rtnetlink for links, bridge and tc, `nft` for nftables.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddrV4;

use super::netlink::Netlink;
use super::nft::{CachedNft, MssRule, NftCli, NftOps, NftState, SplitRule};
use super::nft_ip::{LearnCli, LearnOps, LearnRule, LearnState, Learned};

/// How long the MSS rules read from nftables are trusted.
const NFT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(120);
use super::tc::{IfTc, TcOps, TcRule};
use super::{
    BridgeOptions, LinkState, LinkUpdate, NetOps, PortVlans, SysResult, VlanFlags, VxlanParams,
};

/// [`super::System`] implementation for Linux.
#[derive(Debug, Clone)]
pub struct LinuxSystem {
    /// rtnetlink backend.
    pub net: Netlink,
    /// nftables backend (reads of the MSS rules are cached).
    pub nft: CachedNft<NftCli>,
    /// Where the clients are (table `ip wanderlan`).
    pub learn: LearnCli,
}

impl LinuxSystem {
    /// Opens the netlink connection (inside a tokio runtime).
    pub fn connect() -> SysResult<Self> {
        Ok(Self {
            net: Netlink::connect()?,
            nft: CachedNft::new(NftCli, NFT_CACHE_TTL),
            learn: LearnCli::default(),
        })
    }
}

impl NetOps for LinuxSystem {
    async fn links(&self) -> SysResult<Vec<LinkState>> {
        self.net.links().await
    }
    async fn port_vlans(&self) -> SysResult<BTreeMap<u32, PortVlans>> {
        self.net.port_vlans().await
    }
    fn vlan_stats_supported(&self) -> bool {
        self.net.vlan_stats_supported()
    }
    async fn create_bridge(&self, name: &str, options: &BridgeOptions) -> SysResult<()> {
        self.net.create_bridge(name, options).await
    }
    async fn set_bridge_options(
        &self,
        index: u32,
        name: &str,
        options: &BridgeOptions,
    ) -> SysResult<()> {
        self.net.set_bridge_options(index, name, options).await
    }
    async fn create_vxlan(&self, name: &str, params: &VxlanParams, mtu: u32) -> SysResult<()> {
        self.net.create_vxlan(name, params, mtu).await
    }
    async fn tunnel_remote(
        &self,
        index: u32,
    ) -> SysResult<Option<(std::net::Ipv4Addr, Option<u16>)>> {
        self.net.tunnel_remote(index).await
    }
    async fn set_tunnel_remote(
        &self,
        index: u32,
        name: &str,
        own_port: u16,
        remote: Option<SocketAddrV4>,
    ) -> SysResult<()> {
        self.net
            .set_tunnel_remote(index, name, own_port, remote)
            .await
    }
    async fn delete_link(&self, index: u32) -> SysResult<()> {
        self.net.delete_link(index).await
    }
    async fn update_link(&self, index: u32, update: &LinkUpdate) -> SysResult<()> {
        self.net.update_link(index, update).await
    }
    async fn set_port_isolated(&self, index: u32, isolated: bool) -> SysResult<()> {
        self.net.set_port_isolated(index, isolated).await
    }
    async fn add_vlans(
        &self,
        index: u32,
        bridge_self: bool,
        vlans: &[(u16, VlanFlags)],
    ) -> SysResult<()> {
        self.net.add_vlans(index, bridge_self, vlans).await
    }
    async fn flush_fdb(&self, bridge: u32, name: &str) -> SysResult<()> {
        self.net.flush_fdb(bridge, name).await
    }
    async fn del_vlans(&self, index: u32, bridge_self: bool, vids: &[u16]) -> SysResult<()> {
        self.net.del_vlans(index, bridge_self, vids).await
    }
}

impl TcOps for LinuxSystem {
    async fn tc_state(&self) -> SysResult<BTreeMap<u32, IfTc>> {
        self.net.tc_state().await
    }
    async fn add_clsact(&self, index: u32) -> SysResult<()> {
        self.net.add_clsact(index).await
    }
    async fn del_clsact(&self, index: u32) -> SysResult<()> {
        self.net.del_clsact(index).await
    }
    async fn add_tc_rule(&self, index: u32, rule: &TcRule) -> SysResult<()> {
        self.net.add_tc_rule(index, rule).await
    }
    async fn del_tc_rule(&self, index: u32, rule: &TcRule) -> SysResult<()> {
        self.net.del_tc_rule(index, rule).await
    }
}

impl NftOps for LinuxSystem {
    async fn nft_state(&self) -> SysResult<Option<NftState>> {
        self.nft.nft_state().await
    }
    async fn set_nft_rules(
        &self,
        rules: &BTreeSet<MssRule>,
        split: &BTreeSet<SplitRule>,
    ) -> SysResult<()> {
        self.nft.set_nft_rules(rules, split).await
    }
    async fn delete_table(&self) -> SysResult<()> {
        self.nft.delete_table().await
    }
}

impl LearnOps for LinuxSystem {
    async fn learn_state(&self) -> SysResult<Option<LearnState>> {
        self.learn.learn_state().await
    }
    async fn set_learn(
        &self,
        port: u16,
        rules: &BTreeSet<LearnRule>,
        notrack: bool,
        fixed: &BTreeSet<u32>,
    ) -> SysResult<()> {
        self.learn.set_learn(port, rules, notrack, fixed).await
    }
    async fn delete_learn(&self) -> SysResult<()> {
        self.learn.delete_learn().await
    }
    async fn learned(&self) -> SysResult<BTreeMap<String, Vec<Learned>>> {
        self.learn.learned().await
    }
}
