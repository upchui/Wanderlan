//! Control probes between daemons: reachability, RTT and path MTU.
//!
//! Every daemon listens on UDP `<site address>:<control_port>` and answers
//! echo requests of its direct peers. Requests are sent with the DF bit set
//! (`IP_PMTUDISC_PROBE`), so a binary search over the request size finds the
//! largest packet that reaches the peer unfragmented. Requests and replies
//! also announce the sender's mesh revision for the configuration sync.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use wanderlan_core::mtu::{self, wire};

/// Interval between reachability/RTT probes.
pub const RTT_INTERVAL: Duration = Duration::from_secs(5);
/// Timeout of a reachability probe.
const RTT_TIMEOUT: Duration = Duration::from_secs(1);
/// Timeout of a single MTU probe.
const MTU_TIMEOUT: Duration = Duration::from_millis(400);
/// Attempts per probe size before a size counts as too large.
const MTU_ATTEMPTS: usize = 3;
/// Consecutive lost probes after which a peer counts as unreachable.
const LOST_LIMIT: u32 = 3;
/// Probes kept for the round-trip statistics (5 minutes at 5 s).
const RTT_WINDOW: usize = 60;
/// Size of reachability probes.
const RTT_SIZE: u16 = 128;

/// The peers to probe and where to listen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeConfig {
    /// Local address and port to bind.
    pub bind: Option<SocketAddrV4>,
    /// Direct peers by name.
    pub peers: BTreeMap<String, Ipv4Addr>,
    /// Further sites whose probes are answered but which are not probed
    /// (peers removed recently, so they can still fetch the new mesh).
    pub responders: BTreeMap<String, Ipv4Addr>,
    /// Incremented to request an immediate path MTU measurement.
    pub generation: u64,
    /// Interval between path MTU measurements; `None` disables them (fixed MSS).
    pub mtu_interval: Option<Duration>,
    /// What this site announces (mesh revision and fingerprint).
    pub announce: wire::Announce,
    /// What this site announces to single peers instead: to a client the
    /// version of the client's view of the network.
    pub announce_to: BTreeMap<String, wire::Announce>,
}

impl ProbeConfig {
    /// What to announce to the peer at `addr`.
    fn announce_for(&self, addr: Ipv4Addr) -> wire::Announce {
        self.peers
            .iter()
            .chain(self.responders.iter())
            .find(|(_, a)| **a == addr)
            .and_then(|(name, _)| self.announce_to.get(name))
            .copied()
            .unwrap_or(self.announce)
    }
}

/// Measurements of one peer, as shown in the status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PeerProbe {
    /// The peer answers probes.
    pub reachable: bool,
    /// Last answered probe.
    pub last_reply: Option<SystemTime>,
    /// Round-trip time of the last answered probe in milliseconds.
    pub rtt_ms: Option<f64>,
    /// Measured path MTU.
    pub pmtu: Option<u16>,
    /// When the path MTU was last measured.
    pub pmtu_measured: Option<SystemTime>,
    /// What the peer announced last.
    #[serde(skip)]
    pub announce: Option<wire::Announce>,
    #[serde(skip)]
    lost: u32,
    /// Recent probes: round trip in ms, or `None` if unanswered.
    #[serde(skip)]
    recent: VecDeque<Option<f64>>,
}

