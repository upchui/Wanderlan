//! The switch check: is a trunk with a VLAN mapping in one switch network
//! with a peer, and do DHCP replies get lost there?
//!
//! A site that maps VLAN V of the network to another VLAN on its trunk
//! drops V as it is there (see `desired::dropped_mappings`). Where the trunk
//! and the trunk of a peer are in one switch network, frames the peer puts
//! out in V still arrive at the trunk in V: the switch sees every device of
//! the mapped VLAN twice, at its own port in the mapped VLAN and at the
//! peer's port in V. That is harmless, unless the switch binds a device to
//! one port (DHCP snooping, port security): then it may send the device's
//! traffic, above all its DHCP replies, to the wrong port.
//!
//! Every [`INTERVAL`] (and when the tunnels change) a marker frame in V goes
//! into every tunnel of the trunk's segment that carries V: the peer floods
//! it (its destination is never learned) out of its trunk. A tap on the
//! trunk looks for it there in V, before tc drops it. One marker seen means
//! shared; [`MISSES`] in a row without one mean no longer.
//!
//! While a trunk is shared, a thread watches the DHCP of the mapped VLAN
//! ([`DhcpWatch`]): the requests that come in at the trunk and the replies
//! for their clients (from the tunnels, or from the trunk itself). Requests
//! of a client that go unanswered [`UNANSWERED`] times in a row are
//! reported: the daemon then asks for broadcast replies, and warns if they
//! get lost all the same.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use wanderlan_core::loopguard::format_mac;
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

/// How long a DHCP request waits for a reply.
pub const REPLY_WAIT: Duration = Duration::from_secs(10);
/// Unanswered DHCP requests of one client in a row that are reported.
pub const UNANSWERED: u8 = 3;
/// A client that asked nothing for so long is forgotten.
const FORGET: Duration = Duration::from_secs(600);

/// One mapped VLAN of a trunk to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The trunk interface.
    pub trunk: String,
    /// The VLAN of the network that the trunk drops as it is.
    pub vid: u16,
    /// The VLAN on the trunk; `None` for untagged.
    pub local: Option<u16>,
    /// The tunnels of the trunk's segment that carry the VLAN (up).
    pub tunnels: Vec<String>,
    /// The DHCP requests from there ask for broadcast replies (the rule is
    /// in place).
    pub broadcast: bool,
}

/// What to check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    /// The mapped VLANs of the trunks.
    pub list: Vec<Target>,
    /// The source address of the markers (from this site's identity).
    pub source: [u8; 6],
}

/// What the DHCP watch of a shared trunk found about one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhcpEvent {
    /// Its requests went unanswered [`UNANSWERED`] times in a row; with
    /// `broadcast` all of them asked for broadcast replies.
    Unanswered {
        /// Sent while the broadcast rule was in place.
        broadcast: bool,
    },
    /// Reported before, it got a reply now (or stopped asking).
    Recovered,
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

/// A BOOTP message: a request (else a reply) of client `client`.
fn parse_bootp(frame: &[u8]) -> Option<(bool, [u8; 6])> {
    // IPv4 right after the Ethernet header (the tap gets it untagged), its
    // header length from the first byte, then 8 bytes UDP.
    let ihl = usize::from(frame.get(14)? & 0x0f) * 4;
    let bootp = frame.get(14 + ihl + 8..)?;
    let client: [u8; 6] = bootp.get(28..34)?.try_into().ok()?;
    match bootp.first()? {
        1 => Some((true, client)),
        2 => Some((false, client)),
        _ => None,
    }
}

/// One client of the DHCP watch.
#[derive(Debug)]
struct Client {
    /// Its requests that wait for a reply: when, and whether they asked for
    /// broadcast replies.
    pending: Vec<(Instant, bool)>,
    /// Unanswered in a row.
    unanswered: u8,
    /// The last request.
    last: Instant,
    /// Reported as unanswered.
    reported: bool,
}

/// The DHCP of one mapped VLAN: which clients get no replies.
#[derive(Debug, Default)]
pub struct DhcpWatch {
    clients: HashMap<[u8; 6], Client>,
    broadcast: bool,
}

impl DhcpWatch {
    /// The requests from now on ask for broadcast replies (or no longer):
    /// only those count from now on, every client may be reported anew.
    pub fn set_broadcast(&mut self, on: bool) {
        if on == self.broadcast {
            return;
        }
        self.broadcast = on;
        for client in self.clients.values_mut() {
            client.unanswered = 0;
            client.reported = false;
        }
    }

