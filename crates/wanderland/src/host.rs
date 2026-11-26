//! The box: what all its networks want, applied by one reconcile.
//!
//! Every network (a [`crate::daemon`] controller) tells the host what it
//! wants; the host puts the wishes together ([`wanderlan_core::merge`])
//! and is the only one that changes the kernel (one at a time). Until every
//! network has said what it wants once, nothing is changed: a reconcile
//! with the wishes of only some networks would remove what the others built
//! before a restart.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use wanderlan_core::desired::Desired;
use wanderlan_core::merge;
use wanderlan_core::reconcile::{self, Change, ReconcileError};
use wanderlan_core::sys::SysError;
use wanderlan_core::sys::linux::LinuxSystem;

/// How long one reconcile may take (a hanging kernel or nft call must not
/// block the daemon, and with it the API and the rollback deadlines).
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(120);

/// What one network gets from a reconcile.
#[derive(Debug, Default)]
pub struct Applied {
    /// The changes made (also those for other networks).
    pub changes: Vec<Change>,
    /// The bridges of the network's segments on the box.
    pub bridges: Vec<String>,
    /// What the network lost to earlier ones.
    pub conflicts: Vec<String>,
}

/// The box.
pub struct Host {
    sys: LinuxSystem,
    state: Mutex<State>,
    /// The ports each network takes (for the sockets of the others, read
    /// without waiting for a reconcile).
    ports: std::sync::Mutex<BTreeMap<String, Vec<u16>>>,
}

struct State {
    /// The networks in their order: the earlier one wins a conflict.
    order: Vec<String>,
    /// What each network wants, once it said so (`None`: nothing, it is
    /// not configured).
    wants: BTreeMap<String, Option<Arc<Desired>>>,
}

impl Host {
    /// The box with `networks`, in their order.
    pub fn new(sys: LinuxSystem, networks: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            sys,
            state: Mutex::new(State {
                order: networks,
                wants: BTreeMap::new(),
            }),
            ports: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    /// Adds network `network` (last). Until it said what it wants, nothing
    /// is changed.
    pub async fn add_network(&self, network: &str) {
        let mut state = self.state.lock().await;
        if !state.order.iter().any(|n| n == network) {
            state.order.push(network.to_string());
        }
    }

    /// Forgets network `network` (after its reset: nothing of it is left).
    pub async fn remove_network(&self, network: &str) {
        let mut state = self.state.lock().await;
        state.order.retain(|n| n != network);
        state.wants.remove(network);
        self.ports_lock().remove(network);
    }

    fn ports_lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u16>>> {
        self.ports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ports the other networks of the box take.
    pub fn ports_of_others(&self, network: &str) -> std::collections::BTreeSet<u16> {
        self.ports_lock()
            .iter()
            .filter(|(n, _)| *n != network)
            .flat_map(|(_, p)| p.iter().copied())
            .collect()
    }

    /// What network `network` would lose to the earlier networks with
    /// `desired` (a change is checked before it is applied).
    pub async fn check(&self, network: &str, desired: &Desired) -> Vec<String> {
        let state = self.state.lock().await;
        let parts: Vec<(&str, &Desired)> = state
            .order
            .iter()
            .filter_map(|n| {
                if n == network {
                    return Some((n.as_str(), desired));
                }
                Some((n.as_str(), state.wants.get(n)?.as_deref()?))
            })
            .collect();
        merge::merge(&parts)
            .and_then(|m| m.conflicts.get(network).cloned())
            .unwrap_or_default()
    }

    /// Network `network` wants `desired` (`None`: nothing). Applies what
    /// the box should look like once every network said what it wants;
    /// `on_change` is called before each change.
    pub async fn reconcile(
        &self,
        network: &str,
        desired: Option<Arc<Desired>>,
        on_change: impl Fn(&Change) + Send + Sync,
    ) -> Result<Applied, ReconcileError> {
        let mut state = self.state.lock().await;
        state.wants.insert(network.to_string(), desired.clone());
        {
            let mut ports = self.ports_lock();
            match &desired {
                Some(d) => {
                    ports.insert(network.to_string(), merge::ports(d).collect());
                }
                None => {
                    ports.remove(network);
                }
            }
        }
        if state.order.iter().any(|n| !state.wants.contains_key(n)) {
            return Ok(Applied::default());
        }
        let parts: Vec<(&str, &Desired)> = state
            .order
            .iter()
            .filter_map(|n| Some((n.as_str(), state.wants.get(n)?.as_deref()?)))
            .collect();
        // No network is configured: nothing is changed (as by a daemon
        // without configuration).
        let Some(merged) = merge::merge(&parts) else {
            return Ok(Applied::default());
        };
        let changes = tokio::time::timeout(
            RECONCILE_TIMEOUT,
            reconcile::reconcile(&self.sys, &merged.desired, on_change),
        )
        .await
        .map_err(|_| {
            ReconcileError::Sys(SysError::new(
                "apply the configuration",
                format!("no answer within {} s", RECONCILE_TIMEOUT.as_secs()),
            ))
        })??;
        Ok(Applied {
            changes,
            bridges: desired
                .as_deref()
                .map(|d| bridges_of(d, &merged.desired))
                .unwrap_or_default(),
            conflicts: merged.conflicts.get(network).cloned().unwrap_or_default(),
        })
    }

    /// The networks of the box, in their order.
    pub async fn networks(&self) -> Vec<String> {
        self.state.lock().await.order.clone()
    }

    /// Removes everything wanderlan made on the box (at the end of the
    /// daemon), at most `limit` long.
    pub async fn teardown(&self, limit: Duration) -> anyhow::Result<Vec<Change>> {
        let mut state = self.state.lock().await;
        state.wants.clear();
        tokio::time::timeout(
            limit,
            reconcile::teardown(&self.sys, |c| tracing::info!("{c}")),
        )
        .await
        .map_err(|_| anyhow::anyhow!("did not finish within {} s", limit.as_secs()))?
        .map_err(|e| anyhow::anyhow!("cleanup failed: {e}"))
    }

    /// True if another network than `network` is configured.
    pub async fn others_configured(&self, network: &str) -> bool {
        let state = self.state.lock().await;
        state.wants.iter().any(|(n, d)| n != network && d.is_some())
    }

    /// Removes everything of a network that is reset: with other networks
    /// configured, only what is its own; as the only one, everything.
    pub async fn reset(
        &self,
        network: &str,
        on_change: impl Fn(&Change) + Send + Sync,
    ) -> Result<Vec<Change>, ReconcileError> {
        if self.others_configured(network).await {
            return self
                .reconcile(network, None, on_change)
                .await
                .map(|a| a.changes);
        }
        let mut state = self.state.lock().await;
        state.wants.insert(network.to_string(), None);
        reconcile::teardown(&self.sys, on_change).await
    }
}

/// The bridges the segments of `own` (one network) are in on the box.
fn bridges_of(own: &Desired, merged: &Desired) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let trunks = own.trunks.iter().map(|t| t.name.as_str());
    for name in trunks {
        if let Some(t) = merged.trunks.iter().find(|t| t.name == name)
            && !out.contains(&t.bridge)
        {
            out.push(t.bridge.clone());
        }
    }
    out
}
