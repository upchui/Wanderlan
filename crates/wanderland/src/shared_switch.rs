//! The switch check: is a trunk with a VLAN mapping in one switch network
//! with a peer?
//!
//! A site that maps VLAN V of the network to another VLAN on its trunk
//! drops V as it is there (see `desired::dropped_mappings`). Where the trunk
//! and the trunk of a peer are in one switch network, frames the peer puts
//! out in V still arrive at the trunk in V: the switch sees every device of
//! the mapped VLAN twice, at its own port in the mapped VLAN and at the
//! peer's port in V. Switch features that bind a device to one port (DHCP
//! snooping, port security) may then send its traffic to the wrong port.
//!
//! Every [`INTERVAL`] (and when the tunnels change) a marker frame in V goes
//! into every tunnel of the trunk's segment that carries V: the peer floods
//! it (its destination is never learned) out of its trunk. A tap on the
//! trunk looks for it there in V, before tc drops it. One marker seen means
//! shared; [`MISSES`] in a row without one mean no longer.

use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use wanderlan_core::sys::nft_ip::KEEPALIVE_ETHERTYPE;

use crate::daemon::Input;
use crate::rawframe::{self, Tap};

/// How often a trunk is checked.
pub const INTERVAL: Duration = Duration::from_secs(30);
/// How long the tap waits for the marker.
const WAIT: Duration = Duration::from_secs(2);
/// Checks in a row without the marker until a trunk counts as alone again.
pub const MISSES: u8 = 3;
/// Magic of the marker, after the ethertype.
const MAGIC: &[u8; 4] = b"WLSH";
/// Version of the marker.
const VERSION: u8 = 1;
/// Destination of the marker: locally administered unicast that nobody
/// sends from, so no bridge or switch learns it and all of them flood it;
/// no host takes it.
const MARKER_DST: [u8; 6] = [0x02, 0x77, 0x6c, 0x73, 0x68, 0x00];
/// Length of the random part that tells this run's marker.
const NONCE_LEN: usize = 8;
/// Length of the marker (a minimal Ethernet frame with its tag).
const MARKER_LEN: usize = 64;

/// One mapped VLAN of a trunk to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The trunk interface.
    pub trunk: String,
    /// The VLAN of the network that the trunk drops as it is.
    pub vid: u16,
    /// The tunnels of the trunk's segment that carry the VLAN (up).
    pub tunnels: Vec<String>,
}

/// What to check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    /// The mapped VLANs of the trunks.
    pub list: Vec<Target>,
    /// The source address of the markers (from this site's identity).
    pub source: [u8; 6],
}

/// The source address of the markers of a site: locally administered
/// unicast, from `seed` (e.g. the fingerprint of its identity), so that the
/// bridges of the peers learn one address per site.
pub fn marker_source(seed: &str) -> [u8; 6] {
    // FNV-1a.
    let hash = seed.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let [a, b, c, d, ..] = hash.to_be_bytes();
    [0x06, 0x77, a, b, c, d]
}

/// The marker in VLAN `vid` (tagged in the frame: it goes straight into a
/// tunnel interface, not through the bridge).
fn marker_frame(vid: u16, source: [u8; 6], nonce: &[u8; NONCE_LEN]) -> [u8; MARKER_LEN] {
    let mut frame = [0u8; MARKER_LEN];
    frame[..6].copy_from_slice(&MARKER_DST);
    frame[6..12].copy_from_slice(&source);
    frame[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
    frame[14..16].copy_from_slice(&(vid & 0x0fff).to_be_bytes());
    frame[16..18].copy_from_slice(&KEEPALIVE_ETHERTYPE.to_be_bytes());
    frame[18..22].copy_from_slice(MAGIC);
    frame[22] = VERSION;
    frame[23..23 + NONCE_LEN].copy_from_slice(nonce);
    frame
}

/// True if `frame`, as a tap gets it (the kernel took the VLAN tag out),
/// is the marker with `nonce`.
fn is_marker(frame: &[u8], nonce: &[u8; NONCE_LEN]) -> bool {
    frame.get(..6) == Some(&MARKER_DST[..])
        && frame.get(12..14) == Some(&KEEPALIVE_ETHERTYPE.to_be_bytes()[..])
        && frame.get(14..18) == Some(&MAGIC[..])
        && frame.get(18) == Some(&VERSION)
        && frame.get(19..19 + NONCE_LEN) == Some(&nonce[..])
}

/// Starts the switch check; it reports changes as
/// [`Input::SharedSwitch`].
pub fn spawn(targets: watch::Receiver<Targets>, inputs: mpsc::Sender<Input>) -> JoinHandle<()> {
    tokio::spawn(run(targets, inputs))
}

/// What is known about one target: shared, and the misses in a row.
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    shared: bool,
    misses: u8,
}

