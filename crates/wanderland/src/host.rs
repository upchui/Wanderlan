//! The box: what all its networks want, applied by one reconcile.
//!
//! Every network (a [`crate::daemon`] controller) tells the host what it
//! wants; the host puts the wishes together ([`wanderlan_core::merge`])
//! and is the only one that changes the kernel (one at a time). Until every
//! network has said what it wants once, nothing is changed: a reconcile
//! with the wishes of only some networks would remove what the others built
//! before a restart. When the box changed, every network hears it
//! ([`Host::subscribe`]): a network that only waited reconciles again for
//! what it does after a change (its loop guard, its keepalives).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, watch};
use wanderlan_core::desired::Desired;
use wanderlan_core::merge::{self, Merged};
use wanderlan_core::reconcile::{self, Change, ReconcileError};
use wanderlan_core::sys::SysError;
use wanderlan_core::sys::linux::LinuxSystem;

/// How long one reconcile may take (a hanging kernel or nft call must not
/// block the daemon, and with it the API and the rollback deadlines).
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(120);

/// What one network gets from a reconcile.
#[derive(Debug)]
pub struct Applied {
    /// The changes made (also those for other networks).
    pub changes: Vec<Change>,
    /// The bridges of the network's segments on the box.
    pub bridges: Vec<String>,
    /// What the network lost to earlier ones.
    pub conflicts: Vec<String>,
    /// The network has its ports (else another network of the box has one
    /// of them, and it got nothing: its sockets stay closed).
    pub ports: bool,
    /// Nothing was done: the box waits for another network.
    pub waiting: bool,
}

impl Applied {
    fn waiting() -> Self {
        Self {
            changes: Vec::new(),
            bridges: Vec::new(),
            conflicts: Vec::new(),
            ports: true,
            waiting: true,
        }
    }
}

/// The box.
pub struct Host {
    sys: LinuxSystem,
    state: Mutex<State>,
    /// The networks in their order: the earlier one wins a conflict (read
    /// without waiting for a reconcile).
    order: std::sync::RwLock<Vec<String>>,
    /// The ports each network takes (for the sockets of the others, read
    /// without waiting for a reconcile).
    ports: std::sync::Mutex<BTreeMap<String, Vec<u16>>>,
    /// The trunk ports of each network (no uplink of a breakout).
    trunks: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
    /// Counts the reconciles that changed the box.
    applied: watch::Sender<u64>,
}