    /// A DHCP frame: from the trunk (`from_trunk`) or a tunnel. Requests
    /// count only from the trunk (the clients of this site); a reply for a
    /// client answers all its requests.
    pub fn frame(
        &mut self,
        now: Instant,
        from_trunk: bool,
        frame: &[u8],
    ) -> Vec<([u8; 6], DhcpEvent)> {
        let Some((request, mac)) = parse_bootp(frame) else {
            return Vec::new();
        };
        if request {
            if from_trunk {
                let client = self.clients.entry(mac).or_insert(Client {
                    pending: Vec::new(),
                    unanswered: 0,
                    last: now,
                    reported: false,
                });
                client.pending.push((now, self.broadcast));
                client.last = now;
            }
            return Vec::new();
        }
        let Some(client) = self.clients.get_mut(&mac) else {
            return Vec::new();
        };
        client.pending.clear();
        client.unanswered = 0;
        if std::mem::take(&mut client.reported) {
            vec![(mac, DhcpEvent::Recovered)]
        } else {
            Vec::new()
        }
    }

    /// Requests without a reply for [`REPLY_WAIT`] count as unanswered
    /// (those sent before the last change of the broadcast rule do not);
    /// clients that asked nothing for long are forgotten.
    pub fn tick(&mut self, now: Instant) -> Vec<([u8; 6], DhcpEvent)> {
        let mut events = Vec::new();
        let broadcast = self.broadcast;
        self.clients.retain(|mac, client| {
            let (expired, waiting): (Vec<_>, Vec<_>) = client
                .pending
                .iter()
                .partition(|(at, _)| now.duration_since(*at) >= REPLY_WAIT);
            client.pending = waiting;
            for (_, asked_broadcast) in expired {
                if asked_broadcast != broadcast {
                    continue;
                }
                client.unanswered = client.unanswered.saturating_add(1);
                if client.unanswered >= UNANSWERED && !client.reported {
                    client.reported = true;
                    events.push((*mac, DhcpEvent::Unanswered { broadcast }));
                }
            }
            let forget = client.pending.is_empty() && now.duration_since(client.last) >= FORGET;
            if forget && client.reported {
                events.push((*mac, DhcpEvent::Recovered));
            }
            !forget
        });
        events
    }
}

/// Starts the switch check; it reports changes as
/// [`Input::SharedSwitch`] and [`Input::Dhcp`].
pub fn spawn(targets: watch::Receiver<Targets>, inputs: mpsc::Sender<Input>) -> JoinHandle<()> {
    tokio::spawn(run(targets, inputs))
}

/// What is known about one target: shared, and the misses in a row.
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    shared: bool,
    misses: u8,
}

/// The DHCP watch of one shared target (a thread); stops when dropped.
struct Observer {
    target: Target,
    stop: Arc<AtomicBool>,
    broadcast: Arc<AtomicBool>,
}

