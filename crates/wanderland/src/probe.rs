//! Control probes between daemons: reachability, RTT and path MTU.
//!
//! Every daemon listens on UDP `<site address>:<control_port>` and answers
//! echo requests of its direct peers. Requests are sent with the DF bit set
//! (`IP_PMTUDISC_PROBE`), so a binary search over the request size finds the
//! largest packet that reaches the peer unfragmented. Requests and replies
//! also announce the sender's network revision for the configuration sync.
//!
//! Peers whose identity is known are probed with version 2 of the probes
//! (`WLP2`, see [`wire2`]): every probe carries an HMAC tag of the tunnel's
//! probe key. The first valid answer confirms the peer (its tunnel may come
//! up); an answer with a wrong tag means another box answers at its address.
//! A peer with a key counts as reachable only with valid answers.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use wanderlan_core::identity::{Identity, PairKeys, PeerIdentity, SiteId};
use wanderlan_core::mtu::{self, wire, wire2};

/// Interval between reachability/RTT probes.
pub const RTT_INTERVAL: Duration = Duration::from_secs(5);
/// Timeout of a reachability probe.
const RTT_TIMEOUT: Duration = Duration::from_secs(1);
/// Timeout of a single MTU probe.
const MTU_TIMEOUT: Duration = Duration::from_millis(400);
/// Longest wait for one MTU probe (a slow, full uplink).
const MTU_TIMEOUT_MAX: Duration = Duration::from_secs(2);
/// When a measurement failed: try again after this long.
const MTU_RETRY: Duration = Duration::from_secs(30);
/// Attempts per probe size before a size counts as too large.
const MTU_ATTEMPTS: usize = 3;
/// Consecutive lost probes after which a peer counts as unreachable.
const LOST_LIMIT: u32 = 3;
/// Probes kept for the round-trip statistics (5 minutes at 5 s).
const RTT_WINDOW: usize = 60;
/// Size of reachability probes (IP packet; larger than a version 2 request).
const RTT_SIZE: u16 = 160;
/// Identities claimed in requests whose keys are kept (peers this site has
/// no identity for yet).
const CLAIMED_MAX: usize = 64;

/// The keys towards one peer whose identity is known.
#[derive(Debug, Clone)]
pub struct PeerKeys {
    /// The peer's identity.
    pub id: SiteId,
    /// The keys of the tunnel to it.
    pub keys: Arc<PairKeys>,
}

impl PartialEq for PeerKeys {
    // The keys follow from the identities.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for PeerKeys {}

/// The identity of this box, compared by its public key.
#[derive(Debug, Clone)]
pub struct Me(pub Arc<Identity>);

impl PartialEq for Me {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Eq for Me {}

/// The peers to probe and where to listen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeConfig {
    /// Local address and port to bind.
    pub bind: Option<SocketAddrV4>,
    /// Direct peers by name.
    pub peers: BTreeMap<String, Ipv4Addr>,
    /// Further sites whose probes are answered but which are not probed
    /// (peers removed recently, so they can still fetch the new network).
    pub responders: BTreeMap<String, Ipv4Addr>,
    /// Incremented to request an immediate path MTU measurement.
    pub generation: u64,
    /// Interval between path MTU measurements; `None` disables them (fixed MSS).
    pub mtu_interval: Option<Duration>,
    /// What this site announces (network revision and fingerprint).
    pub announce: wire::Announce,
    /// What this site announces to single peers instead: to a client the
    /// version of the client's view of the network.
    pub announce_to: BTreeMap<String, wire::Announce>,
    /// This box's identity; without it only version 1 probes.
    pub me: Option<Me>,
    /// The keys towards the peers whose identity is known: probed with
    /// version 2; every other peer with version 1.
    pub keys: BTreeMap<String, PeerKeys>,
    /// Peers this site never sends anything to first (its clients with a
    /// key, maybe behind NAT): probed only where their authenticated probes
    /// come from, once that place answered a challenge.
    pub passive: BTreeSet<String>,
}

impl ProbeConfig {
    /// What to announce to `peer`.
    fn announce_of(&self, peer: &str) -> wire::Announce {
        self.announce_to.get(peer).copied().unwrap_or(self.announce)
    }

    /// The peer (or responder, `false`) at `ip`.
    fn peer_at(&self, ip: Ipv4Addr) -> Option<(String, bool)> {
        self.peers
            .iter()
            .find(|(_, a)| **a == ip)
            .map(|(n, _)| (n.clone(), true))
            .or_else(|| {
                self.responders
                    .iter()
                    .find(|(_, a)| **a == ip)
                    .map(|(n, _)| (n.clone(), false))
            })
    }

