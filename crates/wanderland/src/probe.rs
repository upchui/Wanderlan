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
//!
//! The probes between a client and its server also run the test whether
//! the server reaches the client directly (see `wanderlan_core::nat`): the
//! client asks for a test frame now and then and listens for it at its
//! tunnel interface for a moment; the server sends it from a socket of its
//! own (not the tunnel port) and follows what the client says about it.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use wanderlan_core::identity::{Identity, PairKeys, PeerIdentity, SiteId};
use wanderlan_core::mtu::{self, wire, wire2};
use wanderlan_core::nat;

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
/// How long a client listens for the test frame of its server.
const TEST_WAIT: Duration = Duration::from_secs(2);
/// A test of a client whose server reaches it directly, to see that this
/// still holds.
const TEST_DIRECT: Duration = Duration::from_secs(60);
/// A test of a client whose server does not reach it directly: something
/// may have changed (a port opened, another network).
const TEST_NAT: Duration = Duration::from_secs(300);
/// After a client ran direct and its server no longer reached it, the pause
/// before the next test doubles, up to this (a NAT that keeps the tunnel
/// port open only while the client sends from it would make it flap).
const TEST_NAT_MAX: Duration = Duration::from_secs(24 * 3600);
/// A test that could not be made (the tunnel interface is down): again
/// after this.
const TEST_LATER: Duration = Duration::from_secs(30);
/// Failed tests in a row after which a client that ran direct goes back.
const TEST_FAILURES: u32 = 2;
/// A server sends at most one test frame per client this often.
const TEST_MIN_GAP: Duration = Duration::from_secs(20);

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
    /// This site's tunnel port: the kernel takes it (and the one above, the
    /// source of a tunnel through NAT) when a tunnel comes up, so the
    /// socket for the tests of the client tunnels never does.
    pub tunnel_port: Option<u16>,
    /// Direct peers by name: where their daemon listens (each site has its
    /// own control port).
    pub peers: BTreeMap<SiteId, SocketAddrV4>,
    /// Further sites whose probes are answered but which are not probed
    /// (peers removed recently, so they can still fetch the new network).
    pub responders: BTreeMap<SiteId, Ipv4Addr>,
    /// Incremented to request an immediate path MTU measurement.
    pub generation: u64,
    /// Interval between path MTU measurements; `None` disables them (fixed MSS).
    pub mtu_interval: Option<Duration>,
    /// What this site announces (network revision and fingerprint).
    pub announce: wire::Announce,
    /// What this site announces to single peers instead: to a client the
    /// version of the client's view of the network.
    pub announce_to: BTreeMap<SiteId, wire::Announce>,
    /// This box's identity; without it only version 1 probes.
    pub me: Option<Me>,
    /// The keys towards the peers whose identity is known: probed with
    /// version 2; every other peer with version 1.
    pub keys: BTreeMap<SiteId, PeerKeys>,
    /// Peers this site never sends anything to first (its clients with a
    /// key, maybe behind NAT): probed only where their authenticated probes
    /// come from, once that place answered a challenge.
    pub passive: BTreeSet<SiteId>,
    /// The VNIs and tunnel ports of the passive peers: where their test
    /// frames go.
    pub tests: BTreeMap<SiteId, (u32, u16)>,
    /// The passive peers whose tunnels run direct, at this address.
    pub direct: BTreeMap<SiteId, Ipv4Addr>,
    /// For a client: its uplink, tested through the tunnel to it.
    pub uplink: Option<UplinkTunnel>,
    /// How the peers are called, for messages.
    pub names: BTreeMap<SiteId, String>,
    /// The ports the other networks of the box take.
    pub other_ports: BTreeSet<u16>,
}

/// The tunnel of a client to its uplink, for the test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UplinkTunnel {
    /// The uplink.
    pub peer: SiteId,
    /// The tunnel interface, where the test frame comes out.
    pub interface: String,
    /// The tunnel runs direct now.
    pub direct: bool,
}

impl ProbeConfig {
    /// The ports of this site no other socket of this daemon may take.
    fn own_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self.bind.map(|b| b.port()).into_iter().collect();
        if let Some(tunnel) = self.tunnel_port {
            ports.push(tunnel);
            ports.push(tunnel.saturating_add(1));
        }
        ports.extend(self.other_ports.iter().copied());
        ports
    }

    /// What to announce to `peer`.
    fn announce_of(&self, peer: SiteId) -> wire::Announce {
        self.announce_to
            .get(&peer)
            .copied()
            .unwrap_or(self.announce)
    }

    /// How `peer` is called in messages.
    pub fn name_of(&self, peer: SiteId) -> String {
        self.names
            .get(&peer)
            .cloned()
            .unwrap_or_else(|| peer.fingerprint())
    }

    /// The peer (or responder, `false`) at `ip`.
    fn peer_at(&self, ip: Ipv4Addr) -> Option<(SiteId, bool)> {
        self.peers
            .iter()
            .find(|(_, a)| *a.ip() == ip)
            .map(|(n, _)| (*n, true))
            .or_else(|| {
                self.responders
                    .iter()
                    .find(|(_, a)| **a == ip)
                    .map(|(n, _)| (*n, false))
            })
    }

    /// The peer with identity `id`.
    fn peer_with(&self, id: SiteId) -> Option<SiteId> {
        self.keys.iter().find(|(_, k)| k.id == id).map(|(n, _)| *n)
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
    /// The test whether the server of a client tunnel reaches the client
    /// directly (see the module doc).
    #[serde(skip)]
    pub direct_test: Option<DirectTest>,
    /// Where the authenticated probes of the peer come from: its daemon (its
    /// socket is bound to its control port), also right after it moved to
    /// another port.
    #[serde(skip)]
    pub control_at: Option<SocketAddrV4>,
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

/// The test of a client tunnel, as far as the status shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirectTest {
    /// A test was made (by the client) or a test frame sent (by the server).
    pub tested: bool,
    /// The last tests reached the client.
    pub reached: bool,
    /// At the client: when it tests next.
    pub next: Option<Instant>,
}