async fn run(mut targets: watch::Receiver<Targets>, inputs: mpsc::Sender<Input>) {
    let mut seen: BTreeMap<(String, u16), Seen> = BTreeMap::new();
    // The last error per trunk, logged when it changes.
    let mut failing: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let current = targets.borrow_and_update().clone();
        seen.retain(|(trunk, vid), _| {
            current
                .list
                .iter()
                .any(|t| t.trunk == *trunk && t.vid == *vid)
        });
        failing.retain(|trunk, _| current.list.iter().any(|t| t.trunk == *trunk));
        for target in current.list.iter().filter(|t| !t.tunnels.is_empty()) {
            let result = check(target.clone(), current.source).await;
            let hit = match result {
                Ok(Some(hit)) => {
                    if failing.remove(&target.trunk).is_some() {
                        debug!("The switch check of {} works again", target.trunk);
                    }
                    hit
                }
                // Nothing went out (the tunnels are down): no answer.
                Ok(None) => continue,
                Err(e) => {
                    let message = e.to_string();
                    if failing.get(&target.trunk) != Some(&message) {
                        warn!(
                            "Cannot check whether {} is in one switch network with a peer: {message}",
                            target.trunk
                        );
                        failing.insert(target.trunk.clone(), message);
                    }
                    continue;
                }
            };
            let entry = seen.entry((target.trunk.clone(), target.vid)).or_default();
            let was = entry.shared;
            if hit {
                entry.shared = true;
                entry.misses = 0;
            } else if entry.shared {
                entry.misses = entry.misses.saturating_add(1);
                if entry.misses >= MISSES {
                    entry.shared = false;
                    entry.misses = 0;
                }
            }
            if entry.shared != was {
                let input = Input::SharedSwitch {
                    trunk: target.trunk.clone(),
                    vid: target.vid,
                    shared: entry.shared,
                };
                if inputs.send(input).await.is_err() {
                    return;
                }
            }
        }
        tokio::select! {
            r = targets.changed() => if r.is_err() { return },
            () = tokio::time::sleep(INTERVAL) => {}
        }
    }
}

/// One check of `target`: `None` if no marker went out.
async fn check(target: Target, source: [u8; 6]) -> io::Result<Option<bool>> {
    tokio::task::spawn_blocking(move || {
        let nonce = crate::random::bytes::<NONCE_LEN>().map_err(io::Error::other)?;
        // Open before sending: the marker may be back in microseconds.
        let tap = Tap::open(&target.trunk, KEEPALIVE_ETHERTYPE, *MAGIC, Some(target.vid))?;
        let frame = marker_frame(target.vid, source, &nonce);
        let mut sent = false;
        for tunnel in &target.tunnels {
            match rawframe::send(tunnel, &frame) {
                Ok(()) => sent = true,
                // Down or not there (yet): nothing to check through it.
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENETDOWN | libc::ENODEV)) => {}
                Err(e) => return Err(io::Error::new(e.kind(), format!("{tunnel}: {e}"))),
            }
        }
        if !sent {
            return Ok(None);
        }
        Ok(Some(tap.wait(WAIT, |f| is_marker(f, &nonce))))
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_recognised_as_a_tap_gets_it() {
        let nonce = [7u8; NONCE_LEN];
        let source = marker_source("site");
        assert_eq!(source[0] & 0x03, 0x02, "locally administered unicast");
        assert_ne!(source, marker_source("other"));
        let frame = marker_frame(14, source, &nonce);
        assert_eq!(&frame[12..16], &[0x81, 0x00, 0x00, 14]);
        // The kernel takes the tag out before the tap.
        let mut untagged = frame[..12].to_vec();
        untagged.extend_from_slice(&frame[16..]);
        assert!(is_marker(&untagged, &nonce));
        assert!(!is_marker(&untagged, &[8u8; NONCE_LEN]));
        assert!(!is_marker(&frame, &nonce));
    }
}