impl PeerProbe {
    /// Round-trip statistics over the recent probes.
    pub fn rtt_stats(&self) -> Option<wanderlan_core::api::RttStats> {
        let answered: Vec<f64> = self.recent.iter().flatten().copied().collect();
        if answered.is_empty() {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let count = |n: usize| n as f64;
        let min_ms = answered.iter().copied().fold(f64::INFINITY, f64::min);
        let max_ms = answered.iter().copied().fold(0.0, f64::max);
        let avg_ms = answered.iter().sum::<f64>() / count(answered.len());
        let jitter_ms = if answered.len() > 1 {
            answered
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .sum::<f64>()
                / count(answered.len() - 1)
        } else {
            0.0
        };
        let lost = self.recent.iter().filter(|r| r.is_none()).count();
        Some(wanderlan_core::api::RttStats {
            min_ms,
            avg_ms,
            max_ms,
            jitter_ms,
            loss_percent: 100.0 * count(lost) / count(self.recent.len()),
            samples: u32::try_from(self.recent.len()).unwrap_or(u32::MAX),
        })
    }
}

/// Shared measurement table.
pub type ProbeTable = Arc<RwLock<BTreeMap<String, PeerProbe>>>;

/// Events for the controller.
#[derive(Debug)]
pub enum ProbeEvent {
    /// The measured path MTU of a peer changed.
    Pmtu { peer: String, pmtu: Option<u16> },
    /// A peer announced a mesh revision.
    Announce {
        peer: String,
        announce: wire::Announce,
    },
}

struct Shared {
    socket: UdpSocket,
    pending: Mutex<HashMap<u32, oneshot::Sender<(u16, wire::Announce)>>>,
    seq: AtomicU32,
    config: watch::Receiver<ProbeConfig>,
}

/// Starts the probe service. It follows changes of the configuration.
pub fn spawn(
    config: watch::Receiver<ProbeConfig>,
    table: ProbeTable,
    events: mpsc::Sender<ProbeEvent>,
) -> JoinHandle<()> {
    tokio::spawn(run(config, table, events))
}

async fn run(
    mut config: watch::Receiver<ProbeConfig>,
    table: ProbeTable,
    events: mpsc::Sender<ProbeEvent>,
) {
    loop {
        let bind = config.borrow().bind;
        let Some(bind) = bind else {
            if config.changed().await.is_err() {
                return;
            }
            continue;
        };
        let socket = match bind_socket(bind).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Cannot listen for control probes on {bind}: {e:#}; retrying in 10s");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    r = config.changed() => if r.is_err() { return },
                }
                continue;
            }
        };
        info!("Listening for control probes on {bind} (UDP)");
        let shared = Arc::new(Shared {
            socket,
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU32::new(1),
            config: config.clone(),
        });
        let receiver = tokio::spawn(receive(shared.clone(), table.clone(), events.clone()));
        let mut prober: BTreeMap<String, (Ipv4Addr, JoinHandle<()>)> = BTreeMap::new();
        loop {
            let current = config.borrow_and_update().clone();
            if current.bind != Some(bind) {
                break;
            }
            sync_probers(&mut prober, &current, &shared, &table, &events);
            if config.changed().await.is_err() {
                break;
            }
        }
        receiver.abort();
        for (_, (_, task)) in prober {
            task.abort();
        }
        // Without a listening address (reset, unconfigured) or with a new
        // one, nothing measured or announced before is valid any more.
        if let Ok(mut t) = table.write() {
            t.clear();
        }
    }
}

async fn bind_socket(bind: SocketAddrV4) -> anyhow::Result<UdpSocket> {
    let socket = std::net::UdpSocket::bind(bind)?;
    crate::sockopt::set_pmtudisc_probe(&socket)?;
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket)?)
}

/// Starts and stops the per-peer tasks to match the configured peers.
fn sync_probers(
    probers: &mut BTreeMap<String, (Ipv4Addr, JoinHandle<()>)>,
    config: &ProbeConfig,
    shared: &Arc<Shared>,
    table: &ProbeTable,
    events: &mpsc::Sender<ProbeEvent>,
) {
    probers.retain(|name, (addr, task)| {
        let keep = config.peers.get(name) == Some(addr);
        if !keep {
            task.abort();
        }
        keep
    });
    if let Ok(mut t) = table.write() {
        t.retain(|name, _| config.peers.contains_key(name));
    }
    for (name, addr) in &config.peers {
        if probers.contains_key(name) {
            continue;
        }
        let task = tokio::spawn(probe_peer(
            shared.clone(),
            name.clone(),
            *addr,
            table.clone(),
            events.clone(),
        ));
        probers.insert(name.clone(), (*addr, task));
    }
}