/// Shared measurement table.
pub type ProbeTable = Arc<RwLock<BTreeMap<SiteId, PeerProbe>>>;

/// Events for the controller.
#[derive(Debug)]
pub enum ProbeEvent {
    /// The measured path MTU of a peer changed.
    Pmtu { peer: SiteId, pmtu: Option<u16> },
    /// A peer announced a network revision.
    Announce {
        peer: SiteId,
        announce: wire::Announce,
    },
    /// The probes found something about the identity of a peer with a key:
    /// it answered with a valid tag (confirmed), or with a wrong one.
    Identity { peer: SiteId, state: PeerIdentity },
    /// A passive peer (a client) answered a challenge at this address: its
    /// control traffic comes from there (its NAT's address and port).
    Endpoint { peer: SiteId, addr: SocketAddrV4 },
    /// The tunnel of a client and its server runs direct (at that address
    /// of the peer) or through NAT (`None`).
    Direct { peer: SiteId, at: Option<Ipv4Addr> },
    /// The daemon of a peer started again (its probes carry a new nonce).
    Restarted { peer: SiteId },
}

/// A probe waiting for its reply: from where it was sent to, with that
/// sequence number.
type PendingKey = (SocketAddrV4, u32);

/// A sent probe: where its answer goes, and how to check it.
struct Waiter {
    tx: Option<oneshot::Sender<Answer>>,
    /// For a version 2 probe: the keys and the identity of the peer.
    keys: Option<(Arc<PairKeys>, SiteId)>,
    /// An answer came that does not prove the peer's identity. The probe
    /// still waits for a valid one (a forged answer does not end it).
    wrong: Arc<AtomicBool>,
}

/// The valid answer to a probe.
struct Answer {
    received: u16,
    /// Its flags (version 2).
    flags: u8,
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
    endpoints: Mutex<HashMap<SiteId, SocketAddrV4>>,
    /// Where authenticated requests of passive peers came from lately, to be
    /// confirmed by a challenge.
    candidates: Mutex<HashMap<SiteId, SocketAddrV4>>,
    /// The socket the test frames leave from (not the tunnel port).
    test_socket: Option<UdpSocket>,
    /// The last test frame per passive peer: to which address, when, for
    /// which run of the peer (its nonce).
    tests: Mutex<HashMap<SiteId, (Ipv4Addr, Instant, wire2::Nonce)>>,
    /// What was last reported per peer as [`ProbeEvent::Direct`].
    reported: Mutex<HashMap<SiteId, Option<Ipv4Addr>>>,
    /// The nonce of the last verified request per peer: a new one means
    /// that its daemon started again.
    nonces: Mutex<HashMap<SiteId, wire2::Nonce>>,
    /// The round trips of the last hour, for the graphs.
    history: crate::history::SharedHistory,
}

impl Shared {
    /// Forgets the endpoints, candidates, tests and reports of every peer
    /// for which `keep` is false.
    fn forget_all_but(&self, keep: impl Fn(&SiteId) -> bool) {
        if let Ok(mut m) = self.endpoints.lock() {
            m.retain(|n, _| keep(n));
        }
        if let Ok(mut m) = self.candidates.lock() {
            m.retain(|n, _| keep(n));
        }
        if let Ok(mut m) = self.tests.lock() {
            m.retain(|n, _| keep(n));
        }
        if let Ok(mut m) = self.reported.lock() {
            m.retain(|n, _| keep(n));
        }
        if let Ok(mut m) = self.nonces.lock() {
            m.retain(|n, _| keep(n));
        }
    }

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

/// How `peer` is called in messages.
fn label(shared: &Shared, peer: SiteId) -> String {
    shared.config.borrow().name_of(peer)
}

/// Starts the probe service. It follows changes of the configuration.
pub fn spawn(
    config: watch::Receiver<ProbeConfig>,
    table: ProbeTable,
    events: mpsc::Sender<ProbeEvent>,
    history: crate::history::SharedHistory,
) -> JoinHandle<()> {
    tokio::spawn(run(config, table, events, history))
}

/// How often the probe tasks are checked.
const SUPERVISE: Duration = Duration::from_secs(10);

async fn run(
    mut config: watch::Receiver<ProbeConfig>,
    table: ProbeTable,
    events: mpsc::Sender<ProbeEvent>,
    history: crate::history::SharedHistory,
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
        let avoid = config.borrow().own_ports();
        let test_socket = match test_socket(&avoid).await {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("Cannot open the socket for the tests of the client tunnels: {e}");
                None
            }
        };
        let test_port = test_socket
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port());
        let shared = Arc::new(Shared {
            socket,
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU32::new(1),
            config: config.clone(),
            nonce,
            claimed: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
            candidates: Mutex::new(HashMap::new()),
            test_socket,
            tests: Mutex::new(HashMap::new()),
            reported: Mutex::new(HashMap::new()),
            nonces: Mutex::new(HashMap::new()),
            history: history.clone(),
        });
        let receiver = tokio::spawn(receive(shared.clone(), table.clone(), events.clone()));
        let mut prober: BTreeMap<SiteId, (Target, JoinHandle<()>)> = BTreeMap::new();
        let mut gone = false;
        loop {
            let current = config.borrow_and_update().clone();
            // A new address, or a tunnel port that the test socket has.
            if current.bind != Some(bind)
                || test_port.is_some_and(|p| current.own_ports().contains(&p))
            {
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
        // The old tasks hold the sockets: they end before the next bind
        // (else it may find the address still in use).
        receiver.abort();
        let _ = receiver.await;
        for (_, (_, task)) in prober {
            task.abort();
            let _ = task.await;
        }
        drop(shared);
        // Without a listening address (reset, unconfigured) or with a new
        // one, nothing measured or announced before is valid any more.
        if let Ok(mut t) = table.write() {
            t.clear();
        }
    }
}