    /// The peer with identity `id`.
    fn peer_with(&self, id: SiteId) -> Option<String> {
        self.keys
            .iter()
            .find(|(_, k)| k.id == id)
            .map(|(n, _)| n.clone())
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
    /// When an announce of the peer arrived last (also an unchanged one).
    #[serde(skip)]
    pub announced_at: Option<Instant>,
    /// Last answered probe on the monotonic clock (a clock step at boot
    /// does not move it). For a peer with a key only valid answers count.
    #[serde(skip)]
    pub last_reply_at: Option<Instant>,
    /// What the probes found about the peer's identity (peers with a key).
    #[serde(skip)]
    pub identity: Option<PeerIdentity>,
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
    /// A peer announced a network revision.
    Announce {
        peer: String,
        announce: wire::Announce,
    },
    /// The probes found something about the identity of a peer with a key:
    /// it answered with a valid tag (confirmed), or with a wrong one.
    Identity { peer: String, state: PeerIdentity },
    /// A passive peer (a client) answered a challenge at this address: its
    /// control traffic comes from there (its NAT's address and port).
    Endpoint { peer: String, addr: SocketAddrV4 },
}

/// A probe waiting for its reply: from that peer, with that sequence number.
type PendingKey = (Ipv4Addr, u32);

/// A sent probe: where its answer goes, and how to check it.
struct Waiter {
    tx: oneshot::Sender<Answer>,
    /// For a version 2 probe: the keys and the identity of the peer.
    keys: Option<(Arc<PairKeys>, SiteId)>,
}

/// The answer to a probe.
struct Answer {
    received: u16,
    /// The answer proves the peer's identity (always true for version 1).
    valid: bool,
}

struct Shared {
    socket: UdpSocket,
    pending: Mutex<HashMap<PendingKey, Waiter>>,
    seq: AtomicU32,
    config: watch::Receiver<ProbeConfig>,
    /// The nonce of this run, in every version 2 request.
    nonce: wire2::Nonce,
    /// Keys for identities claimed in requests of peers whose identity this
    /// site does not know yet (bounded).
    claimed: Mutex<HashMap<SiteId, Option<Arc<PairKeys>>>>,
    /// Where the passive peers are probed (confirmed by a challenge).
    endpoints: Mutex<HashMap<String, SocketAddrV4>>,
    /// Where authenticated requests of passive peers came from lately, to be
    /// confirmed by a challenge.
    candidates: Mutex<HashMap<String, SocketAddrV4>>,
}

impl Shared {
    /// The keys towards a peer that claims `id`, derived once.
    fn claimed_keys(&self, me: &Identity, id: SiteId) -> Option<Arc<PairKeys>> {
        let mut claimed = self.claimed.lock().ok()?;
        if !claimed.contains_key(&id) && claimed.len() >= CLAIMED_MAX {
            claimed.clear();
        }
        claimed
            .entry(id)
            .or_insert_with(|| me.pair(&id).map(Arc::new))
            .clone()
    }
}

/// Starts the probe service. It follows changes of the configuration.
pub fn spawn(
    config: watch::Receiver<ProbeConfig>,
    table: ProbeTable,
    events: mpsc::Sender<ProbeEvent>,
) -> JoinHandle<()> {
    tokio::spawn(run(config, table, events))
}

/// How often the probe tasks are checked.
const SUPERVISE: Duration = Duration::from_secs(10);

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
        let nonce = crate::random::bytes::<{ wire2::NONCE_LEN }>().unwrap_or_else(|e| {
            warn!("{e:#}; the probes of this run use a fixed nonce");
            [0; wire2::NONCE_LEN]
        });
        let shared = Arc::new(Shared {
            socket,
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU32::new(1),
            config: config.clone(),
            nonce,
            claimed: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
            candidates: Mutex::new(HashMap::new()),
        });
        let receiver = tokio::spawn(receive(shared.clone(), table.clone(), events.clone()));
        let mut prober: BTreeMap<String, (Target, JoinHandle<()>)> = BTreeMap::new();
        let mut gone = false;
        loop {
            let current = config.borrow_and_update().clone();
            if current.bind != Some(bind) {
                break;
            }
            sync_probers(&mut prober, &current, &shared, &table, &events);
            // Every few seconds also without a change: a task that ended is
            // started again (the receiver with a new socket).
            tokio::select! {
                r = config.changed() => {
                    if r.is_err() {
                        gone = true;
                    }
                }
                () = tokio::time::sleep(SUPERVISE) => {}
            }
            if gone || receiver.is_finished() {
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

/// What a probe task is for: the peer's address and identity.
type Target = (Ipv4Addr, Option<SiteId>);

/// Starts and stops the per-peer tasks to match the configured peers. A
/// peer with another address or identity gets a new task and starts over.
fn sync_probers(
    probers: &mut BTreeMap<String, (Target, JoinHandle<()>)>,
    config: &ProbeConfig,
    shared: &Arc<Shared>,
    table: &ProbeTable,
    events: &mpsc::Sender<ProbeEvent>,
) {
    let target = |name: &str| {
        config
            .peers
            .get(name)
            .map(|addr| (*addr, config.keys.get(name).map(|k| k.id)))
    };
    probers.retain(|name, (current, task)| {
        // A task that ended (it never should) is started again below.
        let keep = target(name) == Some(*current) && !task.is_finished();
        if !keep {
            task.abort();
            if let Ok(mut t) = table.write() {
                t.remove(name);
            }
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
        if let Some(current) = target(name) {
            probers.insert(name.clone(), (current, task));
        }
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
        let Some(packet) = buf.get(..n) else { continue };
        let v2 = wire2::decode(packet);
        // A request of a peer with a key names it by its identity (several
        // clients behind one NAT share an address); everything else by the
        // address.
        let (peer, announce_self, passive) = {
            let cfg = shared.config.borrow();
            let peer = match v2 {
                Some(wire2::Probe::Request(r)) => cfg
                    .peer_with(r.sender)
                    .map(|n| (n, true))
                    .or_else(|| cfg.peer_at(*from.ip())),
                _ => cfg.peer_at(*from.ip()),
            };
            let announce = peer
                .as_ref()
                .map_or(cfg.announce, |(n, _)| cfg.announce_of(n));
            let passive = peer.as_ref().is_some_and(|(n, _)| cfg.passive.contains(n));
            (peer, announce, passive)
        };
        let Some((peer, direct)) = peer else {
            debug!("ignoring probe from {from}, not a peer");
            continue;
        };
        if let Some(msg) = wire::decode(packet) {
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
                    // A version 1 answer proves nothing about the identity.
                    let valid = answer(&shared, *from.ip(), seq, received, |w| w.keys.is_none());
                    if direct && valid {
                        note_announce(&table, &events, &peer, announce).await;
                    }
                }
            }
        } else if let Some(msg) = v2 {
            match msg {
                wire2::Probe::Request(request) => {
                    let me = shared.config.borrow().me.clone();
                    let Some(me) = me else { continue };
                    let Some(keys) = request_keys(&shared, &me.0, &peer, request.sender) else {
                        continue;
                    };
                    // A wrong tag is answered too (the reply is smaller than
                    // the request): the requester sees that this box has
                    // another key than it expects here.
                    let verified = wire2::verify_request(packet, &keys);
                    let flags = if verified {
                        0
                    } else {
                        debug!("probe from {from} with a tag of another key");
                        wire2::FLAG_UNVERIFIED
                    };
                    let reply = wire2::reply(&request, flags, &announce_self, &me.0.id(), &keys);
                    if let Err(e) = shared.socket.send_to(&reply, from).await {
                        debug!("cannot answer probe from {from}: {e}");
                    }
                    if direct && verified {
                        note_announce(&table, &events, &peer, request.announce).await;
                    }
                    // A client asks from where its NAT sends: the place to
                    // probe it, once it answers a challenge there.
                    if passive && verified {
                        let known = shared
                            .endpoints
                            .lock()
                            .ok()
                            .and_then(|e| e.get(&peer).copied());
                        if known != Some(from)
                            && let Ok(mut c) = shared.candidates.lock()
                        {
                            c.insert(peer.clone(), from);
                        }
                    }
                }
                wire2::Probe::Reply(reply) => {
                    let valid = answer(&shared, *from.ip(), reply.seq, reply.received, |w| {
                        w.keys.as_ref().is_some_and(|(keys, id)| {
                            reply.responder == *id
                                && reply.flags & wire2::FLAG_UNVERIFIED == 0
                                && wire2::verify_reply(packet, &shared.nonce, keys)
                        })
                    });
                    if direct && valid {
                        note_announce(&table, &events, &peer, reply.announce).await;
                    }
                }
            }
        }
    }
}

/// Hands an answer to the probe waiting for it; `check` tells whether it
/// proves the peer's identity. Returns that.
fn answer(
    shared: &Shared,
    from: Ipv4Addr,
    seq: u32,
    received: u16,
    check: impl FnOnce(&Waiter) -> bool,
) -> bool {
    let waiter = shared
        .pending
        .lock()
        .ok()
        .and_then(|mut p| p.remove(&(from, seq)));
    let Some(waiter) = waiter else {
        return false;
    };
    let valid = check(&waiter);
    let _ = waiter.tx.send(Answer { received, valid });
    valid
}

/// The keys to check and answer a version 2 request of `peer` that claims
/// the identity `sender`: the keys of its known identity, or (if this site
/// does not know it yet) those of the claimed one. `None` if the claim
/// contradicts the known identity.
fn request_keys(
    shared: &Shared,
    me: &Identity,
    peer: &str,
    sender: SiteId,
) -> Option<Arc<PairKeys>> {
    let known = shared.config.borrow().keys.get(peer).cloned();
    match known {
        Some(k) if k.id == sender => Some(k.keys),
        Some(k) => {
            debug!(
                "a probe from the address of \"{peer}\" claims identity {}, not {}",
                sender.fingerprint(),
                k.id.fingerprint()
            );
            None
        }
        None => shared.claimed_keys(me, sender),
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
            entry.announced_at = Some(Instant::now());
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

/// The outcome of one probe.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    /// The complete probe arrived and the answer is valid.
    Answered(Duration),
    /// An answer came, but not from the box with the peer's key.
    WrongKey,
    /// No (complete) answer.
    Lost,
}

impl Outcome {
    fn rtt(self) -> Option<Duration> {
        match self {
            Self::Answered(rtt) => Some(rtt),
            Self::WrongKey | Self::Lost => None,
        }
    }
}

/// Sends one probe of `size` bytes (IP packet) to `name` and waits for the
/// reply: version 2 if the peer's identity is known, else version 1.
async fn probe_once(
    shared: &Shared,
    name: &str,
    peer: SocketAddrV4,
    size: u16,
    timeout: Duration,
) -> Outcome {
    let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
    let (announce, me, keys) = {
        let cfg = shared.config.borrow();
        (
            cfg.announce_of(name),
            cfg.me.clone(),
            cfg.keys.get(name).cloned(),
        )
    };
    let (packet, keys) = match (me, keys) {
        (Some(me), Some(k)) => (
            wire2::request(seq, size, &announce, &me.0.id(), &shared.nonce, &k.keys),
            Some((k.keys, k.id)),
        ),
        _ => (wire::request(seq, size, &announce), None),
    };
    let (tx, rx) = oneshot::channel();
    let key = (*peer.ip(), seq);
    if let Ok(mut p) = shared.pending.lock() {
        p.insert(key, Waiter { tx, keys });
    }
    // Removed again also when the prober is aborted while waiting.
    let _pending = PendingGuard { shared, key };
    let start = Instant::now();
    match shared.socket.send_to(&packet, peer).await {
        // EMSGSIZE: larger than the local interface MTU.
        Err(_) => Outcome::Lost,
        Ok(_) => match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) if !answer.valid => Outcome::WrongKey,
            Ok(Ok(answer)) if usize::from(answer.received) == packet.len() => {
                Outcome::Answered(start.elapsed())
            }
            _ => Outcome::Lost,
        },
    }
}

/// Removes a waiting probe when the wait ends, however it ends.
struct PendingGuard<'a> {
    shared: &'a Shared,
    key: PendingKey,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut p) = self.shared.pending.lock() {
            p.remove(&self.key);
        }
    }
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
    let (port, passive) = {
        let cfg = shared.config.borrow();
        (
            cfg.bind.map_or(0, |b| b.port()),
            cfg.passive.contains(&name),
        )
    };
    // A passive peer is probed only where it was confirmed.
    let mut target = if passive {
        shared
            .endpoints
            .lock()
            .ok()
            .and_then(|e| e.get(&name).copied())
    } else {
        Some(SocketAddrV4::new(addr, port))
    };
    let mut next_mtu: Option<Instant> = Some(Instant::now());
    let mut last_pmtu: Option<u16> = None;
    let mut generation = shared.config.borrow().generation;
    loop {
        let current = shared.config.borrow().generation;
        if current != generation {
            generation = current;
            next_mtu = Some(Instant::now());
        }
        if passive {
            let candidate = shared
                .candidates
                .lock()
                .ok()
                .and_then(|mut c| c.remove(&name));
            if let Some(candidate) = candidate.filter(|c| Some(*c) != target)
                && matches!(
                    probe_once(&shared, &name, candidate, RTT_SIZE, RTT_TIMEOUT).await,
                    Outcome::Answered(_)
                )
            {
                info!("Client \"{name}\" is at {candidate}");
                target = Some(candidate);
                if let Ok(mut e) = shared.endpoints.lock() {
                    e.insert(name.clone(), candidate);
                }
                next_mtu = Some(Instant::now());
                let _ = events
                    .send(ProbeEvent::Endpoint {
                        peer: name.clone(),
                        addr: candidate,
                    })
                    .await;
            }
        }
        let Some(target) = target else {
            // Not contacted yet: nothing is sent to it.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        let outcome = probe_once(&shared, &name, target, RTT_SIZE, RTT_TIMEOUT).await;
        let rtt = outcome.rtt();
        let became_reachable = update_reachability(&table, &name, rtt);
        if became_reachable {
            info!("Peer \"{name}\" ({addr}) is reachable");
            next_mtu = Some(Instant::now());
        }
        let keyed = shared.config.borrow().keys.contains_key(&name);
        if let Some(state) = identity_change(&table, &name, keyed, outcome) {
            match state {
                PeerIdentity::Confirmed => {
                    info!("Peer \"{name}\" ({addr}) proved its identity");
                }
                PeerIdentity::WrongKey => warn!(
                    "Something at {addr} answers the probes for \"{name}\", but not with its key"
                ),
                PeerIdentity::Waiting => {}
            }
            let _ = events
                .send(ProbeEvent::Identity {
                    peer: name.clone(),
                    state,
                })
                .await;
        }

        let mtu_interval = shared.config.borrow().mtu_interval;
        let due = next_mtu.is_some_and(|t| Instant::now() >= t);
        if let (Some(interval), true, Some(rtt)) = (mtu_interval, due, rtt) {
            // A loaded uplink answers late: wait at least twice the round trip.
            let timeout = (rtt * 2).clamp(MTU_TIMEOUT, MTU_TIMEOUT_MAX);
            let measured = mtu::search(mtu::PROBE_MIN, mtu::PROBE_MAX, |size| {
                let shared = shared.clone();
                let name = name.clone();
                async move {
                    for _ in 0..MTU_ATTEMPTS {
                        let outcome = probe_once(&shared, &name, target, size, timeout).await;
                        if outcome.rtt().is_some() {
                            return true;
                        }
                    }
                    false
                }
            })
            .await;
            // A failed measurement keeps the last good value (the MSS clamp
            // stays) and is tried again soon.
            let pmtu = measured.or(last_pmtu);
            let wait = if measured.is_some() {
                interval
            } else {
                MTU_RETRY.min(interval)
            };
            next_mtu = Instant::now().checked_add(wait);
            if let (None, Some(last)) = (measured, last_pmtu) {
                warn!("Path MTU to \"{name}\" could not be measured again; keeping {last} bytes");
            }
            if let Ok(mut t) = table.write() {
                let entry = t.entry(name.clone()).or_default();
                entry.pmtu = pmtu;
                entry.pmtu_measured = Some(SystemTime::now());
            }
            if pmtu != last_pmtu {
                let keyed = shared.config.borrow().keys.contains_key(&name);
                match pmtu {
                    Some(p) => info!(
                        "Path MTU to \"{name}\" is {p} bytes (TCP MSS {})",
                        mtu::mss_with(p, mtu::overhead(keyed, false))
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

/// Records what a probe found about the identity of a peer with a key.
/// Returns the new state if it changed: confirmed by the first valid answer
/// (for good, while the task runs), a wrong key only before that.
fn identity_change(
    table: &ProbeTable,
    name: &str,
    keyed: bool,
    outcome: Outcome,
) -> Option<PeerIdentity> {
    let Ok(mut t) = table.write() else {
        return None;
    };
    let entry = t.entry(name.to_string()).or_default();
    if !keyed {
        entry.identity = None;
        return None;
    }
    let current = entry.identity.unwrap_or_default();
    let next = match (current, outcome) {
        (PeerIdentity::Confirmed, _) => PeerIdentity::Confirmed,
        (_, Outcome::Answered(_)) => PeerIdentity::Confirmed,
        (_, Outcome::WrongKey) => PeerIdentity::WrongKey,
        (state, Outcome::Lost) => state,
    };
    let changed = entry.identity != Some(next);
    entry.identity = Some(next);
    changed.then_some(next)
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
            entry.last_reply_at = Some(Instant::now());
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

    /// A probe service on 127.0.0.1 with an identity and keys towards the
    /// peer "other" (with identity `expect`).
    async fn service(me: &Arc<Identity>, expect: Option<SiteId>) -> (Arc<Shared>, ProbeTable) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bind = match socket.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            SocketAddr::V6(_) => unreachable!(),
        };
        let mut keys = BTreeMap::new();
        if let Some(id) = expect {
            keys.insert(
                "other".to_string(),
                PeerKeys {
                    id,
                    keys: Arc::new(me.pair(&id).unwrap()),
                },
            );
        }
        let config = ProbeConfig {
            bind: Some(bind),
            peers: [("other".to_string(), Ipv4Addr::LOCALHOST)].into(),
            me: Some(Me(me.clone())),
            keys,
            ..ProbeConfig::default()
        };
        let (_tx, rx) = watch::channel(config);
        // The sender stays alive with the service (leaked for the test).
        std::mem::forget(_tx);
        let shared = Arc::new(Shared {
            socket,
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU32::new(1),
            config: rx,
            nonce: crate::random::bytes().unwrap(),
            claimed: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
            candidates: Mutex::new(HashMap::new()),
        });
        let table: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
        let (events, _) = mpsc::channel(16);
        tokio::spawn(receive(shared.clone(), table.clone(), events));
        (shared, table)
    }

    fn port(shared: &Shared) -> SocketAddrV4 {
        match shared.socket.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            SocketAddr::V6(_) => unreachable!(),
        }
    }

    #[tokio::test]
    async fn authenticated_probes_confirm_the_right_box_only() {
        let a = Arc::new(Identity::from_secret([1; 32]));
        let b = Arc::new(Identity::from_secret([2; 32]));
        let c = Arc::new(Identity::from_secret([3; 32]));
        let wait = Duration::from_millis(500);

        // Both know each other: answered.
        let (sa, _) = service(&a, Some(b.id())).await;
        let (sb, _) = service(&b, Some(a.id())).await;
        let out = probe_once(&sa, "other", port(&sb), RTT_SIZE, wait).await;
        assert!(matches!(out, Outcome::Answered(_)), "{out:?}");
        // A full-size MTU probe too.
        let out = probe_once(&sa, "other", port(&sb), mtu::PROBE_MAX, wait).await;
        assert!(matches!(out, Outcome::Answered(_)), "{out:?}");

        // b does not know a's identity yet: it answers with the claimed one.
        let (sb2, _) = service(&b, None).await;
        let out = probe_once(&sa, "other", port(&sb2), RTT_SIZE, wait).await;
        assert!(matches!(out, Outcome::Answered(_)), "{out:?}");

        // Another box (c) at the address a expects b at: a wrong key.
        let (sc, _) = service(&c, Some(a.id())).await;
        let out = probe_once(&sa, "other", port(&sc), RTT_SIZE, wait).await;
        assert_eq!(out, Outcome::WrongKey);
        let (sc2, _) = service(&c, None).await;
        let out = probe_once(&sa, "other", port(&sc2), RTT_SIZE, wait).await;
        assert_eq!(out, Outcome::WrongKey);

        // Without keys: version 1 between the same services.
        let (sv1, _) = service(&a, None).await;
        let out = probe_once(&sv1, "other", port(&sb), RTT_SIZE, wait).await;
        assert!(matches!(out, Outcome::Answered(_)), "{out:?}");
    }

    #[test]
    fn a_confirmed_peer_stays_confirmed() {
        let table: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
        let ok = Outcome::Answered(Duration::from_millis(1));
        assert_eq!(
            identity_change(&table, "a", true, Outcome::WrongKey),
            Some(PeerIdentity::WrongKey)
        );
        assert_eq!(identity_change(&table, "a", true, Outcome::Lost), None);
        assert_eq!(
            identity_change(&table, "a", true, ok),
            Some(PeerIdentity::Confirmed)
        );
        assert_eq!(identity_change(&table, "a", true, Outcome::WrongKey), None);
        assert_eq!(identity_change(&table, "a", false, ok), None);
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