impl Observer {
    fn start(target: &Target, inputs: mpsc::Sender<Input>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let broadcast = Arc::new(AtomicBool::new(target.broadcast));
        let (t, s, b) = (target.clone(), stop.clone(), broadcast.clone());
        let started = std::thread::Builder::new()
            .name("dhcp watch".into())
            .spawn(move || observe(&t, &s, &b, &inputs));
        if let Err(e) = started {
            warn!("Cannot watch the DHCP at {}: {e}", target.trunk);
        }
        Self {
            target: target.clone(),
            stop,
            broadcast,
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The DHCP watch of `target` until `stop`.
fn observe(
    target: &Target,
    stop: &AtomicBool,
    broadcast: &AtomicBool,
    inputs: &mpsc::Sender<Input>,
) {
    let mut failed: Option<String> = None;
    while !stop.load(Ordering::Relaxed) {
        let taps = Tap::open_dhcp(&target.trunk, target.local).and_then(|trunk| {
            let tunnels = target
                .tunnels
                .iter()
                .map(|t| Tap::open_dhcp(t, Some(target.vid)))
                .collect::<io::Result<Vec<Tap>>>()?;
            Ok((trunk, tunnels))
        });
        let (trunk, tunnels) = match taps {
            Ok(taps) => taps,
            Err(e) => {
                let message = e.to_string();
                if failed.as_ref() != Some(&message) {
                    warn!("Cannot watch the DHCP at {}: {message}", target.trunk);
                    failed = Some(message);
                }
                std::thread::sleep(INTERVAL);
                continue;
            }
        };
        watch_dhcp(target, &trunk, &tunnels, stop, broadcast, inputs);
        return;
    }
}

fn watch_dhcp(
    target: &Target,
    trunk: &Tap,
    tunnels: &[Tap],
    stop: &AtomicBool,
    broadcast: &AtomicBool,
    inputs: &mpsc::Sender<Input>,
) {
    let mut watch = DhcpWatch::default();
    let mut buf = [0u8; 512];
    let all: Vec<&Tap> = std::iter::once(trunk).chain(tunnels).collect();
    while !stop.load(Ordering::Relaxed) {
        watch.set_broadcast(broadcast.load(Ordering::Relaxed));
        let mut events = Vec::new();
        if Tap::poll(&all, Duration::from_secs(1)) {
            while let Some(n) = trunk.try_recv(&mut buf) {
                events.extend(watch.frame(Instant::now(), true, buf.get(..n).unwrap_or_default()));
            }
            for tap in tunnels {
                while let Some(n) = tap.try_recv(&mut buf) {
                    events.extend(watch.frame(
                        Instant::now(),
                        false,
                        buf.get(..n).unwrap_or_default(),
                    ));
                }
            }
        }
        events.extend(watch.tick(Instant::now()));
        for (client, event) in events {
            let input = Input::Dhcp {
                trunk: target.trunk.clone(),
                vid: target.vid,
                local: target.local,
                client: format_mac(&client),
                event,
            };
            if inputs.blocking_send(input).is_err() {
                return;
            }
        }
    }
}

async fn run(mut targets: watch::Receiver<Targets>, inputs: mpsc::Sender<Input>) {
    let mut seen: BTreeMap<(String, u16), Seen> = BTreeMap::new();
    let mut observers: BTreeMap<(String, u16), Observer> = BTreeMap::new();
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
        follow(&current, &seen, &mut observers, &inputs);
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
            let shared = entry.shared;
            if shared != was {
                let input = Input::SharedSwitch {
                    trunk: target.trunk.clone(),
                    vid: target.vid,
                    local: target.local,
                    shared,
                };
                if inputs.send(input).await.is_err() {
                    return;
                }
                // The DHCP watch at once, not after the other checks.
                follow(&current, &seen, &mut observers, &inputs);
            }
        }
        follow(&current, &seen, &mut observers, &inputs);
        tokio::select! {
            r = targets.changed() => if r.is_err() { return },
            () = tokio::time::sleep(INTERVAL) => {}
        }
    }
}

/// A DHCP watch for every shared target (started anew when its tunnels
/// change), none for the others; tells them whether the broadcast rule is
/// in place.
fn follow(
    current: &Targets,
    seen: &BTreeMap<(String, u16), Seen>,
    observers: &mut BTreeMap<(String, u16), Observer>,
    inputs: &mpsc::Sender<Input>,
) {
    let shared: BTreeMap<(String, u16), &Target> = current
        .list
        .iter()
        .map(|t| ((t.trunk.clone(), t.vid), t))
        .filter(|(key, _)| seen.get(key).is_some_and(|s| s.shared))
        .collect();
    observers.retain(|key, o| {
        shared
            .get(key)
            .is_some_and(|t| t.tunnels == o.target.tunnels && t.local == o.target.local)
    });
    for (key, target) in shared {
        let observer = observers
            .entry(key)
            .or_insert_with(|| Observer::start(target, inputs.clone()));
        observer
            .broadcast
            .store(target.broadcast, Ordering::Relaxed);
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

    /// A DHCP frame as a tap gets it: `op` 1 (request) or 2 (reply).
    fn bootp(op: u8, client: [u8; 6]) -> Vec<u8> {
        let mut frame = vec![0u8; 14];
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[9] = 17;
        frame.extend(ip);
        frame.extend([0, 68, 0, 67, 0, 0, 0, 0]);
        let mut payload = vec![0u8; 240];
        payload[0] = op;
        payload[28..34].copy_from_slice(&client);
        frame.extend(payload);
        frame
    }

    #[test]
    fn a_client_without_replies_is_reported_once_and_recovers_with_one() {
        let (a, b) = ([2, 0, 0, 0, 0, 0xa], [2, 0, 0, 0, 0, 0xb]);
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut w = DhcpWatch::default();
        for s in [0, 4, 8] {
            w.frame(at(s), true, &bootp(1, a));
            w.frame(at(s), true, &bootp(1, b));
        }
        // b gets its reply in time (through a tunnel); requests of others
        // that come out of a tunnel do not count.
        w.frame(at(9), false, &bootp(2, b));
        w.frame(at(9), false, &bootp(1, [2, 0, 0, 0, 0, 0xc]));
        assert_eq!(w.tick(at(17)), []);
        let unanswered = DhcpEvent::Unanswered { broadcast: false };
        assert_eq!(w.tick(at(18)), [(a, unanswered)]);
        w.frame(at(20), true, &bootp(1, a));
        assert_eq!(w.tick(at(40)), [], "reported once");

        // With the broadcast rule only the requests after it count.
        w.frame(at(41), true, &bootp(1, a));
        w.set_broadcast(true);
        for s in [42, 43] {
            w.frame(at(s), true, &bootp(1, a));
        }
        assert_eq!(w.tick(at(53)), []);
        w.frame(at(54), true, &bootp(1, a));
        assert_eq!(
            w.tick(at(64)),
            [(a, DhcpEvent::Unanswered { broadcast: true })]
        );
        // A reply from the trunk (a server on this site's LAN) counts too.
        assert_eq!(
            w.frame(at(65), true, &bootp(2, a)),
            [(a, DhcpEvent::Recovered)]
        );
        assert_eq!(w.frame(at(70), false, &bootp(2, a)), []);
        // Reported again, then a reply.
        for s in [80, 81, 82] {
            w.frame(at(s), true, &bootp(1, a));
        }
        assert_eq!(w.tick(at(92)).len(), 1);
        assert_eq!(
            w.frame(at(93), false, &bootp(2, a)),
            [(a, DhcpEvent::Recovered)]
        );
    }
}