/// A socket on a free port, never on one of `avoid`: port 0 may well get
/// this site's tunnel port (both lie in the range of free ports), and a
/// tunnel could then not come up ("Address in use").
async fn test_socket(avoid: &[u16]) -> std::io::Result<UdpSocket> {
    // The rejected ones stay open until the end, so the next is another.
    let mut rejected = Vec::new();
    for _ in 0..8 {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        if !avoid.contains(&socket.local_addr()?.port()) {
            return Ok(socket);
        }
        rejected.push(socket);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "no free port besides those of this site",
    ))
}

async fn bind_socket(bind: SocketAddrV4) -> anyhow::Result<UdpSocket> {
    let socket = std::net::UdpSocket::bind(bind)?;
    crate::sockopt::set_pmtudisc_probe(&socket)?;
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket)?)
}

/// What a probe task is for: the peer's address and identity.
/// Where a peer is probed, with which key, and whether only where its
/// requests come from (a passive client): a change starts its prober anew.
type Target = (SocketAddrV4, Option<SiteId>, bool);

/// Starts and stops the per-peer tasks to match the configured peers. A
/// peer with another address or identity gets a new task and starts over.
fn sync_probers(
    probers: &mut BTreeMap<SiteId, (Target, JoinHandle<()>)>,
    config: &ProbeConfig,
    shared: &Arc<Shared>,
    table: &ProbeTable,
    events: &mpsc::Sender<ProbeEvent>,
) {
    let target = |name: &SiteId| {
        config.peers.get(name).map(|addr| {
            (
                *addr,
                config.keys.get(name).map(|k| k.id),
                config.passive.contains(name),
            )
        })
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
    // What was found out about a peer that is gone is forgotten: back, it
    // is tested anew (and told so) instead of keeping an old result.
    shared.forget_all_but(|name| config.peers.contains_key(name));
    for (name, addr) in &config.peers {
        if probers.contains_key(name) {
            continue;
        }
        let task = tokio::spawn(probe_peer(
            shared.clone(),
            *name,
            *addr,
            table.clone(),
            events.clone(),
        ));
        if let Some(current) = target(name) {
            probers.insert(*name, (current, task));
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
                .map_or(cfg.announce, |(n, _)| cfg.announce_of(*n));
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
                    // Unauthenticated: it counts only for a peer without a
                    // key (anyone can send it from the peer's address).
                    let keyed = shared.config.borrow().keys.contains_key(&peer);
                    if direct && !keyed {
                        note_announce(&table, &events, peer, announce);
                    }
                }
                wire::Probe::Reply {
                    seq,
                    received,
                    announce,
                } => {
                    // A version 1 answer proves nothing about the identity.
                    let valid = answer(&shared, from, seq, received, 0, |w| w.keys.is_none());
                    if direct && valid {
                        note_announce(&table, &events, peer, announce);
                    }
                }
            }
        } else if let Some(msg) = v2 {
            match msg {
                wire2::Probe::Request(request) => {
                    let me = shared.config.borrow().me.clone();
                    let Some(me) = me else { continue };
                    let Some(keys) = request_keys(&shared, &me.0, peer, request.sender) else {
                        continue;
                    };
                    // A wrong tag is answered too (the reply is smaller than
                    // the request): the requester sees that this box has
                    // another key than it expects here.
                    let verified = wire2::verify_request(packet, &keys);
                    let mut flags = if verified {
                        0
                    } else {
                        debug!("probe from {from} with a tag of another key");
                        wire2::FLAG_UNVERIFIED
                    };
                    if passive
                        && verified
                        && serve_direct(&shared, &table, &events, peer, from, &request, &keys).await
                    {
                        flags |= wire2::FLAG_DIRECT;
                    }
                    let reply = wire2::reply(&request, flags, &announce_self, &me.0.id(), &keys);
                    if let Err(e) = shared.socket.send_to(&reply, from).await {
                        debug!("cannot answer probe from {from}: {e}");
                    }
                    if direct && verified {
                        note_announce(&table, &events, peer, request.announce);
                        note_nonce(&shared, &events, peer, request.nonce);
                        if !passive && let Ok(mut t) = table.write() {
                            t.entry(peer).or_default().control_at = Some(from);
                        }
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
                            c.insert(peer, from);
                        }
                    }
                }
                wire2::Probe::Reply(reply) => {
                    let (seq, received, flags) = (reply.seq, reply.received, reply.flags);
                    let valid = answer(&shared, from, seq, received, flags, |w| {
                        w.keys.as_ref().is_some_and(|(keys, id)| {
                            reply.responder == *id
                                && reply.flags & wire2::FLAG_UNVERIFIED == 0
                                && wire2::verify_reply(packet, &shared.nonce, keys)
                        })
                    });
                    if direct && valid {
                        note_announce(&table, &events, peer, reply.announce);
                    }
                }
            }
        }
    }
}

