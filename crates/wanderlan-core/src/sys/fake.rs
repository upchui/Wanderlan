//! In-memory model of the kernel networking state for tests.
//!
//! It mirrors the kernel behaviour wanderlan relies on: ports get the
//! bridge's default PVID when attached, deleting a bridge detaches its ports,
//! VLAN entries only exist on bridge ports, only one PVID per port.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

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