/// Receives requests (answered if they come from a peer) and replies
/// (handed to the waiting probe).
async fn receive(shared: Arc<Shared>, table: ProbeTable, events: mpsc::Sender<ProbeEvent>) {
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, from) = match shared.socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                debug!("probe receive error: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let SocketAddr::V4(from) = from else { continue };
        let Some(msg) = buf.get(..n).and_then(wire::decode) else {
            continue;
        };
        let (peer, announce_self) = {
            let cfg = shared.config.borrow();
            let peer = cfg
                .peers
                .iter()
                .find(|(_, a)| **a == *from.ip())
                .map(|(n, _)| (n.clone(), true))
                .or_else(|| {
                    cfg.responders
                        .iter()
                        .find(|(_, a)| **a == *from.ip())
                        .map(|(n, _)| (n.clone(), false))
                });
            (peer, cfg.announce_for(*from.ip()))
        };
        let Some((peer, direct)) = peer else {
            debug!("ignoring probe from {from}, not a peer");
            continue;
        };
        match msg {
            wire::Probe::Request { seq, len, announce } => {
                let reply = wire::reply(seq, len, &announce_self);
                if let Err(e) = shared.socket.send_to(&reply, from).await {
                    debug!("cannot answer probe from {from}: {e}");
                }
                if direct {
                    note_announce(&table, &events, &peer, announce).await;
                }
            }
            wire::Probe::Reply {
                seq,
                received,
                announce,
            } => {
                let waiter = shared.pending.lock().ok().and_then(|mut p| p.remove(&seq));
                if let Some(tx) = waiter {
                    let _ = tx.send((received, announce));
                }
                if direct {
                    note_announce(&table, &events, &peer, announce).await;
                }
            }
        }
    }
}

async fn note_announce(
    table: &ProbeTable,
    events: &mpsc::Sender<ProbeEvent>,
    peer: &str,
    announce: wire::Announce,
) {
    let changed = match table.write() {
        Ok(mut t) => {
            let entry = t.entry(peer.to_string()).or_default();
            let changed = entry.announce != Some(announce);
            entry.announce = Some(announce);
            changed
        }
        Err(_) => false,
    };
    if changed {
        let _ = events
            .send(ProbeEvent::Announce {
                peer: peer.to_string(),
                announce,
            })
            .await;
    }
}

/// Sends one probe of `size` bytes (IP packet) and waits for the reply.
/// Returns the round-trip time if the complete probe arrived.
async fn probe_once(
    shared: &Shared,
    peer: SocketAddrV4,
    size: u16,
    timeout: Duration,
) -> Option<Duration> {
    let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
    let announce = shared.config.borrow().announce_for(*peer.ip());
    let packet = wire::request(seq, size, &announce);
    let (tx, rx) = oneshot::channel();
    if let Ok(mut p) = shared.pending.lock() {
        p.insert(seq, tx);
    }
    let start = Instant::now();
    let result = match shared.socket.send_to(&packet, peer).await {
        // EMSGSIZE: larger than the local interface MTU.
        Err(_) => None,
        Ok(_) => match tokio::time::timeout(timeout, rx).await {
            Ok(Ok((received, _))) if usize::from(received) == packet.len() => Some(start.elapsed()),
            _ => None,
        },
    };
    if let Ok(mut p) = shared.pending.lock() {
        p.remove(&seq);
    }
    result
}