/// Hands an answer to the probe waiting for it if `check` says that it
/// proves the peer's identity; otherwise the probe notes it and waits on.
/// Returns whether it was valid.
fn answer(
    shared: &Shared,
    from: SocketAddrV4,
    seq: u32,
    received: u16,
    flags: u8,
    check: impl FnOnce(&Waiter) -> bool,
) -> bool {
    let Ok(mut pending) = shared.pending.lock() else {
        return false;
    };
    let Some(waiter) = pending.get_mut(&(from, seq)) else {
        return false;
    };
    if !check(waiter) {
        waiter.wrong.store(true, Ordering::Relaxed);
        return false;
    }
    if let Some(tx) = waiter.tx.take() {
        let _ = tx.send(Answer { received, flags });
    }
    pending.remove(&(from, seq));
    true
}

/// Notes the nonce of a verified request of `peer`: another one than
/// before means that its daemon started again.
fn note_nonce(
    shared: &Shared,
    events: &mpsc::Sender<ProbeEvent>,
    peer: SiteId,
    nonce: wire2::Nonce,
) {
    let Ok(mut nonces) = shared.nonces.lock() else {
        return;
    };
    if let Some(before) = nonces.insert(peer, nonce)
        && before != nonce
    {
        let _ = events.try_send(ProbeEvent::Restarted { peer });
    }
}

/// A verified request of a client of this site: sends the test frame it
/// asks for and follows what it says about the last one. Returns whether
/// the tunnel to it runs direct now: the client says that the test frame
/// sent to the address the request comes from reached it.
async fn serve_direct(
    shared: &Shared,
    table: &ProbeTable,
    events: &mpsc::Sender<ProbeEvent>,
    peer: SiteId,
    from: SocketAddrV4,
    request: &wire2::Request,
    keys: &PairKeys,
) -> bool {
    let (test, running) = {
        let cfg = shared.config.borrow();
        (
            cfg.tests.get(&peer).copied(),
            cfg.direct.get(&peer).copied(),
        )
    };
    let (vni, port) = (test.map(|(v, _)| v), test.map_or(0, |(_, p)| p));
    let now = Instant::now();
    let (packet, tested) = {
        let Ok(mut tests) = shared.tests.lock() else {
            return false;
        };
        // One test per gap and run of the client (a restarted client is
        // tested at once).
        let recent = tests.get(&peer).is_some_and(|(_, at, nonce)| {
            *nonce == request.nonce && now.duration_since(*at) < TEST_MIN_GAP
        });
        let packet = match vni {
            Some(vni) if request.flags & wire2::FLAG_TEST_ME != 0 && port != 0 && !recent => {
                tests.insert(peer, (*from.ip(), now, request.nonce));
                Some(nat::direct_test_packet(
                    vni,
                    keys,
                    &request.nonce,
                    request.seq,
                ))
            }
            _ => None,
        };
        (packet, tests.get(&peer).map(|(ip, ..)| *ip))
    };
    if let (Some(packet), Some(socket)) = (packet, &shared.test_socket) {
        let to = SocketAddrV4::new(*from.ip(), port);
        match socket.send_to(&packet, to).await {
            Ok(_) => debug!(
                "test frame for client \"{}\" sent to {to}",
                label(shared, peer)
            ),
            Err(e) => debug!(
                "cannot send the test frame for client \"{}\" to {to}: {e}",
                label(shared, peer)
            ),
        }
    }
    // What runs direct stays so while the client still says that it is
    // reached from the same address (after a restart of this daemon too).
    let ip = *from.ip();
    let claimed = request.flags & wire2::FLAG_REACHED != 0;
    let at = (claimed && (tested == Some(ip) || running == Some(ip))).then_some(ip);
    if let Ok(mut t) = table.write() {
        t.entry(peer).or_default().direct_test = Some(DirectTest {
            tested: tested.is_some() || running.is_some(),
            reached: at.is_some(),
            next: None,
        });
    }
    if at != running {
        report_direct(shared, events, peer, at);
    }
    at.is_some()
}

/// Tells the controller how a client tunnel runs now (once per change; a
/// full queue is tried again with the next probe).
fn report_direct(
    shared: &Shared,
    events: &mpsc::Sender<ProbeEvent>,
    peer: SiteId,
    at: Option<Ipv4Addr>,
) {
    let Ok(mut reported) = shared.reported.lock() else {
        return;
    };
    if reported.get(&peer) == Some(&at) {
        return;
    }
    let event = ProbeEvent::Direct { peer, at };
    if events.try_send(event).is_ok() {
        reported.insert(peer, at);
    }
}