struct State {
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
                wants: BTreeMap::new(),
            }),
            order: std::sync::RwLock::new(networks),
            ports: std::sync::Mutex::new(BTreeMap::new()),
            trunks: std::sync::Mutex::new(BTreeMap::new()),
            applied: watch::Sender::new(0),
        })
    }

    /// Tells when the box was changed (by any network).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.applied.subscribe()
    }

    fn order(&self) -> Vec<String> {
        self.order
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Adds network `network` (last). Until it said what it wants, nothing
    /// is changed.
    pub async fn add_network(&self, network: &str) {
        let _state = self.state.lock().await;
        let mut order = self
            .order
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !order.iter().any(|n| n == network) {
            order.push(network.to_string());
        }
    }

    /// Forgets network `network` (after its reset: nothing of it is left).
    pub async fn remove_network(&self, network: &str) {
        let mut state = self.state.lock().await;
        self.order
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|n| n != network);
        state.wants.remove(network);
        self.ports_lock().remove(network);
    }

    fn ports_lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u16>>> {
        self.ports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The trunk ports of every network of the box.
    pub fn trunk_ports(&self) -> BTreeSet<String> {
        self.trunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    /// The ports the other networks of the box take.
    pub fn ports_of_others(&self, network: &str) -> BTreeSet<u16> {
        self.ports_lock()
            .iter()
            .filter(|(n, _)| *n != network)
            .flat_map(|(_, p)| p.iter().copied())
            .collect()
    }

    /// The box as the networks in `wants` want it (`replace`: network,
    /// what it would want instead).
    fn merged(
        &self,
        wants: &BTreeMap<String, Option<Arc<Desired>>>,
        replace: Option<(&str, &Desired)>,
    ) -> Option<Merged> {
        let order = self.order();
        let parts: Vec<(&str, &Desired)> = order
            .iter()
            .filter_map(|n| match replace {
                Some((r, desired)) if r == n => Some((n.as_str(), desired)),
                _ => Some((n.as_str(), wants.get(n)?.as_deref()?)),
            })
            .collect();
        merge::merge(&parts)
    }

    /// What `desired` of network `network` would take from any network of
    /// the box that it does not lose now (a change is checked before it is
    /// applied), in sentences.
    pub async fn check(&self, network: &str, desired: &Desired) -> Vec<String> {
        let state = self.state.lock().await;
        let before = self.merged(&state.wants, None);
        let Some(after) = self.merged(&state.wants, Some((network, desired))) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (name, lost) in &after.lost {
            let had: BTreeSet<_> = before
                .as_ref()
                .and_then(|b| b.lost.get(name))
                .map(|l| l.iter().map(merge::Lost::what).collect())
                .unwrap_or_default();
            let new: Vec<merge::Lost> = lost
                .iter()
                .filter(|l| !had.contains(&l.what()))
                .cloned()
                .collect();
            for line in merge::describe(&new) {
                if name == network {
                    out.push(line);
                } else {
                    out.push(format!("network \"{name}\" would lose {line}"));
                }
            }
        }
        out
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
        self.note_ports(network, desired.as_deref());
        if self.order().iter().any(|n| !state.wants.contains_key(n)) {
            return Ok(Applied::waiting());
        }
        self.apply(&state.wants, network, desired.as_deref(), on_change)
            .await
    }

    fn note_ports(&self, network: &str, desired: Option<&Desired>) {
        {
            let mut trunks = self
                .trunks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match desired {
                Some(d) => {
                    trunks.insert(
                        network.to_string(),
                        d.trunks.iter().map(|t| t.name.clone()).collect(),
                    );
                }
                None => {
                    trunks.remove(network);
                }
            }
        }
        let mut ports = self.ports_lock();
        match desired {
            Some(d) => {
                ports.insert(network.to_string(), merge::ports(d).collect());
            }
            None => {
                ports.remove(network);
            }
        }
    }

    /// Reconciles the box to `wants`. A plan that cannot be reached because
    /// of a later network (its trunk port is in a bridge of someone else,
    /// say) is tried again without the later networks, one after the
    /// other: one network does not stop the others. `network` gets the
    /// error only if it is one that was left out.
    async fn apply(
        &self,
        wants: &BTreeMap<String, Option<Arc<Desired>>>,
        network: &str,
        own: Option<&Desired>,
        on_change: impl Fn(&Change) + Send + Sync,
    ) -> Result<Applied, ReconcileError> {
        let order = self.order();
        let mut wants = wants.clone();
        let mut left_out: Vec<String> = Vec::new();
        loop {
            // No network is configured: nothing is changed (as by a daemon
            // without configuration).
            let Some(merged) = self.merged(&wants, None) else {
                return Ok(Applied {
                    waiting: false,
                    ..Applied::waiting()
                });
            };
            let result = tokio::time::timeout(
                RECONCILE_TIMEOUT,
                reconcile::reconcile(&self.sys, &merged.desired, &on_change),
            )
            .await
            .map_err(|_| {
                ReconcileError::Sys(SysError::new(
                    "apply the configuration",
                    format!("no answer within {} s", RECONCILE_TIMEOUT.as_secs()),
                ))
            })
            .and_then(|r| r);
            let later = order
                .iter()
                .rev()
                .find(|n| wants.get(*n).is_some_and(Option::is_some) && Some(*n) != order.first())
                .cloned();
            match result {
                Ok(changes) => {
                    self.applied.send_modify(|g| *g += 1);
                    let left = left_out.iter().any(|n| n == network);
                    return Ok(Applied {
                        changes,
                        bridges: if own.is_some() && !left {
                            merged.bridges.get(network).cloned().unwrap_or_default()
                        } else {
                            Vec::new()
                        },
                        conflicts: merged.conflicts.get(network).cloned().unwrap_or_default(),
                        ports: merged.has_ports(network),
                        waiting: false,
                    });
                }
                // Only a plan that cannot be reached is tried without a
                // network (a failed system call may pass next time).
                Err(ReconcileError::Plan(e)) if later.is_some() => {
                    let later = later.unwrap_or_default();
                    tracing::warn!("Network \"{later}\" is left out on this box for now: {e}");
                    if later == network {
                        // Applied without it, then its error.
                        wants.insert(later.clone(), None);
                        left_out.push(later);
                        let without = Box::pin(self.apply_quiet(&wants, &on_change)).await;
                        if let Err(again) = without {
                            tracing::warn!("{again}");
                        }
                        return Err(ReconcileError::Plan(e));
                    }
                    wants.insert(later.clone(), None);
                    left_out.push(later);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Reconciles the box to `wants`, as far as it goes (for the networks
    /// that are not left out).
    async fn apply_quiet(
        &self,
        wants: &BTreeMap<String, Option<Arc<Desired>>>,
        on_change: impl Fn(&Change) + Send + Sync,
    ) -> Result<(), ReconcileError> {
        let Some(merged) = self.merged(wants, None) else {
            return Ok(());
        };
        tokio::time::timeout(
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
        self.applied.send_modify(|g| *g += 1);
        Ok(())
    }

    /// The networks of the box, in their order.
    pub fn networks(&self) -> Vec<String> {
        self.order()
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

    /// Removes everything of network `network` (a reset, or a first
    /// configuration rolled back): with other networks configured, only
    /// what is its own (also before every network has said what it wants:
    /// what a network not heard yet had is built again when it does); as
    /// the only one, everything. Decided and done in one go.
    pub async fn reset(
        &self,
        network: &str,
        on_change: impl Fn(&Change) + Send + Sync,
    ) -> Result<Vec<Change>, ReconcileError> {
        let mut state = self.state.lock().await;
        state.wants.insert(network.to_string(), None);
        self.note_ports(network, None);
        let others = state.wants.iter().any(|(n, d)| n != network && d.is_some());
        let changes = if others {
            let applied = self.apply(&state.wants, network, None, on_change).await?;
            applied.changes
        } else {
            let changes = reconcile::teardown(&self.sys, on_change).await?;
            self.applied.send_modify(|g| *g += 1);
            changes
        };
        Ok(changes)
    }
}