/// Probing loop for one peer: reachability every few seconds, path MTU when
/// due (at start, periodically and whenever the peer becomes reachable).
async fn probe_peer(
    shared: Arc<Shared>,
    name: String,
    addr: Ipv4Addr,
    table: ProbeTable,
    events: mpsc::Sender<ProbeEvent>,
) {
    let port = shared.config.borrow().bind.map_or(0, |b| b.port());
    let target = SocketAddrV4::new(addr, port);
    let mut next_mtu: Option<Instant> = Some(Instant::now());
    let mut last_pmtu: Option<u16> = None;
    let mut generation = shared.config.borrow().generation;
    loop {
        let current = shared.config.borrow().generation;
        if current != generation {
            generation = current;
            next_mtu = Some(Instant::now());
        }
        let rtt = probe_once(&shared, target, RTT_SIZE, RTT_TIMEOUT).await;
        let became_reachable = update_reachability(&table, &name, rtt);
        if became_reachable {
            info!("Peer \"{name}\" ({addr}) is reachable");
            next_mtu = Some(Instant::now());
        }

        let mtu_interval = shared.config.borrow().mtu_interval;
        let due = next_mtu.is_some_and(|t| Instant::now() >= t);
        if let (Some(interval), true, Some(_)) = (mtu_interval, due, rtt) {
            let pmtu = mtu::search(mtu::PROBE_MIN, mtu::PROBE_MAX, |size| {
                let shared = shared.clone();
                async move {
                    for _ in 0..MTU_ATTEMPTS {
                        if probe_once(&shared, target, size, MTU_TIMEOUT)
                            .await
                            .is_some()
                        {
                            return true;
                        }
                    }
                    false
                }
            })
            .await;
            next_mtu = Some(Instant::now() + interval);
            if let Ok(mut t) = table.write() {
                let entry = t.entry(name.clone()).or_default();
                entry.pmtu = pmtu;
                entry.pmtu_measured = Some(SystemTime::now());
            }
            if pmtu != last_pmtu {
                match pmtu {
                    Some(p) => info!(
                        "Path MTU to \"{name}\" is {p} bytes (TCP MSS {})",
                        mtu::mss_for_pmtu(p)
                    ),
                    None => warn!("Path MTU to \"{name}\" could not be measured"),
                }
                last_pmtu = pmtu;
                let _ = events
                    .send(ProbeEvent::Pmtu {
                        peer: name.clone(),
                        pmtu,
                    })
                    .await;
            }
        }
        tokio::time::sleep(RTT_INTERVAL).await;
    }
}

/// Records the result of a reachability probe. Returns true if the peer
/// just became reachable.
fn update_reachability(table: &ProbeTable, name: &str, rtt: Option<Duration>) -> bool {
    let Ok(mut t) = table.write() else {
        return false;
    };
    let entry = t.entry(name.to_string()).or_default();
    entry
        .recent
        .push_back(rtt.map(|r| r.as_secs_f64() * 1000.0));
    while entry.recent.len() > RTT_WINDOW {
        entry.recent.pop_front();
    }
    match rtt {
        Some(rtt) => {
            let was = entry.reachable;
            entry.reachable = true;
            entry.lost = 0;
            entry.last_reply = Some(SystemTime::now());
            entry.rtt_ms = Some(rtt.as_secs_f64() * 1000.0);
            !was
        }
        None => {
            entry.lost = entry.lost.saturating_add(1);
            if entry.lost >= LOST_LIMIT && entry.reachable {
                entry.reachable = false;
                warn!("Peer \"{name}\" does not answer control probes");
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_statistics() {
        let mut p = PeerProbe::default();
        assert!(p.rtt_stats().is_none());
        p.recent = [Some(10.0), Some(14.0), None, Some(12.0)].into();
        let s = p.rtt_stats().unwrap();
        assert!((s.min_ms - 10.0).abs() < 1e-9);
        assert!((s.max_ms - 14.0).abs() < 1e-9);
        assert!((s.avg_ms - 12.0).abs() < 1e-9);
        // |14-10| and |12-14| over the answered probes.
        assert!((s.jitter_ms - 3.0).abs() < 1e-9);
        assert!((s.loss_percent - 25.0).abs() < 1e-9);
        assert_eq!(s.samples, 4);
    }

    #[test]
    fn the_window_is_limited() {
        let table: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
        for _ in 0..RTT_WINDOW + 10 {
            update_reachability(&table, "a", Some(Duration::from_millis(5)));
        }
        let t = table.read().unwrap();
        assert_eq!(t["a"].recent.len(), RTT_WINDOW);
    }
}