/// The keys to check and answer a version 2 request of `peer` that claims
/// the identity `sender`: the keys of its known identity, or (if this site
/// does not know it yet) those of the claimed one. `None` if the claim
/// contradicts the known identity.
fn request_keys(
    shared: &Shared,
    me: &Identity,
    peer: SiteId,
    sender: SiteId,
) -> Option<Arc<PairKeys>> {
    let known = shared.config.borrow().keys.get(&peer).cloned();
    match known {
        Some(k) if k.id == sender => Some(k.keys),
        Some(k) => {
            debug!(
                "a probe from the address of \"{}\" claims identity {}, not {}",
                label(shared, peer),
                sender.fingerprint(),
                k.id.fingerprint()
            );
            None
        }
        None => shared.claimed_keys(me, sender),
    }
}

/// Notes what a peer announced; a change is told to the controller unless
/// its queue is full (the table holds it, the controller looks every
/// second): the answers to the probes never wait for the controller.
fn note_announce(
    table: &ProbeTable,
    events: &mpsc::Sender<ProbeEvent>,
    peer: SiteId,
    announce: wire::Announce,
) {
    let changed = match table.write() {
        Ok(mut t) => {
            let entry = t.entry(peer).or_default();
            let changed = entry.announce != Some(announce);
            entry.announce = Some(announce);
            entry.announced_at = Some(Instant::now());
            changed
        }
        Err(_) => false,
    };
    if changed {
        let _ = events.try_send(ProbeEvent::Announce { peer, announce });
    }
}

/// The outcome of one probe.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    /// The complete probe arrived and the answer is valid (with its flags).
    Answered(Duration, u8),
    /// An answer came, but not from the box with the peer's key.
    WrongKey,
    /// No (complete) answer.
    Lost,
}

impl Outcome {
    fn rtt(self) -> Option<Duration> {
        match self {
            Self::Answered(rtt, _) => Some(rtt),
            Self::WrongKey | Self::Lost => None,
        }
    }
}

/// Sends one probe of `size` bytes (IP packet) to `name` and waits for the
/// reply: version 2 if the peer's identity is known, else version 1.
async fn probe_once(
    shared: &Shared,
    name: SiteId,
    peer: SocketAddrV4,
    size: u16,
    timeout: Duration,
) -> Outcome {
    let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
    probe_seq(shared, name, peer, size, timeout, 0, seq).await
}

