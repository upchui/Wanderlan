//! In-memory model of the kernel networking state for tests.
//!
//! It mirrors the kernel behaviour wanderlan relies on: ports get the
//! bridge's default PVID when attached, deleting a bridge detaches its ports,
//! VLAN entries only exist on bridge ports, only one PVID per port.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use std::collections::BTreeSet;

use super::nft::{MssRule, NftOps, NftState, SplitRule};
use super::observe::{FdbEntry, FdbEvent, ObserveOps};
use super::tc::{IfTc, TcOps, TcRule};
use super::{
    BridgeOptions, GretapParams, LinkKind, LinkState, LinkUpdate, NetOps, PortVlans, SysError,
    SysResult, VlanFlags,
};

/// Mutable state of the fake kernel.
#[derive(Debug, Default, Clone)]
pub struct FakeState {
    /// Links by index.
    pub links: BTreeMap<u32, LinkState>,
    /// VLAN entries by index.
    pub vlans: BTreeMap<u32, PortVlans>,
    /// tc state by index.
    pub tc: BTreeMap<u32, IfTc>,
    /// MSS rules; `None` if the nftables table does not exist.
    pub mss: Option<BTreeSet<MssRule>>,
    /// Split horizon rules (in the same table).
    pub split: BTreeSet<SplitRule>,
    /// Bridge FDB entries.
    pub fdb: Vec<FdbEntry>,
    /// Log of all mutating calls, e.g. `create_bridge br0`.
    pub calls: Vec<String>,
    next_index: u32,
}

/// A fake implementation of [`NetOps`].
#[derive(Debug, Default)]
pub struct FakeNet {
    state: Mutex<FakeState>,
}

fn err(op: &str, msg: &str) -> SysError {
    SysError::new(op.to_string(), msg.to_string())
}

impl FakeNet {
    /// Creates an empty fake kernel.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds a physical interface (e.g. the trunk), administratively down.
    pub fn add_physical(&self, name: &str) -> u32 {
        let mut s = self.lock();
        let index = s.alloc();
        s.links.insert(
            index,
            LinkState {
                index,
                name: name.to_string(),
                kind: LinkKind::Other(String::new()),
                alias: None,
                master: None,
                admin_up: false,
                oper_up: false,
                mtu: 1500,
                isolated: None,
                traffic: crate::api::Traffic::default(),
            },
        );
        index
    }

    /// Inserts an arbitrary link (for "foreign interface" scenarios).
    pub fn insert_link(&self, mut link: LinkState) -> u32 {
        let mut s = self.lock();
        let index = s.alloc();
        link.index = index;
        s.links.insert(index, link);
        index
    }

    /// Returns a copy of the current state.
    pub fn state(&self) -> FakeState {
        self.lock().clone()
    }

    /// Returns and clears the call log.
    pub fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().calls)
    }

    /// Looks up a link by name.
    pub fn link(&self, name: &str) -> Option<LinkState> {
        self.lock().links.values().find(|l| l.name == name).cloned()
    }

    /// tc state of a link by name.
    pub fn tc(&self, name: &str) -> IfTc {
        let s = self.lock();
        s.links
            .values()
            .find(|l| l.name == name)
            .and_then(|l| s.tc.get(&l.index).cloned())
            .unwrap_or_default()
    }

    /// Adds a foreign filter (not created by wanderlan) to a link.
    pub fn add_foreign_filter(&self, name: &str) {
        let mut s = self.lock();
        if let Some(index) = s.links.values().find(|l| l.name == name).map(|l| l.index) {
            let entry = s.tc.entry(index).or_default();
            entry.clsact = true;
            entry.foreign += 1;
        }
    }

    /// VLAN entries of a link by name.
    pub fn vlans(&self, name: &str) -> PortVlans {
        let s = self.lock();
        s.links
            .values()
            .find(|l| l.name == name)
            .and_then(|l| s.vlans.get(&l.index).cloned())
            .unwrap_or_default()
    }
}

impl FakeState {
    fn alloc(&mut self) -> u32 {
        self.next_index += 1;
        self.next_index
    }

    fn name_taken(&self, name: &str) -> bool {
        self.links.values().any(|l| l.name == name)
    }

    fn detach(&mut self, index: u32) {
        if let Some(link) = self.links.get_mut(&index) {
            link.master = None;
            link.isolated = None;
        }
        self.vlans.remove(&index);
    }
}

impl NetOps for FakeNet {
    async fn links(&self) -> SysResult<Vec<LinkState>> {
        Ok(self.lock().links.values().cloned().collect())
    }

