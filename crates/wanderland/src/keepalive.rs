//! Keepalives of the client tunnels: a client sends a small frame through
//! each of its tunnels inside UDP every [`nat::KEEPALIVE`], so that its NAT
//! keeps the mapping and its server learns where its tunnel packets come
//! from (see `wanderlan_core::nat`). The server drops the frame at the
//! tunnel port (untagged).

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use wanderlan_core::nat;

/// What to send where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keepalives {
    /// The tunnel interfaces (to the uplink, inside UDP).
    pub interfaces: Vec<String>,
    /// The fingerprint of this site's identity, in the frame.
    pub fingerprint: String,
    /// Changed to send at once (a tunnel just came up).
    pub kick: u64,
}

/// Starts the keepalive task.
pub fn spawn(config: watch::Receiver<Keepalives>) -> JoinHandle<()> {
    tokio::spawn(run(config))
}

async fn run(mut config: watch::Receiver<Keepalives>) {
    // Errors per interface, logged when they change.
    let mut failing: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let current = config.borrow_and_update().clone();
        let frame = nat::keepalive_frame(&current.fingerprint);
        failing.retain(|name, _| current.interfaces.contains(name));
        for name in &current.interfaces {
            match crate::rawframe::send(name, &frame) {
                Ok(()) => {
                    if failing.remove(name).is_some() {
                        debug!("Keepalives through {name} work again");
                    }
                }
                // A tunnel that is down or not created yet: nothing to
                // keep alive yet.
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENETDOWN | libc::ENODEV)) => {}
                Err(e) => {
                    let message = e.to_string();
                    if failing.get(name) != Some(&message) {
                        warn!("Cannot send a keepalive through {name}: {message}");
                        failing.insert(name.clone(), message);
                    }
                }
            }
        }
        // A little jitter, so that many clients do not send in step.
        let jitter = Duration::from_millis(u64::from(rand_jitter()) % 2000);
        tokio::select! {
            r = config.changed() => if r.is_err() { return },
            () = tokio::time::sleep(nat::KEEPALIVE.saturating_sub(jitter)) => {}
        }
    }
}

/// A few random bits for the jitter.
fn rand_jitter() -> u16 {
    crate::random::bytes::<2>().map_or(0, u16::from_be_bytes)
}