/// [`probe_once`] with the flags and the sequence number of the request.
async fn probe_seq(
    shared: &Shared,
    name: SiteId,
    peer: SocketAddrV4,
    size: u16,
    timeout: Duration,
    flags: u8,
    seq: u32,
) -> Outcome {
    let (announce, me, keys) = {
        let cfg = shared.config.borrow();
        (
            cfg.announce_of(name),
            cfg.me.clone(),
            cfg.keys.get(&name).cloned(),
        )
    };
    let (packet, keys) = match (me, keys) {
        (Some(me), Some(k)) => (
            wire2::request(
                seq,
                size,
                flags,
                &announce,
                &me.0.id(),
                &shared.nonce,
                &k.keys,
            ),
            Some((k.keys, k.id)),
        ),
        _ => (wire::request(seq, size, &announce), None),
    };
    let (tx, rx) = oneshot::channel();
    let key = (peer, seq);
    let wrong = Arc::new(AtomicBool::new(false));
    if let Ok(mut p) = shared.pending.lock() {
        p.insert(
            key,
            Waiter {
                tx: Some(tx),
                keys,
                wrong: wrong.clone(),
            },
        );
    }
    // Removed again also when the prober is aborted while waiting.
    let _pending = PendingGuard { shared, key };
    let start = Instant::now();
    match shared.socket.send_to(&packet, peer).await {
        // EMSGSIZE: larger than the local interface MTU.
        Err(_) => Outcome::Lost,
        Ok(_) => match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) if usize::from(answer.received) == packet.len() => {
                Outcome::Answered(start.elapsed(), answer.flags)
            }
            _ if wrong.load(Ordering::Relaxed) => Outcome::WrongKey,
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
    name: SiteId,
    to: SocketAddrV4,
    table: ProbeTable,
    events: mpsc::Sender<ProbeEvent>,
) {
    let addr = *to.ip();
    let passive = shared.config.borrow().passive.contains(&name);
    // A passive peer is probed only where it was confirmed.
    let mut place = if passive {
        shared
            .endpoints
            .lock()
            .ok()
            .and_then(|e| e.get(&name).copied())
    } else {
        Some(to)
    };
    let mut next_mtu: Option<Instant> = Some(Instant::now());
    let mut last_pmtu: Option<u16> = None;
    let mut generation = shared.config.borrow().generation;
    // At a client, towards its uplink: the test of the tunnel.
    let mut test: Option<UplinkTest> = None;
    let mut lost_in_row = 0u32;
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
            if let Some(candidate) = candidate.filter(|c| Some(*c) != place)
                && matches!(
                    probe_once(&shared, name, candidate, RTT_SIZE, RTT_TIMEOUT).await,
                    Outcome::Answered(..)
                )
            {
                info!("Client \"{}\" is at {candidate}", label(&shared, name));
                place = Some(candidate);
                if let Ok(mut e) = shared.endpoints.lock() {
                    e.insert(name, candidate);
                }
                next_mtu = Some(Instant::now());
                let _ = events
                    .send(ProbeEvent::Endpoint {
                        peer: name,
                        addr: candidate,
                    })
                    .await;
            }
        }
        let Some(target) = place else {
            // Not contacted yet: nothing is sent to it.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        let uplink = shared
            .config
            .borrow()
            .uplink
            .clone()
            .filter(|u| u.peer == name);
        match (&uplink, &test) {
            (Some(u), None) => test = Some(UplinkTest::new(u.direct)),
            (None, Some(_)) => test = None,
            _ => {}
        }
        let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
        let mut flags = test.as_ref().map_or(0, UplinkTest::flags);
        let mut tap = None;
        if let (Some(u), Some(t)) = (&uplink, test.as_mut())
            && Instant::now() >= t.next
        {
            match open_test_tap(&shared, name, &u.interface, seq) {
                Ok(wait) => {
                    flags |= wire2::FLAG_TEST_ME;
                    tap = Some(wait);
                }
                Err(why) => t.untestable(why, Instant::now()),
            }
        }
        let outcome = probe_seq(&shared, name, target, RTT_SIZE, RTT_TIMEOUT, flags, seq).await;
        if let (Some(u), Some(t)) = (&uplink, test.as_mut()) {
            let seen = match tap {
                Some(wait) => Some(wait.await.unwrap_or(false)),
                None => None,
            };
            let now = Instant::now();
            if let Some(seen) = seen {
                match t.result(seen, now) {
                    Some(true) => info!(
                        "\"{}\" reaches this site directly (its test frame came through the tunnel port)",
                        label(&shared, name)
                    ),
                    Some(false) => info!(
                        "\"{}\" no longer reaches this site directly; next test in {} min",
                        label(&shared, name),
                        t.pause.as_secs() / 60
                    ),
                    None => {}
                }
            }
            if let Outcome::Answered(_, reply_flags) = outcome {
                t.answer(reply_flags, now);
            }
            if let Ok(mut table) = table.write() {
                table.entry(name).or_default().direct_test = Some(t.status());
            }
            let at = t.direct().then_some(addr);
            if at.is_some() != u.direct {
                report_direct(&shared, &events, name, at);
            }
        }
        let flags = test.as_ref().map_or(0, UplinkTest::flags);
        let rtt = outcome.rtt();
        lost_in_row = if rtt.is_some() { 0 } else { lost_in_row + 1 };
        // A client that does not answer at its last mapping any more is
        // probed again only once it came back (a new verified request).
        if passive && lost_in_row >= LOST_LIMIT {
            place = None;
            lost_in_row = 0;
            if let Ok(mut e) = shared.endpoints.lock() {
                e.remove(&name);
            }
        }
        let became_reachable = update_reachability(&table, name, &label(&shared, name), rtt);
        // For the graphs (kept apart from the table, which is copied often).
        if let Ok(mut h) = shared.history.write() {
            #[allow(clippy::cast_possible_truncation)]
            h.record_rtt(
                name,
                crate::history::now_ms(),
                rtt.map(|r| (r.as_secs_f64() * 1000.0) as f32),
            );
        }
        if became_reachable {
            info!("Peer \"{}\" ({addr}) is reachable", label(&shared, name));
            next_mtu = Some(Instant::now());
        }
        let keyed = shared.config.borrow().keys.contains_key(&name);
        if let Some(state) = identity_change(&table, name, keyed, outcome) {
            match state {
                PeerIdentity::Confirmed => {
                    info!(
                        "Peer \"{}\" ({addr}) proved its identity",
                        label(&shared, name)
                    );
                }
                PeerIdentity::WrongKey => warn!(
                    "Something at {addr} answers the probes for \"{}\", but not with its key",
                    label(&shared, name)
                ),
                PeerIdentity::Waiting => {}
            }
            let _ = events
                .send(ProbeEvent::Identity { peer: name, state })
                .await;
        }

        let mtu_interval = shared.config.borrow().mtu_interval;
        let due = next_mtu.is_some_and(|t| Instant::now() >= t);
        if let (Some(interval), true, Some(rtt)) = (mtu_interval, due, rtt) {
            // A loaded uplink answers late: wait at least twice the round trip.
            let timeout = (rtt * 2).clamp(MTU_TIMEOUT, MTU_TIMEOUT_MAX);
            let measured = mtu::search(mtu::PROBE_MIN, mtu::PROBE_MAX, |size| {
                let shared = shared.clone();
                async move {
                    for _ in 0..MTU_ATTEMPTS {
                        let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
                        let outcome =
                            probe_seq(&shared, name, target, size, timeout, flags, seq).await;
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
                warn!(
                    "Path MTU to \"{}\" could not be measured again; keeping {last} bytes",
                    label(&shared, name)
                );
            }
            if let Ok(mut t) = table.write() {
                let entry = t.entry(name).or_default();
                entry.pmtu = pmtu;
                entry.pmtu_measured = Some(SystemTime::now());
            }
            if pmtu != last_pmtu {
                match pmtu {
                    Some(p) => info!(
                        "Path MTU to \"{}\" is {p} bytes (TCP MSS {})",
                        label(&shared, name),
                        mtu::mss_for_pmtu(p)
                    ),
                    None => warn!(
                        "Path MTU to \"{}\" could not be measured",
                        label(&shared, name)
                    ),
                }
                last_pmtu = pmtu;
                let _ = events.send(ProbeEvent::Pmtu { peer: name, pmtu }).await;
            }
        }
        tokio::time::sleep(RTT_INTERVAL).await;
    }
}

/// The test of the tunnel to the uplink, at the client.
#[derive(Debug)]
struct UplinkTest {
    /// The last tests reached this site.
    reached: bool,
    /// The server says in its answers that it runs the tunnel direct.
    server_direct: bool,
    /// A test was made.
    tested: bool,
    /// Failed tests in a row.
    failures: u32,
    /// When to test next.
    next: Instant,
    /// When the last test was made (the server answers one test in
    /// [`TEST_MIN_GAP`]).
    last: Option<Instant>,
    /// The pause between tests while not reached.
    pause: Duration,
    /// Why the last test could not be made (logged once).
    problem: Option<String>,
}

impl UplinkTest {
    /// `direct`: the tunnel runs direct already (after a restart).
    fn new(direct: bool) -> Self {
        Self {
            reached: direct,
            server_direct: direct,
            tested: false,
            failures: 0,
            next: Instant::now(),
            last: None,
            pause: TEST_NAT,
            problem: None,
        }
    }

    /// The flags of every request to the uplink.
    fn flags(&self) -> u8 {
        if self.reached { wire2::FLAG_REACHED } else { 0 }
    }

    /// The tunnel runs direct: both ends say so.
    fn direct(&self) -> bool {
        self.reached && self.server_direct
    }

    fn status(&self) -> DirectTest {
        DirectTest {
            tested: self.tested,
            reached: self.reached,
            next: Some(self.next),
        }
    }

    /// No test could be made now.
    fn untestable(&mut self, why: String, now: Instant) {
        if self.problem.as_ref() != Some(&why) {
            debug!("No test of the tunnel to the uplink: {why}");
            self.problem = Some(why);
        }
        self.next = now + TEST_LATER;
    }

    /// Takes the result of a test. Returns the new state if it changed.
    fn result(&mut self, seen: bool, now: Instant) -> Option<bool> {
        self.tested = true;
        self.problem = None;
        self.last = Some(now);
        if seen {
            self.failures = 0;
            self.next = now + TEST_DIRECT;
            let changed = !self.reached;
            self.reached = true;
            return changed.then_some(true);
        }
        self.failures += 1;
        if self.reached && self.failures >= TEST_FAILURES {
            self.reached = false;
            self.pause = (self.pause * 2).min(TEST_NAT_MAX);
            self.next = now + self.pause;
            return Some(false);
        }
        // Reached until now: soon again, to know.
        self.next = now + if self.reached { TEST_LATER } else { self.pause };
        None
    }

    /// Takes the flags of a valid answer of the server. A server that no
    /// longer runs the tunnel direct (it sees this site at another address,
    /// it was reset) gets a new test soon.
    fn answer(&mut self, flags: u8, now: Instant) {
        let direct = flags & wire2::FLAG_DIRECT != 0;
        if self.server_direct && !direct && self.reached {
            let earliest = self
                .last
                .map_or(now, |l| l + TEST_MIN_GAP + Duration::from_secs(1));
            self.next = self.next.min(earliest.max(now));
        }
        self.server_direct = direct;
    }
}

/// Opens the tap for a test of the tunnel to the uplink `name` with the
/// request `seq`: the wait for the test frame, or why no test can be made.
fn open_test_tap(
    shared: &Shared,
    name: SiteId,
    interface: &str,
    seq: u32,
) -> Result<JoinHandle<bool>, String> {
    let keys = shared
        .config
        .borrow()
        .keys
        .get(&name)
        .map(|k| k.keys.clone())
        .ok_or_else(|| format!("the identity of \"{}\" is not known", label(shared, name)))?;
    if !interface_up(interface) {
        return Err(format!("{interface} is down"));
    }
    let tap = crate::rawframe::Tap::open(
        interface,
        wanderlan_core::sys::nft_ip::KEEPALIVE_ETHERTYPE,
        *nat::DIRECT_MAGIC,
    )
    .map_err(|e| format!("cannot listen on {interface}: {e}"))?;
    let nonce = shared.nonce;
    Ok(tokio::task::spawn_blocking(move || {
        tap.wait(TEST_WAIT, |frame| {
            nat::verify_direct_frame(frame, &keys, &nonce, seq)
        })
    }))
}

/// True if the interface exists and is up.
fn interface_up(interface: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{interface}/flags"))
        .ok()
        .and_then(|t| u32::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
        .is_some_and(|flags| flags & 1 != 0)
}

/// Records what a probe found about the identity of a peer with a key.
/// Returns the new state if it changed: confirmed by the first valid answer
/// (for good, while the task runs), a wrong key only before that.
fn identity_change(
    table: &ProbeTable,
    name: SiteId,
    keyed: bool,
    outcome: Outcome,
) -> Option<PeerIdentity> {
    let Ok(mut t) = table.write() else {
        return None;
    };
    let entry = t.entry(name).or_default();
    if !keyed {
        entry.identity = None;
        return None;
    }
    let current = entry.identity.unwrap_or_default();
    let next = match (current, outcome) {
        (PeerIdentity::Confirmed, _) => PeerIdentity::Confirmed,
        (_, Outcome::Answered(..)) => PeerIdentity::Confirmed,
        (_, Outcome::WrongKey) => PeerIdentity::WrongKey,
        (state, Outcome::Lost) => state,
    };
    let changed = entry.identity != Some(next);
    entry.identity = Some(next);
    changed.then_some(next)
}

/// Records the result of a reachability probe. Returns true if the peer
/// just became reachable.
fn update_reachability(table: &ProbeTable, name: SiteId, who: &str, rtt: Option<Duration>) -> bool {
    let Ok(mut t) = table.write() else {
        return false;
    };
    let entry = t.entry(name).or_default();
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
                warn!("Peer \"{who}\" does not answer control probes");
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

    /// The peer of the test services.
    const OTHER: SiteId = SiteId::from_bytes([9; 32]);
    /// A peer of the tables.
    const A: SiteId = SiteId::from_bytes([10; 32]);

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
                OTHER,
                PeerKeys {
                    id,
                    keys: Arc::new(me.pair(&id).unwrap()),
                },
            );
        }
        let config = ProbeConfig {
            bind: Some(bind),
            peers: [(OTHER, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))].into(),
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
            test_socket: None,
            tests: Mutex::new(HashMap::new()),
            reported: Mutex::new(HashMap::new()),
            nonces: Mutex::new(HashMap::new()),
            history: std::sync::Arc::default(),
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
        let out = probe_once(&sa, OTHER, port(&sb), RTT_SIZE, wait).await;
        assert!(matches!(out, Outcome::Answered(..)), "{out:?}");
        // A full-size MTU probe too.
        let out = probe_once(&sa, OTHER, port(&sb), mtu::PROBE_MAX, wait).await;
        assert!(matches!(out, Outcome::Answered(..)), "{out:?}");

        // b does not know a's identity yet: it answers with the claimed one.
        let (sb2, _) = service(&b, None).await;
        let out = probe_once(&sa, OTHER, port(&sb2), RTT_SIZE, wait).await;
        assert!(matches!(out, Outcome::Answered(..)), "{out:?}");

        // Another box (c) at the address a expects b at: a wrong key.
        let (sc, _) = service(&c, Some(a.id())).await;
        let out = probe_once(&sa, OTHER, port(&sc), RTT_SIZE, wait).await;
        assert_eq!(out, Outcome::WrongKey);
        let (sc2, _) = service(&c, None).await;
        let out = probe_once(&sa, OTHER, port(&sc2), RTT_SIZE, wait).await;
        assert_eq!(out, Outcome::WrongKey);

        // Without keys: version 1 between the same services.
        let (sv1, _) = service(&a, None).await;
        let out = probe_once(&sv1, OTHER, port(&sb), RTT_SIZE, wait).await;
        assert!(matches!(out, Outcome::Answered(..)), "{out:?}");
    }

    #[test]
    fn a_confirmed_peer_stays_confirmed() {
        let table: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
        let ok = Outcome::Answered(Duration::from_millis(1), 0);
        assert_eq!(
            identity_change(&table, A, true, Outcome::WrongKey),
            Some(PeerIdentity::WrongKey)
        );
        assert_eq!(identity_change(&table, A, true, Outcome::Lost), None);
        assert_eq!(
            identity_change(&table, A, true, ok),
            Some(PeerIdentity::Confirmed)
        );
        assert_eq!(identity_change(&table, A, true, Outcome::WrongKey), None);
        assert_eq!(identity_change(&table, A, false, ok), None);
    }

    #[test]
    fn a_client_goes_direct_at_once_and_back_only_after_two_failed_tests() {
        let now = Instant::now();
        let mut t = UplinkTest::new(false);
        assert_eq!((t.flags(), t.direct()), (0, false));
        // Not reached: the next test in 5 minutes.
        assert_eq!(t.result(false, now), None);
        assert_eq!(t.next, now + TEST_NAT);
        assert_eq!(t.result(true, now), Some(true));
        assert_eq!(t.flags(), wire2::FLAG_REACHED);
        // Direct only once the server says so too.
        assert!(!t.direct());
        t.answer(wire2::FLAG_DIRECT, now);
        assert!(t.direct());
        // One failed test: still direct, tested again soon.
        assert_eq!(t.result(false, now), None);
        assert!(t.direct());
        assert_eq!(t.next, now + TEST_LATER);
        assert_eq!(t.result(false, now), Some(false));
        assert!(!t.direct());
        // The pause doubles after going back.
        assert_eq!(t.next, now + 2 * TEST_NAT);
        // A server that stops running direct: tested again after the gap.
        let mut t = UplinkTest::new(true);
        t.result(true, now);
        t.answer(0, now);
        assert!(!t.direct());
        assert_eq!(t.next, now + TEST_MIN_GAP + Duration::from_secs(1));
    }

    #[test]
    fn the_window_is_limited() {
        let table: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
        for _ in 0..RTT_WINDOW + 10 {
            update_reachability(&table, A, "a", Some(Duration::from_millis(5)));
        }
        let t = table.read().unwrap();
        assert_eq!(t[&A].recent.len(), RTT_WINDOW);
    }
}