    async fn port_vlans(&self) -> SysResult<BTreeMap<u32, PortVlans>> {
        Ok(self
            .lock()
            .vlans
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    async fn create_bridge(&self, name: &str, options: &BridgeOptions) -> SysResult<()> {
        let mut s = self.lock();
        s.calls.push(format!("create_bridge {name}"));
        if s.name_taken(name) {
            return Err(err("create bridge", "File exists"));
        }
        let index = s.alloc();
        s.links.insert(
            index,
            LinkState {
                index,
                name: name.to_string(),
                kind: LinkKind::Bridge(*options),
                alias: None,
                master: None,
                admin_up: false,
                oper_up: false,
                mtu: 1500,
                isolated: None,
                traffic: crate::api::Traffic::default(),
            },
        );
        Ok(())
    }

    async fn set_bridge_options(
        &self,
        index: u32,
        name: &str,
        options: &BridgeOptions,
    ) -> SysResult<()> {
        let mut s = self.lock();
        s.calls.push(format!("set_bridge_options {name}"));
        match s.links.get_mut(&index) {
            Some(link) if matches!(link.kind, LinkKind::Bridge(_)) => {
                link.kind = LinkKind::Bridge(*options);
                Ok(())
            }
            _ => Err(err("set bridge options", "not a bridge")),
        }
    }

    async fn create_gretap(&self, name: &str, params: &GretapParams, mtu: u32) -> SysResult<()> {
        let mut s = self.lock();
        s.calls.push(format!(
            "create_gretap {name} {}->{}",
            params.local, params.remote
        ));
        if s.name_taken(name) {
            return Err(err("create gretap", "File exists"));
        }
        let index = s.alloc();
        s.links.insert(
            index,
            LinkState {
                index,
                name: name.to_string(),
                kind: LinkKind::Gretap(*params),
                alias: None,
                master: None,
                admin_up: false,
                oper_up: false,
                mtu,
                isolated: None,
                traffic: crate::api::Traffic::default(),
            },
        );
        Ok(())
    }

    async fn delete_link(&self, index: u32) -> SysResult<()> {
        let mut s = self.lock();
        let Some(link) = s.links.remove(&index) else {
            return Err(err("delete link", "No such device"));
        };
        s.calls.push(format!("delete_link {}", link.name));
        s.vlans.remove(&index);
        s.tc.remove(&index);
        let ports: Vec<u32> = s
            .links
            .values()
            .filter(|l| l.master == Some(index))
            .map(|l| l.index)
            .collect();
        for port in ports {
            s.detach(port);
        }
        Ok(())
    }

    async fn update_link(&self, index: u32, update: &LinkUpdate) -> SysResult<()> {
        let mut s = self.lock();
        let name = s
            .links
            .get(&index)
            .map(|l| l.name.clone())
            .ok_or_else(|| err("update link", "No such device"))?;
        s.calls.push(format!("update_link {name} {update:?}"));
        if let Some(master) = update.master {
            match master {
                None => s.detach(index),
                Some(bridge) => {
                    // Like the kernel: moving a port to another bridge
                    // first removes it (and its VLANs) from the old one.
                    if s.links
                        .get(&index)
                        .and_then(|l| l.master)
                        .is_some_and(|m| m != bridge)
                    {
                        s.detach(index);
                    }
                    let pvid = match s.links.get(&bridge).map(|l| &l.kind) {
                        Some(LinkKind::Bridge(o)) => o.default_pvid,
                        _ => return Err(err("attach", "master is not a bridge")),
                    };
                    if let Some(link) = s.links.get_mut(&index) {
                        link.master = Some(bridge);
                        link.isolated = Some(false);
                    }
                    if pvid != 0 {
                        s.vlans
                            .entry(index)
                            .or_default()
                            .insert(pvid, VlanFlags::NATIVE);
                    }
                }
            }
        }
        let link = s
            .links
            .get_mut(&index)
            .ok_or_else(|| err("update link", "No such device"))?;
        if let Some(up) = update.up {
            link.admin_up = up;
            link.oper_up = up;
        }
        if let Some(mtu) = update.mtu {
            link.mtu = mtu;
        }
        if let Some(alias) = &update.alias {
            link.alias = Some(alias.clone());
        }
        Ok(())
    }

    async fn set_port_isolated(&self, index: u32, isolated: bool) -> SysResult<()> {
        let mut s = self.lock();
        let link = s
            .links
            .get_mut(&index)
            .ok_or_else(|| err("set isolated", "No such device"))?;
        if link.master.is_none() {
            return Err(err("set isolated", "not a bridge port"));
        }
        link.isolated = Some(isolated);
        let name = link.name.clone();
        s.calls.push(format!("set_port_isolated {name} {isolated}"));
        Ok(())
    }

    async fn add_vlans(
        &self,
        index: u32,
        bridge_self: bool,
        vlans: &[(u16, VlanFlags)],
    ) -> SysResult<()> {
        let mut s = self.lock();
        let link = s
            .links
            .get(&index)
            .ok_or_else(|| err("add vlans", "No such device"))?;
        let is_bridge = matches!(link.kind, LinkKind::Bridge(_));
        if bridge_self != is_bridge || (!is_bridge && link.master.is_none()) {
            return Err(err("add vlans", "Operation not supported"));
        }
        let name = link.name.clone();
        s.calls.push(format!("add_vlans {name} {}", vlans.len()));
        let entries = s.vlans.entry(index).or_default();
        for (vid, flags) in vlans {
            if flags.pvid {
                for f in entries.values_mut() {
                    f.pvid = false;
                }
            }
            entries.insert(*vid, *flags);
        }
        Ok(())
    }

    async fn flush_fdb(&self, _bridge: u32, name: &str) -> SysResult<()> {
        self.lock().calls.push(format!("flush_fdb {name}"));
        Ok(())
    }

    async fn del_vlans(&self, index: u32, _bridge_self: bool, vids: &[u16]) -> SysResult<()> {
        let mut s = self.lock();
        let name = s
            .links
            .get(&index)
            .map(|l| l.name.clone())
            .ok_or_else(|| err("del vlans", "No such device"))?;
        s.calls.push(format!("del_vlans {name} {}", vids.len()));
        if let Some(entries) = s.vlans.get_mut(&index) {
            for vid in vids {
                entries.remove(vid);
            }
        }
        Ok(())
    }
}

impl TcOps for FakeNet {
    async fn tc_state(&self) -> SysResult<BTreeMap<u32, IfTc>> {
        Ok(self
            .lock()
            .tc
            .iter()
            .filter(|(_, t)| t.clsact)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    async fn add_clsact(&self, index: u32) -> SysResult<()> {
        let mut s = self.lock();
        s.calls.push(format!("add_clsact #{index}"));
        let entry = s.tc.entry(index).or_default();
        if entry.clsact {
            return Err(err("add clsact", "File exists"));
        }
        entry.clsact = true;
        Ok(())
    }

    async fn del_clsact(&self, index: u32) -> SysResult<()> {
        let mut s = self.lock();
        s.calls.push(format!("del_clsact #{index}"));
        match s.tc.remove(&index) {
            Some(t) if t.clsact => Ok(()),
            _ => Err(err("del clsact", "No such file or directory")),
        }
    }

    async fn add_tc_rule(&self, index: u32, rule: &TcRule) -> SysResult<()> {
        let mut s = self.lock();
        s.calls
            .push(format!("add_tc_rule #{index} {}", rule.cookie()));
        let entry = s.tc.entry(index).or_default();
        if !entry.clsact {
            return Err(err("add tc rule", "no clsact qdisc"));
        }
        if entry
            .ours
            .iter()
            .any(|r| r.direction == rule.direction && r.prio() == rule.prio())
        {
            return Err(err("add tc rule", "File exists"));
        }
        entry.ours.push(*rule);
        Ok(())
    }

    async fn del_tc_rule(&self, index: u32, rule: &TcRule) -> SysResult<()> {
        let mut s = self.lock();
        s.calls
            .push(format!("del_tc_rule #{index} {}", rule.cookie()));
        let entry = s.tc.entry(index).or_default();
        let before = entry.ours.len();
        entry
            .ours
            .retain(|r| !(r.direction == rule.direction && r.prio() == rule.prio()));
        if entry.ours.len() == before {
            return Err(err("del tc rule", "No such file or directory"));
        }
        Ok(())
    }
}

impl NftOps for FakeNet {
    async fn nft_state(&self) -> SysResult<Option<NftState>> {
        let s = self.lock();
        Ok(s.mss.as_ref().map(|mss| NftState::of(mss, &s.split)))
    }

    async fn set_nft_rules(
        &self,
        rules: &BTreeSet<MssRule>,
        split: &BTreeSet<SplitRule>,
    ) -> SysResult<()> {
        let mut s = self.lock();
        s.calls
            .push(format!("set_nft_rules {} {}", rules.len(), split.len()));
        s.mss = Some(rules.clone());
        s.split = split.clone();
        Ok(())
    }

    async fn delete_table(&self) -> SysResult<()> {
        let mut s = self.lock();
        s.calls.push("delete_table".into());
        s.split.clear();
        if s.mss.take().is_none() {
            return Err(err("delete table", "No such file or directory"));
        }
        Ok(())
    }
}

impl ObserveOps for FakeNet {
    async fn fdb(&self) -> SysResult<Vec<FdbEntry>> {
        Ok(self.lock().fdb.clone())
    }

    fn watch_fdb(&self) -> SysResult<tokio::sync::mpsc::UnboundedReceiver<FdbEvent>> {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(rx)
    }
}
