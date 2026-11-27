//! The controller: owns the configuration and the runtime state and keeps the
//! system in the desired state.
//!
//! All state changes happen here, sequentially: API requests, probe results,
//! loop guard reports, fetched configurations and timers arrive as [`Input`]s
//! or timer ticks in one select loop.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future as _;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};
use wanderlan_core::api::{
    self, ApiError, Applied, BreakoutFamily, BreakoutStatus, ChangeSource, ConfigInput,
    ConfigTexts, Confirmed, Done, Interface, Invitation, JoinParams, Joined, Level, MacEntry,
    PeerStatus, PendingChange, PortState, PortTraffic, Request, ResetReport, SiteState,
    SiteSummary, Status, UplinkStatus, Validation, VlanPortTraffic, VlanTraffic,
};
use wanderlan_core::config::{
    self, Config, ConfigFile, Diagnostic, Loaded, Network, render_local, render_network,
    render_network_wire, unified_diff,
};
use wanderlan_core::desired::{self, Runtime};
use wanderlan_core::identity::{Identity, Keyring, PeerIdentity, SiteId};
use wanderlan_core::join::{self, JoinReply, JoinRequest};
use wanderlan_core::loopguard::format_mac;
use wanderlan_core::mtu::wire::Announce;
use wanderlan_core::mtu::wire2;
use wanderlan_core::policy;
use wanderlan_core::reconcile::{self, Change};
use wanderlan_core::sync::{self, Version};
use wanderlan_core::sys::breakout::{BreakoutOps as _, Egress};
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::sys::nft::NftOps as _;
use wanderlan_core::sys::nft::PortDir;
use wanderlan_core::sys::nft_ip::LearnOps as _;
use wanderlan_core::sys::observe::ObserveOps as _;
use wanderlan_core::sys::{LinkKind, NetOps as _};
use wanderlan_core::topology::{self, ForwardingKey, TunnelKind};
use wanderlan_core::view;

use crate::Args;
use crate::api_server;
use crate::api_server::{Control, Registry, Route};
use crate::events::EventLog;
use crate::fdb_guard::{self, LoopReport, PortMap};
use crate::files::{Layout, Paths};
use crate::history::{self, History, SharedHistory};
use crate::host::Host;
use crate::invites::{Invite, Invites};
use crate::keepalive;
use crate::probe::{self, Me, PeerKeys, ProbeConfig, ProbeEvent, ProbeTable};
use crate::stats::{self, Rates, VlanRates};
use crate::sync_net::{self, Offer};

/// Delays of the extra flushes after a topology change. Sites apply a new
/// configuration a few seconds apart; until all have switched, a bridge can
/// learn addresses behind ports that are wrong in the new topology.
const FOLLOW_UP_FLUSHES: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];
/// How long a removed peer may still fetch the configuration from us.
const GRACE: Duration = Duration::from_secs(300);
/// Former control ports that answer on at most (after several changes).
const MAX_FORMER: usize = 4;
/// Wait time after a failed fetch before trying the same peer again.
const FETCH_BACKOFF: Duration = Duration::from_secs(30);
/// How long a rejected configuration is not fetched again.
const REJECT_TTL: Duration = Duration::from_secs(600);
/// How long a client waits after a push that failed (no answer).
const PUSH_RETRY: Duration = Duration::from_secs(5);
/// How long a client waits for its uplink to take over a change before it
/// asks whether the uplink refused it.
const RESYNC_AFTER: Duration = Duration::from_secs(15);
/// How old the FDB snapshot used for the status may be.
const FDB_CACHE_TTL: Duration = Duration::from_secs(5);
/// First line of a client's view when its last change was refused; the
/// reason follows. A TOML comment, so older daemons simply ignore it.
const REFUSED_MARKER: &str = "# wanderlan: not taken over: ";

/// What a fetched configuration is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    /// From a server or the uplink: it replaces the own network.
    Replace,
    /// From a client: its changes are merged into the own network.
    Merge,
    /// From the uplink after a change of this site was not taken over:
    /// replaces the own network only if the uplink refused the change.
    Resync,
}

/// The reason in the first line of a view whose change was refused.
fn refusal(text: &str) -> Option<&str> {
    text.lines().next()?.strip_prefix(REFUSED_MARKER)
}

/// Why the last change of a client was refused here, and when.
#[derive(Debug, Clone)]
struct Refusal {
    reason: String,
    /// On the clock of the probe table (compared with its announces).
    at: std::time::Instant,
}

/// Fingerprints of configurations that were rejected and are not fetched
/// again for [`REJECT_TTL`]; after that they get another chance.
#[derive(Debug, Default)]
struct Rejected(BTreeMap<String, Instant>);

impl Rejected {
    fn insert(&mut self, fingerprint: String, now: Instant) {
        self.0.retain(|_, until| *until > now);
        self.0.insert(fingerprint, now + REJECT_TTL);
    }

    /// The fingerprints still rejected at `now`.
    fn active(&self, now: Instant) -> BTreeSet<String> {
        self.0
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(fp, _)| fp.clone())
            .collect()
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}
/// Shortest accepted confirmation timeout.
const MIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Name of the netlink connection among the background tasks.
const NETLINK_TASK: &str = "netlink connection";

/// What became of a network taken over from a peer.
#[derive(Debug)]
enum TakeOver {
    /// Applied (or there was nothing to do).
    Applied,
    /// Not taken over; the client learns why.
    Refused(String),
    /// Could not be applied now; tried again later.
    Failed(String),
}

/// Inputs of the controller.
#[derive(Debug)]
pub enum Input {
    /// An API request with the channel for the answer.
    Api {
        /// The request.
        request: Request,
        /// Where to send the result.
        reply: oneshot::Sender<Result<serde_json::Value, ApiError>>,
    },
    /// A result of the probe service.
    Probe(ProbeEvent),
    /// A configuration fetched from a peer.
    Fetched {
        /// The peer.
        peer: SiteId,
        /// What to do with it.
        mode: FetchMode,
        /// The network text or an error.
        result: Result<String, String>,
    },
    /// The loop guard detected a loop.
    Loop(LoopReport),
    /// A site asks to join with an invitation of this site.
    JoinRequest {
        /// Source address of the connection: the new site's address.
        from: Ipv4Addr,
        /// The request.
        request: JoinRequest,
        /// Where to send the answer.
        reply: oneshot::Sender<JoinReply>,
    },
    /// A client pushed its network (it is never fetched from: it may be
    /// behind NAT).
    SyncPush {
        /// The client.
        peer: SiteId,
        /// Its network.
        text: String,
        /// Where the decision goes.
        reply: oneshot::Sender<sync_net::PushAnswer>,
    },
    /// The uplink answered a push of this site's network.
    Pushed {
        /// The uplink.
        peer: SiteId,
        /// The answer, or why there is none.
        result: Result<sync_net::PushAnswer, String>,
    },
    /// SIGHUP: the files were changed by hand.
    Reload,
    /// The inviting site answered our join request.
    JoinAnswer {
        /// What the user asked for.
        params: JoinParams,
        /// The answer, or why there is none.
        result: Result<JoinReply, String>,
        /// Where to send the API result.
        reply: oneshot::Sender<Result<serde_json::Value, ApiError>>,
    },
}

/// Exists while connection tracking is loaded (in this network namespace).
const CONNTRACK_MAX: &str = "/proc/sys/net/netfilter/nf_conntrack_max";
/// How often the learned client endpoints are read while one is missing.
const LEARN_WAITING: Duration = Duration::from_secs(2);
/// How often once all are known (a NAT that rebinds is noticed with the
/// next keepalive, then within this time).
const LEARN_KNOWN: Duration = Duration::from_secs(10);
/// A site that answered probes within this time (three probe rounds)
/// counts as running: a join does not take over its entry.
const ALIVE_WINDOW: Duration = Duration::from_secs(15);
/// How long a joining site asks again while the site it takes over still
/// answered a moment ago (it was just reset): longer than [`ALIVE_WINDOW`].
const JOIN_TAKEOVER_RETRY: Duration = Duration::from_secs(30);
/// How long a joining site retries while the inviting site is busy.
const JOIN_BUSY_RETRY: Duration = Duration::from_secs(60);
/// Wrong invitation codes allowed per address within [`JOIN_FAILURE_WINDOW`].
const JOIN_MAX_FAILURES: usize = 5;
/// Window for counting wrong invitation codes.
const JOIN_FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How long a joining box has to answer with its proof.
const JOIN_PROOF_TIMEOUT: Duration = Duration::from_secs(60);

/// A change waiting for confirmation.
#[derive(Debug)]
struct Pending {
    previous: Option<Config>,
    deadline: Instant,
    /// When it was applied (monotonic: compared with the probe replies).
    applied_at: std::time::Instant,
    source: ChangeSource,
    /// The peer it came from (a sync), or the site that joins.
    peer: Option<SiteId>,
    from_files: bool,
    fingerprint: String,
    /// For changes of a client merged here: the fingerprint the client
    /// announced, rejected too on a rollback (else fetched again at once).
    announced: Option<String>,
    /// The peers this change removed (in grace): their grace starts again
    /// when it is confirmed.
    graced: Vec<SiteId>,
}

/// Learned MAC addresses per bridge port index.
type MacCounts = Arc<BTreeMap<u32, usize>>;

/// Derived from the configuration and the runtime state; computed again
/// only when one of them changed (the status is asked for every second).
#[derive(Clone)]
struct Derived {
    config_id: u64,
    runtime: Runtime,
    desired: Option<Arc<desired::Desired>>,
    /// Fingerprint of the configuration in effect.
    fingerprint: String,
    /// Version of the confirmed configuration.
    confirmed: Option<Version>,
}

/// State of one network of the daemon.
struct Controller {
    sys: LinuxSystem,
    /// The box: applies what the networks want.
    host: Arc<Host>,
    /// The name of the network (see [`crate::files`]).
    network: String,
    /// Its tag in interface names ([`topology::DEFAULT_TAG`] for the
    /// default network).
    tag: char,
    /// The name of this box's site in the network (for the list of
    /// networks).
    site_name: Arc<RwLock<Option<String>>>,
    /// The bridges of this network's segments, as applied last.
    bridges: Vec<String>,
    /// What this network lost to another one on the box.
    conflicts: Vec<String>,
    /// The box did nothing at the last reconcile (another network had not
    /// said what it wants yet): reconciled again when the box changes.
    waiting: bool,
    /// This network has its ports on the box (else its sockets stay closed:
    /// another network has one of them).
    has_ports: bool,
    paths: Paths,
    events: Arc<EventLog>,
    /// The confirmed configuration (on disk, announced to peers).
    confirmed: Option<Config>,
    /// The configuration in effect (differs from `confirmed` while pending).
    config: Option<Config>,
    pending: Option<Pending>,
    runtime: Runtime,
    applied_key: Option<ForwardingKey>,
    last_error: Option<String>,
    flushes: BTreeSet<Instant>,
    probe_config: watch::Sender<ProbeConfig>,
    probes: ProbeTable,
    offer: watch::Sender<Offer>,
    ports: watch::Sender<PortMap>,
    rates: Rates,
    vlan_rates: VlanRates,
    /// The traffic of the last hour, for the graphs.
    history: SharedHistory,
    /// When the daemon started (ms since the Unix epoch).
    started_ms: u64,
    /// Base chains of other firewalls that drop forwarded packets (only
    /// read while this site has a breakout).
    forward_drops: Vec<String>,
    inputs: mpsc::Sender<Input>,
    /// Fingerprints not to fetch again, until when.
    rejected: Rejected,
    /// Clients whose last change was refused here, with the reason.
    refused: BTreeMap<SiteId, Refusal>,
    /// Since when the uplink has not taken over a change of this site.
    diverged_since: Option<Instant>,
    /// Why the last change of this site was not taken over by its uplink.
    sync_problem: Option<String>,
    /// The version each client of this site should have (its view).
    expected: BTreeMap<SiteId, Version>,
    /// The name this site had before it was renamed last. Its uplink may
    /// give it back (a newer network of the uplink that does not have the
    /// rename yet wins, like for every change of this site).
    renamed_from: Option<String>,
    /// The FDB as read last, for the status.
    /// The MAC addresses per port as counted last, for the status.
    fdb_cache: std::sync::Mutex<Option<(Instant, MacCounts)>>,
    /// Counts the changes of the configuration in effect (cache key).
    config_id: u64,
    /// What the status needs from the configuration, until it changes.
    derived: std::sync::Mutex<Option<Derived>>,
    fetching: bool,
    fetch_backoff: BTreeMap<SiteId, Instant>,
    grace: BTreeMap<SiteId, (Ipv4Addr, Instant)>,
    /// The keys of the peers in `grace`: with them they still fetch the new
    /// network (authenticated), e.g. a server that became a client of
    /// another site learns so from its old peers.
    grace_keys: BTreeMap<SiteId, PeerKeys>,
    /// The peers the last configuration change put in grace (taken by the
    /// pending change it makes).
    grace_added: Vec<SiteId>,
    /// Changes of clients of this site (by client, their fingerprint) that
    /// this client passed on and its uplink has not taken over yet: if the
    /// uplink refuses, so does this site, and the client learns why.
    relayed: BTreeMap<SiteId, String>,
    /// A change made on this site itself (not passed on from a client) is
    /// in what the uplink has not taken over yet: a refusal may be about it.
    own_change_up: bool,
    generation: u64,
    invites: Invites,
    join_failures: BTreeMap<Ipv4Addr, VecDeque<Instant>>,
    /// The nonces sent to boxes that join with an identity already in the
    /// network (they prove it holds the key), with when.
    join_nonces: BTreeMap<(Ipv4Addr, SiteId), (Vec<u8>, Instant)>,
    joining: bool,
    /// The last error of a step retried every second, logged once.
    last_repeated: Option<String>,
    /// The identity of this box, once it has one (created when first
    /// needed: configured, joining, or the first change applied).
    identity: Option<Arc<Identity>>,
    /// The keys derived from this box's identity and those of its peers.
    keyring: Option<Keyring>,
    /// The keys towards the direct peers whose identity is known.
    peer_keys: BTreeMap<SiteId, PeerKeys>,
    /// Where the control traffic of the clients comes from (confirmed by a
    /// challenge): the address their tunnel packets must come from too.
    control_endpoints: BTreeMap<SiteId, SocketAddrV4>,
    /// When to read the learned tunnel endpoints of the clients next.
    learn_next: Instant,
    /// A push to the uplink is running.
    pushing: bool,
    /// After a failed push: not before then again.
    push_retry_at: Instant,
    /// The fingerprint of the network last pushed to the uplink.
    pushed: Option<String>,
    /// The tunnels this site sends keepalives through.
    keepalive: watch::Sender<keepalive::Keepalives>,
    /// Former addresses of the probes and the sync (another control port
    /// before) and since when: they answer until every peer uses the new.
    former_binds: Vec<(SocketAddrV4, Instant)>,
    /// The address the probes and the sync were given last.
    last_bind: Option<SocketAddrV4>,
}

/// A network of the daemon: its controller and how to reach it.
struct NetworkHandle {
    name: String,
    inputs: mpsc::Sender<Input>,
    route: Route,
    task: JoinHandle<anyhow::Error>,
}

/// What every network shares: the box.
#[derive(Clone)]
struct Shared {
    sys: LinuxSystem,
    host: Arc<Host>,
    rates: Rates,
    vlan_rates: VlanRates,
    history: SharedHistory,
    reconcile_interval: u64,
}

/// Runs the daemon until SIGTERM or SIGINT.
pub async fn run(args: Args) -> Result<()> {
    info!("wanderland {} starting", wanderlan_core::version_line());
    let sys = LinuxSystem::connect().context("cannot open a netlink connection")?;
    let mut sigterm = signal(SignalKind::terminate()).context("cannot handle SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot handle SIGINT")?;
    let mut sighup = signal(SignalKind::hangup()).context("cannot handle SIGHUP")?;

    let rates: Rates = Arc::new(RwLock::new(BTreeMap::new()));
    let vlan_rates: VlanRates = Arc::new(RwLock::new(stats::VlanTraffic::default()));
    let history: SharedHistory = Arc::new(RwLock::new(History::default()));
    let mut tasks = vec![(
        "statistics",
        stats::spawn(
            sys.clone(),
            rates.clone(),
            vlan_rates.clone(),
            history.clone(),
        ),
    )];
    // Without the netlink connection nothing works any more: its end ends
    // the daemon, and systemd starts it again.
    if let Some(connection) = sys.net.take_connection() {
        tasks.push((NETLINK_TASK, connection));
    }
    let layout = Layout {
        config_dir: args.config_dir.clone(),
        state_dir: args.state_dir.clone(),
    };
    let (found, problems) = layout.scan_report();
    for problem in problems {
        warn!("{problem}");
    }
    let names: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
    let host = Host::new(sys.clone(), names);
    let shared = Shared {
        sys,
        host: host.clone(),
        rates,
        vlan_rates,
        history,
        reconcile_interval: args.reconcile_interval.max(1),
    };
    let mut networks: Vec<NetworkHandle> = Vec::new();
    for (name, tag) in found {
        let paths = layout.paths(&name);
        if name != api::DEFAULT_NETWORK {
            info!(
                "Network \"{name}\" (tag {tag}) in {}",
                paths.config_dir.display()
            );
        }
        networks.push(Controller::spawn(name, tag, paths, &shared));
    }
    let (control_tx, mut control) = mpsc::channel::<Control>(8);
    let registry = Registry::new(
        networks.iter().map(|n| n.route.clone()).collect(),
        control_tx,
    );
    tasks.push((
        "API",
        api_server::spawn(args.socket.clone(), registry.clone()).context("cannot start the API")?,
    ));

    let failure = loop {
        tokio::select! {
            _ = sighup.recv() => {
                info!("SIGHUP: reloading the configuration files");
                for network in &networks {
                    let _ = network.inputs.send(Input::Reload).await;
                }
            }
            (name, why) = first_finished(&mut tasks) => {
                break Some(format!("the {name} service stopped ({why})"));
            }
            (name, why) = first_stopped(&mut networks) => {
                break Some(format!("network \"{name}\" stopped ({why})"));
            }
            Some(request) = control.recv() => match request {
                Control::Add { name, reply } => {
                    let result = add_network(&layout, &shared, &registry, &mut networks, &name).await;
                    let _ = reply.send(result);
                }
                Control::Remove { name, reply } => {
                    let result = remove_network(&layout, &shared, &registry, &mut networks, &name).await;
                    let _ = reply.send(result);
                }
            },
            _ = sigterm.recv() => break None,
            _ = sigint.recv() => break None,
        }
    };

    // The netlink connection stays until everything is removed.
    for network in &networks {
        network.task.abort();
    }
    let mut connection = None;
    for (name, task) in tasks {
        if name == NETLINK_TASK {
            connection = Some(task);
        } else {
            task.abort();
        }
    }
    if let Some(why) = failure {
        // Clean up as far as possible (systemd's ExecStopPost does the
        // rest), then exit: systemd starts the daemon again, which builds
        // everything anew.
        if let Err(e) = host.teardown(STOP_TEARDOWN).await {
            warn!("{e:#}");
        }
        if let Some(task) = connection {
            task.abort();
        }
        anyhow::bail!("internal error: {why}; exiting so that systemd restarts wanderland");
    }
    info!("Stopping; removing everything wanderlan created");
    let removed = host.teardown(STOP_TEARDOWN).await?;
    if let Some(task) = connection {
        task.abort();
    }
    let _ = std::fs::remove_file(&args.socket);
    info!("Stopped ({} changes undone)", removed.len());
    Ok(())
}

impl Controller {
    /// Starts the controller of network `name` and its services.
    fn spawn(name: String, tag: char, paths: Paths, shared: &Shared) -> NetworkHandle {
        let events = EventLog::new();
        let site_name: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let (inputs_tx, inputs) = mpsc::channel::<Input>(256);
        let (probe_tx, probe_rx) = watch::channel(ProbeConfig::default());
        let (probe_event_tx, probe_events) = mpsc::channel(64);
        let probes: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
        let (offer_tx, offer_rx) = watch::channel(Offer::default());
        let (ports_tx, ports_rx) = watch::channel(PortMap::default());
        let (keepalive_tx, keepalive_rx) = watch::channel(keepalive::Keepalives::default());
        let services = vec![
            (
                "probes",
                probe::spawn(
                    probe_rx,
                    probes.clone(),
                    probe_event_tx,
                    shared.history.clone(),
                ),
            ),
            ("sync", sync_net::spawn(offer_rx, inputs_tx.clone())),
            ("keepalive", keepalive::spawn(keepalive_rx)),
            (
                "loop guard",
                fdb_guard::spawn(&shared.sys, ports_rx, inputs_tx.clone()),
            ),
        ];
        let invites = Invites::load(paths.invites());
        let ctl = Controller {
            sys: shared.sys.clone(),
            host: shared.host.clone(),
            network: name.clone(),
            tag,
            site_name: site_name.clone(),
            bridges: Vec::new(),
            conflicts: Vec::new(),
            waiting: false,
            has_ports: true,
            paths,
            events: events.clone(),
            confirmed: None,
            config: None,
            pending: None,
            runtime: Runtime::default(),
            applied_key: None,
            last_error: None,
            flushes: BTreeSet::new(),
            probe_config: probe_tx,
            probes,
            offer: offer_tx,
            ports: ports_tx,
            rates: shared.rates.clone(),
            vlan_rates: shared.vlan_rates.clone(),
            history: shared.history.clone(),
            started_ms: history::now_ms(),
            forward_drops: Vec::new(),
            inputs: inputs_tx.clone(),
            rejected: Rejected::default(),
            refused: BTreeMap::new(),
            diverged_since: None,
            sync_problem: None,
            expected: BTreeMap::new(),
            renamed_from: None,
            fdb_cache: std::sync::Mutex::new(None),
            config_id: 0,
            derived: std::sync::Mutex::new(None),
            fetching: false,
            fetch_backoff: BTreeMap::new(),
            grace: BTreeMap::new(),
            grace_keys: BTreeMap::new(),
            grace_added: Vec::new(),
            relayed: BTreeMap::new(),
            own_change_up: false,
            generation: 0,
            invites,
            join_failures: BTreeMap::new(),
            join_nonces: BTreeMap::new(),
            joining: false,
            last_repeated: None,
            identity: None,
            keyring: None,
            peer_keys: BTreeMap::new(),
            control_endpoints: BTreeMap::new(),
            learn_next: Instant::now(),
            pushing: false,
            push_retry_at: Instant::now(),
            pushed: None,
            keepalive: keepalive_tx,
            former_binds: Vec::new(),
            last_bind: None,
        };
        let interval = shared.reconcile_interval;
        let task = tokio::spawn(ctl.run(inputs, probe_events, services, interval));
        NetworkHandle {
            route: Route {
                name: name.clone(),
                tag,
                inputs: inputs_tx.clone(),
                events,
                site: site_name,
            },
            name,
            inputs: inputs_tx,
            task,
        }
    }

    /// The controller's loop; ends only when one of its services stopped
    /// (they never should), with why.
    async fn run(
        mut self,
        mut inputs: mpsc::Receiver<Input>,
        mut probe_events: mpsc::Receiver<ProbeEvent>,
        mut services: Vec<(&'static str, JoinHandle<()>)>,
        reconcile_interval: u64,
    ) -> anyhow::Error {
        self.startup();
        self.seed_measurements().await;
        self.seed_identities().await;
        if let Some(warning) = bridge_netfilter_warning() {
            warn!("{warning}");
            self.events.warn(warning);
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(reconcile_interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut second = tokio::time::interval(Duration::from_secs(1));
        second.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut applied = self.host.subscribe();
        loop {
            let next_flush = self.flushes.first().copied();
            let deadline = self.pending.as_ref().map(|p| p.deadline);
            tokio::select! {
                _ = ticker.tick() => self.reconcile().await,
                // Another network changed the box: one that only waited
                // does now what follows a reconcile (port map, keepalives).
                Ok(()) = applied.changed() => {
                    if self.waiting {
                        self.reconcile().await;
                    }
                }
                _ = second.tick() => self.every_second().await,
                Some(input) = inputs.recv() => self.handle(input).await,
                Some(event) = probe_events.recv() => self.handle(Input::Probe(event)).await,
                () = sleep_until_opt(deadline) => {
                    if let Err(e) = self.rollback("the change was not confirmed in time").await {
                        self.events.error(e.message);
                    }
                }
                () = sleep_until_opt(next_flush) => {
                    if let Some(at) = next_flush {
                        self.flushes.remove(&at);
                    }
                    self.flush_bridges().await;
                }
                (name, why) = first_finished(&mut services) => {
                    for (_, task) in &services {
                        task.abort();
                    }
                    return anyhow::anyhow!("its {name} service stopped ({why})");
                }
            }
        }
    }
}

/// Adds network `name` to the box and starts its controller.
async fn add_network(
    layout: &Layout,
    shared: &Shared,
    registry: &Registry,
    networks: &mut Vec<NetworkHandle>,
    name: &str,
) -> Result<api::NetworkInfo, ApiError> {
    let tag = layout
        .add(name)
        .map_err(|e| ApiError::new(format!("{e:#}")))?;
    shared.host.add_network(name).await;
    let handle = Controller::spawn(name.to_string(), tag, layout.paths(name), shared);
    registry.add(handle.route.clone());
    networks.push(handle);
    info!("Network \"{name}\" (tag {tag}) added");
    Ok(api::NetworkInfo {
        name: name.to_string(),
        tag,
        site: None,
    })
}

/// Resets network `name`, stops its controller and removes its files.
async fn remove_network(
    layout: &Layout,
    shared: &Shared,
    registry: &Registry,
    networks: &mut Vec<NetworkHandle>,
    name: &str,
) -> Result<ResetReport, ApiError> {
    if name == api::DEFAULT_NETWORK {
        return Err(ApiError::new(
            "the default network cannot be removed; `wanderlan reset` empties it",
        ));
    }
    let Some(at) = networks.iter().position(|n| n.name == name) else {
        return Err(ApiError::new(format!("this box has no network \"{name}\"")));
    };
    let (reply, answer) = oneshot::channel();
    let asked = networks[at].inputs.send(Input::Api {
        request: Request::Reset,
        reply,
    });
    if asked.await.is_err() {
        return Err(ApiError::new(format!("network \"{name}\" does not answer")));
    }
    let value = answer
        .await
        .map_err(|_| ApiError::new(format!("network \"{name}\" does not answer")))??;
    let mut report: ResetReport = serde_json::from_value(value).map_err(api_err)?;
    let handle = networks.remove(at);
    handle.task.abort();
    registry.remove(name);
    shared.host.remove_network(name).await;
    if let Err(e) = layout.remove(name) {
        report.problems.push(format!("{e:#}"));
    }
    info!("Network \"{name}\" removed");
    Ok(report)
}

/// Waits until the controller of a network ends (it never should), with
/// its name and why. Must not be polled again after it returned.
async fn first_stopped(networks: &mut [NetworkHandle]) -> (String, String) {
    std::future::poll_fn(|cx| {
        for network in networks.iter_mut() {
            if let std::task::Poll::Ready(result) = std::pin::Pin::new(&mut network.task).poll(cx) {
                let why = match result {
                    Ok(e) => format!("{e:#}"),
                    Err(e) => e.to_string(),
                };
                return std::task::Poll::Ready((network.name.clone(), why));
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// Why a tunnel runs as it does, for the status.
fn tunnel_reason(
    t: &topology::Tunnel<'_>,
    runtime: &desired::Runtime,
    test: Option<probe::DirectTest>,
    port: u16,
) -> String {
    let peer = &t.peer.name;
    let id = t.peer.id;
    let test = test.unwrap_or_default();
    let next = test.next.map(|at| {
        let secs = at
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        if secs >= 120 {
            format!("; next test in {} min", secs / 60)
        } else {
            format!("; next test in {secs} s")
        }
    });
    match t.kind {
        TunnelKind::Server => "direct: the servers reach each other".to_string(),
        TunnelKind::Client => match runtime.direct.get(&id) {
            Some(ip) => format!("direct: reached at {ip}:{port}"),
            None if test.tested => format!(
                "nat: not reachable from here at UDP {port} (behind NAT or a firewall); it opens the flows"
            ),
            None => "nat: not tested yet (a client tests once it is connected)".to_string(),
        },
        TunnelKind::Uplink => {
            if runtime.uplink_direct {
                format!("direct: \"{peer}\" reaches this site at UDP {port}")
            } else if test.reached {
                format!("nat: reached at UDP {port}, waiting for \"{peer}\" to run direct")
            } else if test.tested {
                format!(
                    "nat: \"{peer}\" cannot reach this site at UDP {port} (NAT or a firewall){}",
                    next.unwrap_or_default()
                )
            } else {
                "nat: not tested yet".to_string()
            }
        }
    }
}

/// The interfaces of the host for the API; `carries` maps an index to the
/// peers whose tunnel leaves through it.
fn interface_list(
    links: &[wanderlan_core::sys::LinkState],
    carries: &BTreeMap<u32, Vec<String>>,
) -> Vec<Interface> {
    let name_of = |i: u32| links.iter().find(|l| l.index == i).map(|l| l.name.clone());
    links
        .iter()
        .map(|l| Interface {
            name: l.name.clone(),
            kind: match &l.kind {
                LinkKind::Bridge(_) => "bridge".into(),
                LinkKind::Vxlan(_) => "vxlan".into(),
                LinkKind::Other(k) => k.clone(),
            },
            up: l.admin_up,
            master: l.master.and_then(name_of),
            mtu: l.mtu,
            managed: desired::is_owner_alias(l.alias.as_deref()),
            carries: carries.get(&l.index).cloned().unwrap_or_default(),
        })
        .collect()
}

/// How long the teardown at a stop may take (below systemd's 30 s).
const STOP_TEARDOWN: Duration = Duration::from_secs(25);
/// How long `wanderland --cleanup` may take.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Removes everything a daemon of this host created; also the kernel's GRE
/// fallback devices when nothing else needs their module. Run by systemd
/// after every end of the service (also a crash or a kill), before every
/// start (in case that could not run) and when the package is removed, so
/// nothing stays behind. Does nothing if nothing is there.
pub async fn cleanup(args: &Args) -> Result<()> {
    let sys = LinuxSystem::connect().context("Cleanup failed: cannot open a netlink connection")?;
    let result = match tokio::time::timeout(
        CLEANUP_TIMEOUT,
        reconcile::teardown(&sys, |c| info!("Cleanup: {c}")),
    )
    .await
    {
        Ok(Ok(removed)) => {
            if removed.is_empty() {
                debug!("Cleanup: nothing of wanderlan left");
            } else {
                info!("Cleanup: {} changes undone", removed.len());
            }
            Ok(())
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("Cleanup failed: {e}")),
        Err(_) => Err(anyhow::anyhow!(
            "Cleanup failed: did not finish within {} s",
            CLEANUP_TIMEOUT.as_secs()
        )),
    };
    // The socket also after a failure.
    let _ = std::fs::remove_file(&args.socket);
    result
}

/// wanderlan's bridges (index, name): br0 and those of further trunks.
/// A warning if br_netfilter sends bridged frames through iptables (Docker
/// loads it with `bridge-nf-call-iptables = 1`): the bridge option of
/// wanderlan can only add to that, not switch it off, so untagged frames of
/// the trunk then pass iptables and conntrack.
fn bridge_netfilter_warning() -> Option<String> {
    let on = |name: &str| {
        std::fs::read_to_string(format!("/proc/sys/net/bridge/{name}"))
            .is_ok_and(|v| v.trim() == "1")
    };
    let calls: Vec<&str> = ["bridge-nf-call-iptables", "bridge-nf-call-ip6tables"]
        .into_iter()
        .filter(|n| on(n))
        .collect();
    (!calls.is_empty()).then(|| {
        format!(
            "br_netfilter is loaded with {} = 1 (e.g. by Docker): untagged frames on the trunk pass iptables and conntrack, which costs throughput. Set net.bridge.{} = 0 if nothing on this host needs it.",
            calls.join(", "),
            calls[0]
        )
    })
}

fn owned_bridges(links: &[wanderlan_core::sys::LinkState]) -> Vec<(u32, String)> {
    links
        .iter()
        .filter(|l| {
            desired::is_owner_alias(l.alias.as_deref()) && matches!(l.kind, LinkKind::Bridge(_))
        })
        .map(|l| (l.index, l.name.clone()))
        .collect()
}

/// Waits until one of the background services ends (they never should).
/// Must not be polled again after it returned.
async fn first_finished(tasks: &mut [(&'static str, JoinHandle<()>)]) -> (&'static str, String) {
    std::future::poll_fn(|cx| {
        for (name, task) in tasks.iter_mut() {
            if let std::task::Poll::Ready(result) = std::pin::Pin::new(task).poll(cx) {
                let why = match result {
                    Ok(()) => "it ended".to_string(),
                    Err(e) if e.is_panic() => "it panicked, see above".to_string(),
                    Err(e) => e.to_string(),
                };
                return std::task::Poll::Ready((*name, why));
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// Sleeps until the instant, or forever if there is none.
async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A hex fingerprint as raw bytes.
fn fingerprint_bytes(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = hex
            .get(2 * i..2 * i + 2)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .unwrap_or(0);
    }
    out
}

/// Raw fingerprint bytes as hex.
fn fingerprint_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Gives the sites of `new` without an identity the one they have in `old`:
/// the same name, or (renamed) the same address under a name `new` does not
/// have any more. A file written without the `id` lines (an older format,
/// with names) changes no identity; what `local` says about such a site
/// (`self`, the trunk of its tunnel) follows.
fn keep_ids(old: &Network, new: &mut Network, local: &mut config::Local) {
    let names: BTreeSet<String> = new.sites.iter().map(|s| s.name.clone()).collect();
    let provisional: Vec<(SiteId, String, Ipv4Addr)> = new
        .sites
        .iter()
        .filter(|s| !s.has_identity())
        .map(|s| (s.id, s.name.clone(), s.address))
        .collect();
    for (id, name, address) in provisional {
        let found = old.by_name(&name).or_else(|| {
            old.sites
                .iter()
                .find(|o| o.address == address && !names.contains(&o.name))
        });
        if let Some(o) = found.filter(|o| o.has_identity() && new.site(o.id).is_none()) {
            let to = o.id;
            new.rekey(id, to);
            local.rekey(id, to);
        }
    }
}

fn api_err(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(e.to_string())
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(v).map_err(api_err)
}

/// Loads both texts; returns the configuration or the diagnostics.
fn load_texts(network: &str, local: &str) -> Result<(Loaded, Vec<Diagnostic>), Vec<Diagnostic>> {
    config::load(network, local).map(|o| (o.loaded, o.warnings))
}

fn invalid(diagnostics: Vec<Diagnostic>) -> ApiError {
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    ApiError {
        message: format!("the configuration is invalid ({errors} errors)"),
        diagnostics,
    }
}

impl Controller {
    /// Loads the configuration at startup. A file change that was pending
    /// when the daemon stopped is rolled back first.
    fn startup(&mut self) {
        match self.paths.migrate() {
            Ok(moved) => moved.into_iter().for_each(|m| self.events.info(m)),
            Err(e) => self.events.error(format!("{e:#}")),
        }
        if self.paths.marker_exists() {
            match self.paths.restore_confirmed() {
                Ok(true) => self.events.warn(
                    "An unconfirmed change of the configuration files was pending when wanderland stopped; the confirmed files were restored",
                ),
                Ok(false) => self.paths.clear_marker(),
                Err(e) => self.events.error(format!("{e:#}")),
            }
        }
        match self.paths.load_identity() {
            Ok(identity) => self.identity = identity.map(Arc::new),
            Err(e) => self.events.error(format!("{e:#}")),
        }
        // Files edited without the `id` lines keep the identities of what
        // was in effect last (its copy in the state directory).
        let saved = self.paths.confirmed_network();
        let mut cfg = self.load_files_with(saved.as_ref());
        if let Some(cfg) = &mut cfg {
            let before = cfg.clone();
            if let Some(saved) = &saved {
                keep_ids(saved, &mut cfg.network, &mut cfg.local);
            }
            // As written by the daemon: else the identities that were
            // found again (the file names sites by their names) are lost
            // with the next start.
            let written = if *cfg == before && self.files_hold(cfg) {
                self.paths.save_copy()
            } else {
                self.paths.write_confirmed(cfg)
            };
            if let Err(e) = written {
                warn!("{e:#}");
            }
        }
        self.confirmed = cfg.clone();
        self.set_config(cfg);
        self.claim_identity();
    }

    /// The identity of this box: the one in the state directory, created
    /// if there is none yet.
    fn ensure_identity(&mut self) -> Option<Arc<Identity>> {
        if self.identity.is_none() {
            match self.paths.load_identity() {
                Ok(identity) => self.identity = identity.map(Arc::new),
                Err(e) => {
                    self.events.error(format!("{e:#}"));
                    return None;
                }
            }
        }
        if self.identity.is_none() {
            match self.paths.create_identity() {
                Ok(identity) => {
                    self.events.info(format!(
                        "Created the identity of this box: {}",
                        identity.id().fingerprint()
                    ));
                    self.identity = Some(Arc::new(identity));
                }
                Err(e) => self.events.error(format!("{e:#}")),
            }
        }
        self.identity.clone()
    }

    /// The identity of this box, if it has one.
    fn own_id(&self) -> Option<SiteId> {
        self.identity.as_ref().map(|i| i.id())
    }

    /// Writes the identity of this box into its entry of the confirmed
    /// network if the entry has none (a network of an older version): like
    /// a revision taken over, written directly and spread by the sync. An
    /// entry with another identity belongs to another box.
    fn claim_identity(&mut self) {
        let Some(mut cfg) = self.confirmed.clone() else {
            return;
        };
        if self.pending.is_some() {
            return;
        }
        let Some(id) = self.ensure_identity().map(|i| i.id()) else {
            return;
        };
        let me = cfg.self_name().to_string();
        match cfg.self_site().and_then(config::Site::own_id) {
            Some(theirs) if theirs != id => {
                self.events.error(format!(
                    "network.toml lists another identity ({}) for this site (\"{me}\") than this box has ({}): another box joined as \"{me}\". The tunnels to sites with identities stay down; run `wanderlan reset` here and join again.",
                    theirs.fingerprint(),
                    id.fingerprint()
                ));
                return;
            }
            Some(_) => return,
            None => {}
        }
        if !sync::claim_own_id(&mut cfg.network, &mut cfg.local, id) {
            return;
        }
        if let Err(e) = self.paths.write_confirmed(&cfg) {
            self.events
                .error(format!("cannot write the configuration files: {e:#}"));
            return;
        }
        self.events.info(format!(
            "This site claims its identity {} in the network (revision {})",
            id.fingerprint(),
            cfg.network.revision
        ));
        self.confirmed = Some(cfg.clone());
        self.set_config(Some(cfg));
    }

    /// Takes the MSS rules left by a previous run as preliminary
    /// measurements, so that a restart does not remove and re-add them.
    async fn seed_measurements(&mut self) {
        let Some(cfg) = &self.config else { return };
        if cfg.mss().is_some() {
            return;
        }
        let Ok(Some(rules)) = self.sys.nft_state().await.map(|s| s.map(|s| s.mss)) else {
            return;
        };
        let peers = self.direct_peers();
        for rule in rules.iter().filter(|r| r.dir == PortDir::Out && !r.ipv6) {
            if let Some(peer) = topology::peer_of(&cfg.network, self.tag, &rule.port)
                && peers.contains_key(&peer.id)
            {
                self.runtime.pmtu.insert(
                    peer.id,
                    rule.mss.saturating_add(wanderlan_core::mtu::MSS_OVERHEAD),
                );
            }
        }
    }

    /// Takes the tunnels left by a previous run with the VNI they should
    /// have as confirmed, so that a restart does not take them down until
    /// the peers answer again (the kernel checks the VNI on every packet).
    async fn seed_identities(&mut self) {
        let Ok(actual) = reconcile::Actual::observe(&self.sys).await else {
            return;
        };
        let ifname = |id: SiteId| {
            self.config
                .as_ref()
                .and_then(|cfg| cfg.network.site(id))
                .map(|s| topology::tunnel_name(self.tag, s))
        };
        let mut adopted = Vec::new();
        for (peer, vni) in &self.runtime.vnis {
            let up = ifname(*peer)
                .and_then(|name| actual.links.get(&name))
                .is_some_and(|l| {
                    desired::is_owner_alias(l.alias.as_deref())
                        && l.admin_up
                        && matches!(l.kind, LinkKind::Vxlan(p) if p.vni == *vni)
                });
            if up {
                adopted.push(*peer);
            }
        }
        for peer in adopted {
            self.runtime.identity.insert(peer, PeerIdentity::Confirmed);
        }
        // A client tunnel that ran keeps its endpoint until the next
        // keepalive tells otherwise; one that ran direct stays so until a
        // test tells otherwise.
        let clients: Vec<(SiteId, String)> = self.config.as_ref().map_or_else(Vec::new, |cfg| {
            topology::tunnels_for(&cfg.network, cfg.self_id())
                .into_iter()
                .filter(|t| t.kind == TunnelKind::Client)
                .map(|t| (t.peer.id, t.ifname(self.tag)))
                .collect()
        });
        for (peer, name) in clients {
            if let Some(link) = actual.links.get(&name)
                && link.admin_up
                && let LinkKind::Vxlan(p) = link.kind
                && let Some(remote) = actual.remotes.get(&name)
                && self.runtime.identity.get(&peer) == Some(&PeerIdentity::Confirmed)
            {
                if p.source_port.is_some() {
                    self.runtime.endpoints.insert(peer, *remote);
                } else {
                    self.runtime.direct.insert(peer, *remote.ip());
                }
            }
        }
        let uplink = self
            .config
            .as_ref()
            .and_then(|cfg| cfg.network.site(cfg.self_site()?.uplink?))
            .map(|s| topology::tunnel_name(self.tag, s));
        if let Some(uplink) = uplink
            && let Some(link) = actual.links.get(&uplink)
            && link.admin_up
            && matches!(link.kind, LinkKind::Vxlan(p) if p.source_port.is_none())
        {
            self.runtime.uplink_direct = true;
        }
        self.publish();
    }

    /// Loads and validates the files (logs problems), with the identities
    /// of `known` for files
    /// written without them.
    fn load_files_with(&self, known: Option<&Network>) -> Option<Config> {
        let (network, local) = match self.paths.read() {
            Ok(t) => t,
            Err(e) => {
                self.events.error(format!("{e:#}"));
                return None;
            }
        };
        let label = |file: ConfigFile| match file {
            ConfigFile::Network => self.paths.network().display().to_string(),
            ConfigFile::Local => self.paths.local().display().to_string(),
        };
        match self.load_input(&network, &local, known) {
            Ok((loaded, warnings)) => {
                for w in &warnings {
                    warn!("{}", w.display_with(&label(w.file)));
                }
                match loaded {
                    Loaded::Unconfigured { .. } => {
                        self.events
                            .info("This site is not configured yet; run `wanderlan` to set it up");
                        None
                    }
                    Loaded::Configured(cfg) => {
                        self.events.info(format!(
                            "Loaded configuration: site \"{}\", {} sites, VLANs {}, trunk {}, network revision {}",
                            cfg.self_name(),
                            cfg.network.sites.len(),
                            cfg.network.vlans,
                            cfg.local.trunk.join(", "),
                            cfg.network.revision
                        ));
                        Some(cfg)
                    }
                }
            }
            Err(diags) => {
                for d in &diags {
                    self.events
                        .error(d.display_with(&label(d.file)).to_string());
                }
                self.events
                    .error("The configuration files are invalid; nothing was changed");
                None
            }
        }
    }

    /// True if the files hold `cfg` as the daemon writes it.
    fn files_hold(&self, cfg: &Config) -> bool {
        self.paths.read().is_ok_and(|(network, local)| {
            network == render_network(&cfg.network)
                && local == render_local(&cfg.local, Some(&cfg.network))
        })
    }

    /// SIGHUP: the files were changed by hand. Without a pending change the
    /// files are taken as confirmed (like a restart would).
    async fn reload_files(&mut self) {
        if self.pending.is_some() {
            self.events
                .warn("Ignoring SIGHUP while a change is waiting for confirmation");
            return;
        }
        // Written with names (by hand): with the identities at once.
        match self.paths.migrate() {
            Ok(moved) => moved.into_iter().for_each(|m| self.events.info(m)),
            Err(e) => self.events.error(format!("{e:#}")),
        }
        // The identities come from the configuration in effect, or (none
        // is: the files were invalid at the start) from the confirmed copy.
        let known: Option<Network> = self
            .confirmed
            .as_ref()
            .map(|c| c.network.clone())
            .or_else(|| self.paths.confirmed_network());
        if let Some(mut cfg) = self.load_files_with(known.as_ref()) {
            // Files written without the `id` lines keep the identities.
            if let Some(old) = &known {
                keep_ids(old, &mut cfg.network, &mut cfg.local);
            }
            // Written back as the daemon writes them (with the identities
            // found again), else kept as they are.
            let written = if self.files_hold(&cfg) {
                self.paths.save_copy()
            } else {
                self.paths.write_confirmed(&cfg)
            };
            if let Err(e) = written {
                warn!("{e:#}");
            }
            self.confirmed = Some(cfg.clone());
            self.set_config(Some(cfg));
            self.claim_identity();
            self.last_error = None;
            self.reconcile().await;
        }
    }

    /// Switches to a new configuration in effect and updates the background
    /// services. The announced version is always the confirmed one.
    fn set_config(&mut self, cfg: Option<Config>) {
        if let Ok(mut site) = self.site_name.write() {
            *site = cfg.as_ref().map(|c| c.self_name().to_string());
        }
        let old_peers = self.direct_peers();
        let old_tunnel_ports = self.tunnel_ports();
        self.grace_added.clear();
        let old_uplink = self
            .config
            .as_ref()
            .and_then(|c| c.self_site().and_then(|s| s.uplink));
        if let (Some(old), Some(new)) = (&self.config, &cfg)
            && let (Some(before), Some(now)) = (old.self_site(), new.self_site())
            && before.name != now.name
        {
            self.renamed_from = Some(before.name.clone());
        }
        self.config = cfg;
        self.config_id += 1;
        let new_peers = self.direct_peers();
        let now = Instant::now();
        for (id, addr) in old_peers {
            // A renamed peer keeps its identity. (Several clients behind one
            // NAT share an address: a removed one gets its grace all the
            // same.)
            if !new_peers.contains_key(&id) {
                // The keys are still those of the old peers here.
                if let Some(k) = self.peer_keys.get(&id) {
                    self.grace_keys.insert(id, k.clone());
                }
                self.grace_added.push(id);
                self.grace.insert(id, (addr, now + GRACE));
            }
        }
        for name in new_peers.keys() {
            self.grace.remove(name);
        }
        self.grace.retain(|_, (_, until)| *until > now);
        let grace = &self.grace;
        self.grace_keys.retain(|name, _| grace.contains_key(name));
        self.runtime
            .pmtu
            .retain(|peer, _| new_peers.contains_key(peer));
        self.runtime
            .direct
            .retain(|peer, _| new_peers.contains_key(peer));
        self.runtime
            .peer_ports
            .retain(|peer, _| new_peers.contains_key(peer));
        self.retest_moved(&old_tunnel_ports);
        // Another uplink is tested anew (a renamed one is the same).
        let uplink = self
            .config
            .as_ref()
            .and_then(|c| c.self_site().and_then(|s| s.uplink));
        if uplink != old_uplink {
            self.runtime.uplink_direct = false;
            // What was passed on to the old one is no longer in question.
            self.relayed.clear();
            self.own_change_up = false;
        }
        // A port shut by the loop guard stays shut while it exists: tunnels
        // of peers that are gone and trunk ports no longer used are freed.
        let trunk_ports: BTreeSet<&String> =
            self.config.as_ref().map_or_else(BTreeSet::new, |cfg| {
                std::iter::once(&cfg.local.trunk)
                    .chain(cfg.local.tunnel_trunks.values())
                    .flatten()
                    .collect()
            });
        // The graphs: the tunnels to the peers and the trunk ports.
        let ifnames = self.peer_ifnames();
        let wanted: BTreeSet<String> = ifnames
            .iter()
            .cloned()
            .chain(trunk_ports.iter().map(|p| (*p).clone()))
            .collect();
        let peers: BTreeSet<SiteId> = new_peers.keys().copied().collect();
        if let Ok(mut h) = self.history.write() {
            h.want(&self.network, wanted, peers);
        }
        self.runtime
            .blocked
            .retain(|port| ifnames.contains(port) || trunk_ports.contains(port));
        self.update_keys();
        self.publish();
    }

    /// The keys towards the direct peers whose identity is known, if the
    /// own entry carries this box's identity; the GRE keys of their tunnels.
    /// A peer with another identity than before starts unconfirmed.
    fn update_keys(&mut self) {
        let mut keys = BTreeMap::new();
        if let (Some(identity), Some(cfg)) = (self.identity.clone(), self.config.as_ref()) {
            let ring = match &mut self.keyring {
                Some(ring) if ring.me().id() == identity.id() => ring,
                slot => slot.insert(Keyring::new(identity.clone())),
            };
            let own = cfg.self_id() == identity.id();
            let peers: Vec<SiteId> = if own {
                topology::tunnels_for(&cfg.network, cfg.self_id())
                    .into_iter()
                    .filter_map(|t| t.peer.own_id())
                    .collect()
            } else {
                Vec::new()
            };
            ring.update(peers.iter());
            for id in peers {
                if let Some(k) = ring.get(&id) {
                    keys.insert(id, PeerKeys { id, keys: k });
                }
            }
        } else {
            self.keyring = None;
        }
        let before = std::mem::take(&mut self.peer_keys);
        self.runtime.identity.retain(|peer, _| {
            keys.get(peer).map(|k| k.id) == before.get(peer).map(|k| k.id)
                && keys.contains_key(peer)
        });
        self.runtime.vnis = keys.iter().map(|(id, k)| (*id, k.keys.vni())).collect();
        self.peer_keys = keys;
    }

    /// Direct peers of the configuration in effect.
    fn direct_peers(&self) -> BTreeMap<SiteId, Ipv4Addr> {
        self.config.as_ref().map_or_else(BTreeMap::new, |cfg| {
            topology::tunnels_for(&cfg.network, cfg.self_id())
                .into_iter()
                .map(|t| (t.peer.id, t.peer.address))
                .collect()
        })
    }

    /// The tunnel interfaces to the direct peers.
    fn peer_ifnames(&self) -> BTreeSet<String> {
        self.config.as_ref().map_or_else(BTreeSet::new, |cfg| {
            topology::tunnels_for(&cfg.network, cfg.self_id())
                .iter()
                .map(|t| t.ifname(self.tag))
                .collect()
        })
    }

    /// How a peer is called: in the network in effect, else (a former peer)
    /// in the confirmed one, else by its identity.
    fn peer_label(&self, id: SiteId) -> String {
        [self.config.as_ref(), self.confirmed.as_ref()]
            .into_iter()
            .flatten()
            .find(|c| c.network.site(id).is_some())
            .or(self.config.as_ref())
            .map_or_else(|| id.fingerprint(), |c| c.network.label(id))
    }

    /// Updates the probe service and the sync server.
    fn publish(&mut self) {
        let peers = self.direct_peers();
        let now = Instant::now();
        // Former peers still answer for a while; a current peer at the same
        // address (renamed) wins.
        let responders: BTreeMap<SiteId, Ipv4Addr> = self
            .grace
            .iter()
            .filter(|(n, (_, until))| *until > now && !peers.contains_key(*n))
            .map(|(n, (a, _))| (*n, *a))
            .collect();
        // A client listens on every address: behind NAT the address in the
        // network (its NAT's) is not its own. A server on its address. Not
        // while another network of the box has the ports of this one.
        let bind = self
            .config
            .as_ref()
            .filter(|_| self.has_ports)
            .and_then(|cfg| {
                cfg.self_site().map(|s| {
                    let ip = if s.uplink.is_some() {
                        Ipv4Addr::UNSPECIFIED
                    } else {
                        s.address
                    };
                    SocketAddrV4::new(ip, cfg.control_port())
                })
            });
        self.follow_bind(bind);
        let former: Vec<SocketAddrV4> = self.former_binds.iter().map(|(a, _)| *a).collect();
        // The clients with a key are never contacted first: they come.
        let passive: BTreeSet<SiteId> = self.config.as_ref().map_or_else(BTreeSet::new, |cfg| {
            topology::tunnels_for(&cfg.network, cfg.self_id())
                .into_iter()
                .filter(|t| t.kind == TunnelKind::Client && self.peer_keys.contains_key(&t.peer.id))
                .map(|t| t.peer.id)
                .collect()
        });
        let version = |m: &Network| Announce {
            revision: m.revision,
            fingerprint: fingerprint_bytes(&config::fingerprint(m)),
        };
        // Servers and the uplink get what this site knows; a client (of
        // this site, or one in grace that became a client elsewhere) and a
        // removed site only its own view of the network.
        let mut announce_to = BTreeMap::new();
        let mut views = BTreeMap::new();
        let mut texts: BTreeMap<SiteId, String> = BTreeMap::new();
        let mut expected = BTreeMap::new();
        let announce = match &self.confirmed {
            None => Announce::default(),
            Some(c) => {
                // Peers get the network in the form every version reads.
                let own = render_network_wire(&c.network);
                let uplink = c.self_site().and_then(|s| s.uplink);
                for (name, addr) in peers.iter().chain(responders.iter()) {
                    // By address only what no current peer has: a former
                    // peer behind the same NAT fetches with its key.
                    let by_address = peers.contains_key(name) || !peers.values().any(|a| a == addr);
                    let client = c
                        .network
                        .site(*name)
                        .is_none_or(|s| s.uplink.is_some() && Some(*name) != uplink);
                    if client {
                        let v = view::view_for(&c.network, *name);
                        announce_to.insert(*name, version(&v));
                        expected.insert(*name, Version::of(&v));
                        // A refused change: the client learns why.
                        let text = match self.refused.get(name) {
                            Some(r) => format!(
                                "{REFUSED_MARKER}{}\n{}",
                                r.reason.replace('\n', " "),
                                render_network_wire(&v)
                            ),
                            None => render_network_wire(&v),
                        };
                        texts.insert(*name, text.clone());
                        if by_address {
                            views.insert(*addr, text);
                        }
                    } else {
                        texts.insert(*name, own.clone());
                        if by_address {
                            views.insert(*addr, own.clone());
                        }
                    }
                }
                version(&c.network)
            }
        };
        self.expected = expected;
        let mtu_interval = self
            .config
            .as_ref()
            .and_then(|c| c.mss().is_none().then_some(c.mtu_probe_interval()));
        // The test whether a client tunnel can run direct: the VNIs of the
        // clients (for the test frames), the tunnel to the uplink.
        let tests = self.config.as_ref().map_or_else(BTreeMap::new, |cfg| {
            self.runtime
                .vnis
                .iter()
                .filter(|(peer, _)| passive.contains(*peer))
                .map(|(peer, vni)| {
                    (
                        *peer,
                        (*vni, self.runtime.tunnel_port_of(&cfg.network, *peer)),
                    )
                })
                .collect()
        });
        let uplink = self.config.as_ref().and_then(|cfg| {
            let peer = cfg.self_site()?.uplink?;
            if !self.peer_keys.contains_key(&peer) {
                return None;
            }
            Some(probe::UplinkTunnel {
                interface: topology::tunnel_name(self.tag, cfg.network.site(peer)?),
                peer,
                direct: self.runtime.uplink_direct,
                port: cfg.tunnel_port(),
            })
        });
        let probe_peers = self.config.as_ref().map_or_else(BTreeMap::new, |cfg| {
            peers
                .iter()
                .map(|(name, addr)| {
                    (
                        *name,
                        SocketAddrV4::new(*addr, self.runtime.control_port_of(&cfg.network, *name)),
                    )
                })
                .collect()
        });
        self.probe_config.send_replace(ProbeConfig {
            other_ports: self.host.ports_of_others(&self.network),
            bind,
            former: former.clone(),
            // In every request: the peers follow a change at once.
            ports: self.config.as_ref().map(|cfg| wire2::Ports {
                control: cfg.control_port(),
                tunnel: cfg.tunnel_port(),
            }),
            peers: probe_peers,
            responders: responders.clone(),
            generation: self.generation,
            mtu_interval,
            announce,
            announce_to,
            me: self.identity.clone().map(Me),
            // Former peers (in grace) are told apart by their key too.
            keys: self
                .peer_keys
                .iter()
                .chain(
                    self.grace_keys
                        .iter()
                        .filter(|(n, _)| responders.contains_key(*n)),
                )
                .map(|(n, k)| (*n, k.clone()))
                .collect(),
            names: peers
                .keys()
                .chain(responders.keys())
                .map(|id| (*id, self.peer_label(*id)))
                .collect(),
            passive,
            tests,
            direct: self.runtime.direct.clone(),
            uplink,
        });
        let addresses = self.direct_peers();
        let clients: BTreeSet<SiteId> = self.config.as_ref().map_or_else(BTreeSet::new, |cfg| {
            topology::tunnels_for(&cfg.network, cfg.self_id())
                .into_iter()
                .filter(|t| t.kind == TunnelKind::Client)
                .map(|t| t.peer.id)
                .collect()
        });
        let keyed = self
            .peer_keys
            .iter()
            .filter_map(|(name, k)| {
                let address = *addresses.get(name)?;
                Some((
                    k.id,
                    sync_net::Keyed {
                        name: self.peer_label(*name),
                        address,
                        client: clients.contains(name),
                        view: Arc::from(texts.get(name)?.as_str()),
                        keys: k.keys.clone(),
                    },
                ))
            })
            // Former peers (in grace) fetch what they get with their key,
            // from where they are now (a former client behind NAT).
            .chain(responders.iter().filter_map(|(name, address)| {
                let k = self.grace_keys.get(name)?;
                Some((
                    k.id,
                    sync_net::Keyed {
                        name: self.peer_label(*name),
                        address: *address,
                        client: true,
                        view: Arc::from(texts.get(name)?.as_str()),
                        keys: k.keys.clone(),
                    },
                ))
            }))
            .collect();
        self.offer.send_replace(Offer {
            bind,
            former,
            views: Arc::new(views),
            me: self.own_id(),
            keyed: Arc::new(keyed),
        });
        // Keepalives through the tunnel to the uplink through NAT.
        let interfaces: Vec<String> = self.config.as_ref().map_or_else(Vec::new, |cfg| {
            topology::tunnels_for(&cfg.network, cfg.self_id())
                .into_iter()
                .filter(|t| {
                    t.kind == TunnelKind::Uplink && self.runtime.mode(t) == config::TunnelMode::Nat
                })
                .map(|t| t.ifname(self.tag))
                .collect()
        });
        let fingerprint = self.own_id().map(|id| id.fingerprint()).unwrap_or_default();
        self.keepalive.send_if_modified(|k| {
            let changed = k.interfaces != interfaces || k.fingerprint != fingerprint;
            k.interfaces = interfaces;
            k.fingerprint = fingerprint;
            changed
        });
    }

    async fn handle(&mut self, input: Input) {
        match input {
            Input::Reload => self.reload_files().await,
            Input::Api {
                request: Request::Join(params),
                reply,
            } => self.start_join(params, reply),
            Input::Api { request, reply } => {
                let result = self.api(request).await;
                let _ = reply.send(result);
            }
            Input::JoinRequest {
                from,
                request,
                reply,
            } => {
                // The joining site has given up waiting (it was told so):
                // admitting it now would only be rolled back later.
                if reply.is_closed() {
                    return;
                }
                let answer = self.admit(from, request).await;
                let _ = reply.send(answer);
            }
            Input::JoinAnswer {
                params,
                result,
                reply,
            } => {
                let result = self.finish_join(params, result).await;
                let _ = reply.send(result);
            }
            Input::Probe(ProbeEvent::Pmtu { peer, pmtu }) => {
                let old = match pmtu {
                    Some(p) => self.runtime.pmtu.insert(peer, p),
                    None => self.runtime.pmtu.remove(&peer),
                };
                if old != pmtu {
                    self.reconcile().await;
                }
            }
            Input::Probe(ProbeEvent::Identity { peer, state }) => {
                self.peer_identity(peer, state).await;
            }
            Input::Probe(ProbeEvent::Direct { peer, at }) => self.tunnel_direct(peer, at).await,
            Input::Probe(ProbeEvent::Ports { peer, ports }) => self.peer_ports(peer, ports).await,
            Input::Probe(ProbeEvent::Restarted { peer }) => {
                // The uplink started again: it learns at once where this
                // site's tunnel packets come from (not with the next
                // keepalive).
                let uplink = self.config.as_ref().and_then(|c| c.self_site()?.uplink);
                if uplink == Some(peer) {
                    debug!("\"{}\" started again: keepalive now", self.peer_label(peer));
                    self.keepalive.send_modify(|k| k.kick += 1);
                }
            }
            Input::Probe(ProbeEvent::Endpoint { peer, addr }) => {
                self.control_endpoints.insert(peer, addr);
                // Its tunnel packets come from the same NAT: look at once.
                self.learn_next = Instant::now();
            }
            Input::Probe(ProbeEvent::Announce { peer, announce }) => {
                debug!(
                    "Peer \"{}\" announces network revision {}",
                    self.peer_label(peer),
                    announce.revision
                );
                self.maybe_sync();
            }
            Input::Fetched { peer, mode, result } => self.fetched(peer, mode, result).await,
            Input::SyncPush { peer, text, reply } => {
                let answer = self.pushed_to_us(peer, text).await;
                let _ = reply.send(answer);
            }
            Input::Pushed { peer, result } => self.push_answered(peer, result),
            Input::Loop(report) => {
                if self.runtime.blocked.insert(report.port.clone()) {
                    let peer = self.config.as_ref().and_then(|c| {
                        topology::peer_of(&c.network, self.tag, &report.port)
                            .map(|s| c.network.label(s.id))
                    });
                    self.events
                        .emit(Level::Warn, peer.as_deref(), report.message);
                    self.reconcile().await;
                }
            }
        }
    }

    /// What the probes found about the identity of a peer with a key. A
    /// confirmed peer stays confirmed until its identity changes (an answer
    /// with a wrong tag can come from anybody).
    async fn peer_identity(&mut self, id: SiteId, state: PeerIdentity) {
        if !self.peer_keys.contains_key(&id) {
            return;
        }
        let current = self.runtime.identity.get(&id).copied().unwrap_or_default();
        if current == PeerIdentity::Confirmed || current == state {
            return;
        }
        self.runtime.identity.insert(id, state);
        let address = self
            .direct_peers()
            .get(&id)
            .map_or_else(String::new, ToString::to_string);
        let peer = self.peer_label(id);
        let peer = peer.as_str();
        match state {
            PeerIdentity::Confirmed => {
                self.events.emit(
                    Level::Info,
                    Some(peer),
                    format!("Site \"{peer}\" proved its identity; its tunnel may come up"),
                );
            }
            PeerIdentity::WrongKey => self.events.emit(
                Level::Warn,
                Some(peer),
                format!(
                    "The box at {address} answers for site \"{peer}\" with another key: another box at that address, or one that was reset? Its tunnel stays down."
                ),
            ),
            PeerIdentity::Waiting => {}
        }
        self.reconcile().await;
        // The tunnel to the uplink just came up: its keepalive at once, so
        // that the uplink learns where this site is.
        if state == PeerIdentity::Confirmed {
            self.keepalive.send_modify(|k| k.kick += 1);
        }
    }

    /// An error of a step that is retried every second: logged when it is
    /// new, not every time.
    fn error_once(&mut self, message: String) {
        if self.last_repeated.as_deref() != Some(message.as_str()) {
            self.events.error(message.clone());
            self.last_repeated = Some(message);
        }
    }

    /// The test of a client tunnel says how it runs now: direct (the server
    /// reaches the client at `at`) or through NAT.
    async fn tunnel_direct(&mut self, id: SiteId, at: Option<Ipv4Addr>) {
        let Some(cfg) = &self.config else { return };
        let (port, own_port) = (
            self.runtime.tunnel_port_of(&cfg.network, id),
            cfg.tunnel_port(),
        );
        let kind = topology::tunnels_for(&cfg.network, cfg.self_id())
            .into_iter()
            .find(|t| t.peer.id == id)
            .map(|t| t.kind);
        let peer = cfg.network.label(id);
        let changed = match kind {
            Some(TunnelKind::Uplink) => {
                let was = self.runtime.uplink_direct;
                self.runtime.uplink_direct = at.is_some();
                was != at.is_some()
            }
            Some(TunnelKind::Client) => match at {
                Some(ip) => self.runtime.direct.insert(id, ip) != Some(ip),
                None => self.runtime.direct.remove(&id).is_some(),
            },
            _ => false,
        };
        if !changed {
            return;
        }
        let message = match (kind, at) {
            (Some(TunnelKind::Client), Some(ip)) => format!(
                "The tunnel to client \"{peer}\" runs direct now: it is reachable at {ip}:{port}"
            ),
            (_, Some(_)) => format!(
                "The tunnel to \"{peer}\" runs direct now: it reaches this site at UDP {own_port}"
            ),
            (_, None) => format!(
                "The tunnel to \"{peer}\" runs through NAT now (the client opens the flows)"
            ),
        };
        self.events.emit(Level::Info, Some(&peer), message);
        self.publish();
        self.reconcile().await;
    }

    /// The tunnel port of this site and those of its clients, as in effect.
    fn tunnel_ports(&self) -> (Option<u16>, BTreeMap<SiteId, u16>) {
        let Some(cfg) = &self.config else {
            return (None, BTreeMap::new());
        };
        let clients = topology::tunnels_for(&cfg.network, cfg.self_id())
            .into_iter()
            .filter(|t| t.kind == TunnelKind::Client)
            .map(|t| {
                let id = t.peer.id;
                (id, self.runtime.tunnel_port_of(&cfg.network, id))
            })
            .collect();
        (Some(cfg.tunnel_port()), clients)
    }

    /// After another tunnel port (of this site, or of a client) the test
    /// whether a client tunnel runs direct starts anew: through NAT until
    /// then, which always works (the old result was for the old port).
    fn retest_moved(&mut self, before: &(Option<u16>, BTreeMap<SiteId, u16>)) {
        let (own, clients) = self.tunnel_ports();
        if before.0.is_some() && own != before.0 && self.runtime.uplink_direct {
            self.runtime.uplink_direct = false;
            self.events.info(
                "The tunnel port of this site changed: the tunnel to its server runs through NAT until the test finds it reached directly again",
            );
        }
        for (id, port) in clients {
            if before.1.get(&id).is_some_and(|p| *p != port)
                && self.runtime.direct.remove(&id).is_some()
            {
                let label = self.peer_label(id);
                self.events.emit(
                    Level::Info,
                    Some(&label),
                    format!(
                        "Client \"{label}\" uses tunnel port {port} now: its tunnel runs through NAT until its test finds it reached directly again"
                    ),
                );
            }
        }
    }

    /// A direct peer says in its probes which ports it uses now: they count
    /// at once (its entry in the network follows with the sync, after the
    /// change is confirmed there).
    async fn peer_ports(&mut self, peer: SiteId, ports: wire2::Ports) {
        let Some(cfg) = &self.config else { return };
        if !self.direct_peers().contains_key(&peer) {
            return;
        }
        let effective = |r: &Runtime| {
            (
                r.control_port_of(&cfg.network, peer),
                r.tunnel_port_of(&cfg.network, peer),
            )
        };
        let listed = (
            cfg.network.control_port_of(peer),
            cfg.network.tunnel_port_of(peer),
        );
        let now = (ports.control, ports.tunnel);
        let before = effective(&self.runtime);
        let old_tunnel_ports = self.tunnel_ports();
        self.runtime.peer_ports.insert(peer, now);
        if before == now {
            return;
        }
        let label = self.peer_label(peer);
        if now != listed {
            self.events.emit(
                Level::Info,
                Some(&label),
                format!(
                    "Site \"{label}\" uses control port {} and tunnel port {} now; this site follows at once",
                    now.0, now.1
                ),
            );
        } else {
            debug!("\"{label}\" uses the ports of its entry again");
        }
        self.retest_moved(&old_tunnel_ports);
        self.publish();
        self.reconcile().await;
    }

    /// Notes the address given to the probes and the sync: after another
    /// control port the old one answers on (see [`Self::retire_former_binds`]).
    fn follow_bind(&mut self, bind: Option<SocketAddrV4>) {
        let Some(new) = bind else {
            self.former_binds.clear();
            self.last_bind = None;
            return;
        };
        if let Some(old) = self.last_bind
            && old.port() != new.port()
        {
            self.former_binds.retain(|(a, _)| *a != old);
            self.former_binds.push((old, Instant::now()));
            if self.former_binds.len() > MAX_FORMER {
                self.former_binds.remove(0);
            }
            self.events.info(format!(
                "The control port of this site is {} now; the old one ({}) answers until every peer uses the new one",
                new.port(),
                old.port()
            ));
        }
        // Never on a port this site takes now: its control port, its tunnel
        // port and the one above (an address change keeps the port).
        let tunnel = self.config.as_ref().map(Config::tunnel_port);
        let taken = |port: u16| {
            port == new.port() || tunnel.is_some_and(|t| port == t || port == t.saturating_add(1))
        };
        self.former_binds.retain(|(a, _)| !taken(a.port()));
        self.last_bind = Some(new);
    }

    /// A former control port is closed once every direct peer has sent a
    /// request to the new one since: no peer probes it any more. One that
    /// is off keeps it open (it finds this site there when it comes back).
    fn retire_former_binds(&mut self) {
        if self.former_binds.is_empty() {
            return;
        }
        let peers: Vec<SiteId> = self.direct_peers().into_keys().collect();
        let Ok(table) = self.probes.read() else {
            return;
        };
        let moved = |since: Instant| {
            peers.iter().all(|p| {
                table
                    .get(p)
                    .and_then(|e| e.requested_at)
                    .is_some_and(|at| at > since.into_std())
            })
        };
        let before = self.former_binds.len();
        self.former_binds.retain(|(_, since)| !moved(*since));
        drop(table);
        if self.former_binds.len() != before {
            debug!("Every peer uses the control port of this site now");
            self.publish();
        }
    }

    /// Reads where the tunnel packets of the clients come from (the sets of
    /// table `ip wanderlan`) and points their tunnels there. Often while a
    /// client tunnel waits for its endpoint, rarely once all are known.
    async fn learn_endpoints(&mut self) {
        let Some(desired) = self.derived().desired else {
            return;
        };
        if desired.learn.rules.is_empty() || Instant::now() < self.learn_next {
            if desired.learn.rules.is_empty() && !self.runtime.endpoints.is_empty() {
                self.runtime.endpoints.clear();
            }
            return;
        }
        let learned = match self.sys.learned().await {
            Ok(l) => l,
            Err(e) => {
                self.error_once(format!(
                    "Cannot read where the clients are (table ip wanderlan): {e}"
                ));
                self.learn_next = Instant::now() + LEARN_WAITING;
                return;
            }
        };
        let Some(network) = self.config.as_ref().map(|c| c.network.clone()) else {
            return;
        };
        // The sets are named by the tunnels of the clients.
        let clients: Vec<(SiteId, &str)> = desired
            .learn
            .rules
            .iter()
            .filter_map(|r| {
                Some((
                    topology::peer_of(&network, self.tag, &r.client)?.id,
                    r.client.as_str(),
                ))
            })
            .collect();
        let mut endpoints = BTreeMap::new();
        for (peer, set) in &clients {
            let control = self.control_endpoints.get(peer).map(|e| *e.ip());
            match wanderlan_core::nat::choose_endpoint(
                learned.get(*set).map_or(&[][..], Vec::as_slice),
                control,
            ) {
                Ok(addr) => {
                    endpoints.insert(*peer, addr);
                }
                Err(wanderlan_core::nat::NatProblem::OtherAddress(ip)) => {
                    self.error_once(format!(
                        "The tunnel packets of client \"{}\" come from {ip}, its control traffic from another address; not used",
                        network.label(*peer)
                    ));
                }
                // Nothing learned (yet, or expired): the last endpoint stays.
                Err(wanderlan_core::nat::NatProblem::Nothing) => {
                    if let Some(addr) = self.runtime.endpoints.get(peer) {
                        endpoints.insert(*peer, *addr);
                    }
                }
            }
        }
        // Often only while a client that answers waits for its endpoint (one
        // that is gone for days costs no `nft` every few seconds).
        let answers = |client: SiteId| {
            self.probes
                .read()
                .ok()
                .and_then(|t| t.get(&client).and_then(|p| p.last_reply_at))
                .is_some_and(|at| at.elapsed() < ALIVE_WINDOW)
        };
        let waiting = clients
            .iter()
            .any(|(peer, _)| !endpoints.contains_key(peer) && answers(*peer));
        self.learn_next = Instant::now() + if waiting { LEARN_WAITING } else { LEARN_KNOWN };
        if endpoints != self.runtime.endpoints {
            for (client, addr) in &endpoints {
                if self.runtime.endpoints.get(client) != Some(addr) {
                    let name = network.label(*client);
                    self.events.emit(
                        Level::Info,
                        Some(&name),
                        format!("The tunnel packets of client \"{name}\" come from {addr}"),
                    );
                }
            }
            self.runtime.endpoints = endpoints;
            self.reconcile().await;
        }
    }

    /// Housekeeping: automatic confirmation of synced changes, sync retries.
    async fn every_second(&mut self) {
        self.learn_endpoints().await;
        self.retire_former_binds();
        if let Some(p) = &self.pending
            && let ChangeSource::Sync { peer } = &p.source
        {
            let since = p.applied_at;
            let source = peer.clone();
            let peers = self.direct_peers();
            let reachable = self.probes.read().ok().and_then(|t| {
                t.iter()
                    .filter(|(name, _)| peers.contains_key(*name))
                    .find(|(_, pp)| pp.last_reply_at.is_some_and(|r| r > since))
                    .map(|(name, _)| *name)
            });
            let reachable = reachable.map(|id| self.peer_label(id));
            if let Some(via) = reachable {
                match self.confirm(false) {
                    Ok(c) => self.events.emit(
                        Level::Info,
                        Some(&source),
                        format!(
                            "Configuration from \"{source}\" confirmed automatically (peer \"{via}\" is reachable); network revision {}",
                            c.revision
                        ),
                    ),
                    Err(e) => self.error_once(e.message),
                }
            }
        }
        if let Some(p) = &self.pending
            && let ChangeSource::Join { site } = &p.source
            && let Some(joined) = p.peer
        {
            let since = p.applied_at;
            let site = site.clone();
            let up = self.probes.read().ok().is_some_and(|t| {
                t.get(&joined)
                    .and_then(|pp| pp.last_reply_at)
                    .is_some_and(|r| r > since)
            });
            if up {
                match self.confirm(false) {
                    Ok(c) => self.events.emit(
                        Level::Info,
                        Some(&site),
                        format!(
                            "Site \"{site}\" joined the network; network revision {}",
                            c.revision
                        ),
                    ),
                    Err(e) => self.error_once(e.message),
                }
            }
        }
        self.maybe_sync();
    }

    /// Fetches a newer network from a peer, if one announces it: from a server
    /// or the uplink (replacing the own network), otherwise the changes of a
    /// client (merged into the own network).
    fn maybe_sync(&mut self) {
        if self.pending.is_some() || self.fetching {
            return;
        }
        let Some(confirmed) = &self.confirmed else {
            return;
        };
        let me = confirmed.self_id();
        let ours = match self.derived().confirmed {
            Some(v) => v,
            None => Version::of(&confirmed.network),
        };
        let tunnels: Vec<(SiteId, TunnelKind)> = topology::tunnels_for(&confirmed.network, me)
            .into_iter()
            .map(|t| (t.peer.id, t.kind))
            .collect();
        let now = Instant::now();
        let table = self.probes.read().map(|t| t.clone()).unwrap_or_default();
        let rejected = self.rejected.active(now);
        let mut upstream: Vec<(SiteId, Version)> = Vec::new();
        let mut client_changes: Option<(SiteId, Version)> = None;
        let mut adopt: Option<(SiteId, u64)> = None;
        let mut resync: Option<SiteId> = None;
        let mut push: Option<SiteId> = None;
        let mut in_sync_again: Vec<SiteId> = Vec::new();
        for (name, kind) in &tunnels {
            if self.fetch_backoff.get(name).is_some_and(|t| *t > now) {
                continue;
            }
            let Some(probe) = table.get(name) else {
                continue;
            };
            let Some(announce) = probe.announce else {
                continue;
            };
            // A revision that no network.toml can hold, or a jump far
            // ahead (written by hand somewhere), is not adopted.
            if announce.revision > config::MAX_REVISION
                || announce.revision > ours.revision.saturating_add(config::MAX_REVISION_STEP)
            {
                continue;
            }
            let theirs = Version {
                revision: announce.revision,
                fingerprint: fingerprint_hex(&announce.fingerprint),
            };
            match kind {
                TunnelKind::Server | TunnelKind::Uplink
                    if theirs.fingerprint == ours.fingerprint =>
                {
                    // Same content, only a newer revision number: no fetch.
                    if theirs.revision > ours.revision && adopt.is_none() {
                        adopt = Some((*name, theirs.revision));
                    }
                    if *kind == TunnelKind::Uplink {
                        self.diverged_since = None;
                        // The uplink has what this site passed on.
                        self.relayed.clear();
                        self.own_change_up = false;
                    }
                }
                _ if rejected.contains(&theirs.fingerprint) => {}
                TunnelKind::Server => upstream.push((*name, theirs)),
                // The uplink has the last word on what this site knows.
                TunnelKind::Uplink if theirs.revision > ours.revision => {
                    self.diverged_since = None;
                    upstream.push((*name, theirs));
                }
                TunnelKind::Uplink => {
                    // A change of this site that the uplink has not taken
                    // over (yet): sent to it at once (its uplink never
                    // fetches from a client with a key), and after a while
                    // ask whether it refused it.
                    let since = *self.diverged_since.get_or_insert(now);
                    if self.peer_keys.contains_key(name)
                        && probe.reachable
                        && self.pushed.as_deref() != Some(ours.fingerprint.as_str())
                    {
                        push = Some(*name);
                    }
                    if probe.reachable && now.duration_since(since) >= RESYNC_AFTER {
                        resync = Some(*name);
                    }
                }
                TunnelKind::Client => {
                    let Some(expected) = self.expected.get(name) else {
                        continue;
                    };
                    if theirs.fingerprint == expected.fingerprint {
                        // Back in sync only by an announce made after a
                        // refusal: a pushed change arrives before the
                        // client's next probe announces it.
                        if self
                            .refused
                            .get(name)
                            .is_none_or(|r| probe.announced_at.is_some_and(|t| t > r.at))
                        {
                            in_sync_again.push(*name);
                        }
                    } else if client_changes.is_none()
                        && theirs.revision >= expected.revision
                        // A client with a key sends its changes itself.
                        && !self.peer_keys.contains_key(name)
                    {
                        client_changes = Some((*name, theirs));
                    }
                }
            }
        }
        // A client that is in sync again has nothing refused any more.
        let mut republish = false;
        for name in in_sync_again {
            republish |= self.refused.remove(&name).is_some();
        }
        if republish {
            self.publish();
        }
        if let Some((peer, revision)) = adopt {
            self.adopt_revision(peer, revision);
            return;
        }
        if let Some(peer) = push.filter(|_| {
            !self.pushing && upstream.is_empty() && Instant::now() >= self.push_retry_at
        }) {
            self.push_to(peer, &ours.fingerprint);
        }
        // Not asked while the uplink decides on a push: its answer could be
        // older than its decision. A refusal is fetched right after it.
        if self.pushing {
            resync = None;
        }
        // Newer versions from servers or the uplink first, then changes of
        // a client, then the question whether the uplink refused ours.
        let chosen = sync::choose(
            &ours,
            upstream.iter().map(|(n, v)| (*n, v.clone())),
            &rejected,
        )
        .map(|(n, v)| (n, Some(v), FetchMode::Replace))
        .or_else(|| client_changes.map(|(n, v)| (n, Some(v), FetchMode::Merge)))
        .or_else(|| resync.map(|n| (n, None, FetchMode::Resync)));
        let Some((peer, version, mode)) = chosen else {
            return;
        };
        let Some(target) = self.control_target(peer) else {
            return;
        };
        let label = self.peer_label(peer);
        match (&mode, version) {
            (FetchMode::Merge, Some(v)) => self.events.emit(
                Level::Info,
                Some(&label),
                format!(
                    "Client \"{label}\" has changes (revision {}, here: {}); fetching them",
                    v.revision, ours.revision
                ),
            ),
            (FetchMode::Resync, _) => {
                // Asked again only after another wait.
                self.diverged_since = Some(now);
                debug!("Asking \"{label}\" whether it took over the change of this site");
            }
            (_, Some(v)) => self.events.emit(
                Level::Info,
                Some(&label),
                format!(
                    "Peer \"{label}\" has network revision {} (here: {}); fetching it",
                    v.revision, ours.revision
                ),
            ),
            (_, None) => {}
        }
        self.fetching = true;
        let inputs = self.inputs.clone();
        // Authenticated where both know each other's identity.
        let keyed = self.own_id().zip(self.peer_keys.get(&peer).cloned());
        tokio::spawn(async move {
            let result = match keyed {
                Some((me, k)) => sync_net::fetch_sync(target, me, k.id, &k.keys).await,
                None => sync_net::fetch(target).await,
            }
            .map_err(|e| format!("{e:#}"));
            let _ = inputs.send(Input::Fetched { peer, mode, result }).await;
        });
    }

    /// Where the daemon of `peer` listens: the control port its probes say
    /// it uses now, else where its authenticated probes come from (it may
    /// just have moved to another port, which this site learns only with
    /// its network), else its control port in the network.
    fn control_target(&self, peer: SiteId) -> Option<SocketAddrV4> {
        let cfg = self.confirmed.as_ref()?;
        let addr = *self.direct_peers().get(&peer)?;
        if let Some((control, _)) = self.runtime.peer_ports.get(&peer) {
            return Some(SocketAddrV4::new(addr, *control));
        }
        let seen = self
            .probes
            .read()
            .ok()
            .and_then(|t| t.get(&peer).and_then(|p| p.control_at))
            .filter(|at| *at.ip() == addr);
        Some(seen.unwrap_or_else(|| SocketAddrV4::new(addr, cfg.network.control_port_of(peer))))
    }

    /// Sends this site's network to its uplink (`SYNC` with a body).
    fn push_to(&mut self, peer: SiteId, fingerprint: &str) {
        let Some(target) = self.control_target(peer) else {
            return;
        };
        let Some(cfg) = &self.confirmed else { return };
        let (Some(me), Some(k)) = (self.own_id(), self.peer_keys.get(&peer).cloned()) else {
            return;
        };
        let text = render_network_wire(&cfg.network);
        self.pushing = true;
        self.pushed = Some(fingerprint.to_string());
        debug!(
            "Sending the changes of this site to \"{}\"",
            self.peer_label(peer)
        );
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let result = sync_net::push_sync(target, me, k.id, &k.keys, &text)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = inputs.send(Input::Pushed { peer, result }).await;
        });
    }

    /// Remembers a fingerprint that is not fetched again for a while.
    fn reject(&mut self, fingerprint: String) {
        self.rejected.insert(fingerprint, Instant::now());
    }

    /// Takes over a newer revision number with the same content, without a
    /// fetch, an apply or a countdown.
    fn adopt_revision(&mut self, peer: SiteId, revision: u64) {
        let Some(mut cfg) = self.confirmed.clone() else {
            return;
        };
        cfg.network.revision = revision;
        if let Err(e) = self.paths.write_confirmed(&cfg) {
            let message = format!("cannot write the configuration files: {e:#}");
            if self.last_repeated.as_deref() != Some(message.as_str()) {
                warn!("{message}");
                self.last_repeated = Some(message);
            }
            return;
        }
        debug!(
            "Peer \"{}\" has revision {revision} with the same content; taken over",
            self.peer_label(peer)
        );
        self.confirmed = Some(cfg.clone());
        self.set_config(Some(cfg));
    }

    async fn fetched(&mut self, peer: SiteId, mode: FetchMode, result: Result<String, String>) {
        self.fetching = false;
        let text = match result {
            Ok(t) => t,
            Err(e) => {
                self.fetch_backoff
                    .insert(peer, Instant::now() + FETCH_BACKOFF);
                let label = self.peer_label(peer);
                self.events.emit(
                    Level::Warn,
                    Some(&label),
                    format!("Cannot fetch the configuration from \"{label}\": {e}"),
                );
                return;
            }
        };
        self.take_over(peer, mode, text).await;
    }

    /// A client pushed its network: taken over like a fetched one (what the
    /// client may change), if nothing else is pending.
    async fn pushed_to_us(&mut self, peer: SiteId, text: String) -> sync_net::PushAnswer {
        let now = Instant::now();
        // Busy, or the last attempt failed a moment ago (tried again later).
        if self.pending.is_some()
            || self.fetching
            || self.fetch_backoff.get(&peer).is_some_and(|t| *t > now)
        {
            return sync_net::PushAnswer::Busy;
        }
        if !self.peer_keys.contains_key(&peer) {
            return sync_net::PushAnswer::Refused("not a client with a key".into());
        }
        // A change rolled back or refused here is not taken over again.
        if let Some(network) = config::parse_network(&text).ok().and_then(|p| p.network)
            && self
                .rejected
                .active(now)
                .contains(&config::fingerprint(&network))
        {
            let reason = self.refused.get(&peer).map_or_else(
                || "it was rolled back or refused here".to_string(),
                |r| r.reason.clone(),
            );
            return sync_net::PushAnswer::Refused(reason);
        }
        let label = self.peer_label(peer);
        self.events.emit(
            Level::Info,
            Some(&label),
            format!("Client \"{label}\" sent changes"),
        );
        match self.take_over(peer, FetchMode::Merge, text).await {
            TakeOver::Applied => sync_net::PushAnswer::Ok,
            TakeOver::Refused(reason) => sync_net::PushAnswer::Refused(reason),
            // Sent again soon.
            TakeOver::Failed(why) => {
                debug!("The change of client \"{label}\" is not applied now: {why}");
                sync_net::PushAnswer::Busy
            }
        }
    }

    /// A change of client `peer` is not taken over: the client learns why
    /// (in the answer to its push, with its next fetch).
    fn refuse_client(
        &mut self,
        peer: SiteId,
        fingerprint: Option<String>,
        reason: String,
    ) -> TakeOver {
        if let Some(fingerprint) = fingerprint {
            self.reject(fingerprint);
        }
        let label = self.peer_label(peer);
        self.events.emit(
            Level::Warn,
            Some(&label),
            format!("Not taking over the changes of client \"{label}\": {reason}"),
        );
        self.refused.insert(
            peer,
            Refusal {
                reason: reason.clone(),
                at: std::time::Instant::now(),
            },
        );
        self.publish();
        TakeOver::Refused(reason)
    }

    /// The uplink answered the push of this site's network.
    fn push_answered(&mut self, peer: SiteId, result: Result<sync_net::PushAnswer, String>) {
        self.pushing = false;
        let peer = self.peer_label(peer);
        match result {
            Ok(sync_net::PushAnswer::Ok) => {
                debug!("\"{peer}\" took over the change of this site");
            }
            // Pushed again soon.
            // Pushed again in a moment (not every second while it is busy).
            Ok(sync_net::PushAnswer::Busy) => {
                self.pushed = None;
                self.push_retry_at = Instant::now() + PUSH_RETRY;
            }
            // The uplink's view says why (`refused`): fetched at once, its
            // configuration applies again.
            Ok(sync_net::PushAnswer::Refused(reason)) => {
                debug!("\"{peer}\" refused the change of this site: {reason}");
                self.diverged_since = Instant::now().checked_sub(RESYNC_AFTER);
            }
            Err(e) => {
                self.pushed = None;
                self.push_retry_at = Instant::now() + PUSH_RETRY;
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!("Cannot send the changes of this site to \"{peer}\": {e}"),
                );
            }
        }
    }

    /// Takes over a network from `peer` (fetched, or pushed by a client).
    async fn take_over(&mut self, id: SiteId, mode: FetchMode, text: String) -> TakeOver {
        let Some(local) = self.confirmed.as_ref().map(|c| c.local.clone()) else {
            return TakeOver::Failed("this site is not configured".into());
        };
        let peer = self.peer_label(id);
        // A name the text does not have (the view of a client lacks the
        // sites it does not see) is one this site knows.
        let context = self.confirmed.as_ref().map(|c| c.network.clone());
        let parsed = match config::parse_network_in(&text, context.as_ref()) {
            Ok(p) => p.network,
            Err(diags) => {
                let msg = diags.first().map_or_else(String::new, ToString::to_string);
                if mode == FetchMode::Merge {
                    return self.refuse_client(id, None, format!("it cannot be read: {msg}"));
                }
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!("The configuration from \"{peer}\" cannot be read: {msg}"),
                );
                self.fetch_backoff
                    .insert(id, Instant::now() + FETCH_BACKOFF);
                return TakeOver::Failed(msg);
            }
        };
        let Some(network) = parsed else {
            if mode == FetchMode::Merge {
                return self.refuse_client(id, None, "it is empty".into());
            }
            // Nothing in it (e.g. only comments): not again every second.
            self.fetch_backoff
                .insert(id, Instant::now() + FETCH_BACKOFF);
            return TakeOver::Failed("empty".into());
        };
        let fingerprint = config::fingerprint(&network);
        let network = match mode {
            FetchMode::Merge => {
                let Some(own) = self.confirmed.as_ref().map(|c| c.network.clone()) else {
                    return TakeOver::Failed("this site is not configured".into());
                };
                match view::merge(&own, &network, id) {
                    Ok(merged) => {
                        for (_, old, new) in wanderlan_core::rename::renames(&own, &merged.network)
                        {
                            self.events.emit(
                                Level::Info,
                                Some(&new),
                                format!("Site \"{old}\" is now called \"{new}\""),
                            );
                        }
                        merged.network
                    }
                    Err(e) => return self.refuse_client(id, Some(fingerprint), e),
                }
            }
            FetchMode::Resync => {
                let Some(reason) = refusal(&text) else {
                    // Not refused: the uplink may still take it over. A
                    // change for an uplink with a key is sent again (it may
                    // have forgotten a refusal, e.g. by a restart).
                    self.pushed = None;
                    return TakeOver::Applied;
                };
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!(
                        "The change of this site was not taken over by \"{peer}\": {reason}; its configuration applies again"
                    ),
                );
                self.sync_problem = Some(format!(
                    "Your change was not taken over by \"{peer}\": {reason}"
                ));
                self.diverged_since = None;
                // Only changes of its clients in it: refused here too, so they
                // are not merged and passed on again (they learn why). With a
                // change of its own in it the refusal may be about that one:
                // the clients' changes are passed on again by themselves.
                let relayed = std::mem::take(&mut self.relayed);
                if !std::mem::take(&mut self.own_change_up) {
                    for (client, fingerprint) in relayed {
                        self.refuse_client(
                            client,
                            Some(fingerprint),
                            format!("\"{peer}\" refused it: {reason}"),
                        );
                    }
                }
                network
            }
            FetchMode::Replace => {
                // Fetched because it was announced newer: what came must be.
                let ours = self.confirmed.as_ref().map(|c| Version::of(&c.network));
                if ours.is_some_and(|o| !Version::of(&network).is_newer_than(&o)) {
                    debug!("The configuration fetched from \"{peer}\" is not newer; not used");
                    self.fetch_backoff
                        .insert(id, Instant::now() + FETCH_BACKOFF);
                    return TakeOver::Applied;
                }
                // What the uplink has now replaces what this site passed on.
                self.relayed.clear();
                self.own_change_up = false;
                network
            }
        };
        let previous = self.confirmed.as_ref().map(|c| c.network.clone());
        // This site's own name back: its change was refused (Resync), or the
        // uplink's newer network does not have the rename yet.
        let me = self.confirmed.as_ref().map(Config::self_id);
        let taken_back = mode == FetchMode::Resync
            || self.renamed_from.as_deref().is_some_and(|old| {
                me.and_then(|me| network.site(me))
                    .is_some_and(|s| s.name == old)
            });
        match sync::accept(
            network,
            &local,
            previous.as_ref(),
            taken_back,
            self.own_id(),
        ) {
            Err(e) if mode == FetchMode::Merge => {
                self.refuse_client(id, Some(fingerprint), e.to_string())
            }
            Err(e) => {
                self.reject(fingerprint);
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!("Not using the configuration from \"{peer}\": {e}"),
                );
                TakeOver::Failed(e.to_string())
            }
            Ok(cfg) => {
                if mode == FetchMode::Merge {
                    self.refused.remove(&id);
                }
                let source = ChangeSource::Sync { peer: peer.clone() };
                let result = self.apply(cfg, source, Some(id), None, None, false).await;
                // What was announced is rejected too on a rollback (an
                // identity claimed here makes the result differ from it).
                if result.is_ok()
                    && let Some(p) = &mut self.pending
                {
                    p.announced = Some(fingerprint.clone());
                }
                // A client's change this site passes on to its own uplink.
                let has_uplink = self
                    .confirmed
                    .as_ref()
                    .and_then(|c| c.self_site())
                    .is_some_and(|s| s.uplink.is_some());
                if result.is_ok() && mode == FetchMode::Merge && has_uplink {
                    self.relayed.insert(id, fingerprint.clone());
                }
                if let Err(e) = result {
                    // Possibly transient (e.g. a busy kernel); try again later.
                    self.fetch_backoff
                        .insert(id, Instant::now() + FETCH_BACKOFF);
                    self.events.emit(
                        Level::Warn,
                        Some(&peer),
                        format!(
                            "Cannot apply the configuration from \"{peer}\": {}",
                            e.message
                        ),
                    );
                    return TakeOver::Failed(e.message);
                }
                TakeOver::Applied
            }
        }
    }

    // ---- API ---------------------------------------------------------------

    async fn api(&mut self, request: Request) -> Result<serde_json::Value, ApiError> {
        match request {
            Request::Status => to_value(&self.status().await),
            Request::Subscribe => to_value(&self.events.subscribe().0),
            Request::GetConfig => to_value(&self.config_texts()),
            Request::Validate(input) => to_value(&self.validate(&input)),
            Request::Apply(params) => {
                let cfg = self.candidate(&params.config)?;
                let timeout = params.timeout_secs.map(Duration::from_secs);
                let applied = self
                    .apply(
                        cfg,
                        ChangeSource::Local,
                        None,
                        timeout,
                        params.base_revision,
                        false,
                    )
                    .await?;
                to_value(&applied)
            }
            Request::ApplyFiles(params) => {
                let (network, local) = self.paths.read().map_err(api_err)?;
                let cfg = self.candidate(&ConfigInput { network, local })?;
                let timeout = params.timeout_secs.map(Duration::from_secs);
                let applied = self
                    .apply(cfg, ChangeSource::Local, None, timeout, None, true)
                    .await?;
                to_value(&applied)
            }
            Request::Confirm => to_value(&self.confirm(true)?),
            Request::Rollback => {
                self.rollback("rolled back on request").await?;
                to_value(&Done {})
            }
            Request::Interfaces => to_value(&self.interfaces().await?),
            Request::MacTable => to_value(&self.mac_table().await?),
            Request::VlanTraffic => to_value(&self.vlan_traffic().await?),
            Request::History(params) => to_value(&self.history(params.since_ms)),
            Request::Unblock(params) => {
                if !self.runtime.blocked.remove(&params.port) {
                    return Err(ApiError::new(format!(
                        "{} is not blocked by the loop guard",
                        params.port
                    )));
                }
                self.events
                    .info(format!("{} was enabled again on request", params.port));
                self.reconcile().await;
                to_value(&Done {})
            }
            Request::ProbeNow => {
                self.generation += 1;
                self.publish();
                to_value(&Done {})
            }
            Request::Invite(rules) => {
                let Some(confirmed) = &self.confirmed else {
                    return Err(ApiError::new(
                        "this site is not part of a network yet; set it up first (wanderlan)",
                    ));
                };
                join::check_rules(&confirmed.network, confirmed.self_id(), &rules)
                    .map_err(|e| ApiError::new(format!("the rules cannot be used: {e}")))?;
                let invite = self.invites.create(now_secs(), rules).map_err(api_err)?;
                let invitation = self.invitation(&invite);
                self.events.info(format!(
                    "Invitation {} created; on the new site run: {}",
                    invite.code, invitation.command
                ));
                to_value(&invitation)
            }
            Request::Invites => {
                let list: Vec<Invitation> = self
                    .invites
                    .list()
                    .iter()
                    .map(|i| self.invitation(i))
                    .collect();
                to_value(&list)
            }
            Request::DeleteInvite(params) => {
                match self.invites.remove(&params.code).map_err(api_err)? {
                    Some(code) => {
                        self.events.info(format!("Invitation {code} was deleted"));
                        to_value(&Done {})
                    }
                    None => Err(ApiError::new(format!(
                        "there is no open invitation {}",
                        params.code
                    ))),
                }
            }
            Request::Join(_) => Err(ApiError::new("join requests are handled separately")),
            Request::Reset => to_value(&self.reset().await?),
            Request::Networks | Request::AddNetwork(_) | Request::RemoveNetwork(_) => Err(
                ApiError::new("the networks of the box are handled by the daemon itself"),
            ),
        }
    }

    /// An invitation as shown to the user.
    fn invitation(&self, invite: &Invite) -> Invitation {
        let command = self
            .confirmed
            .as_ref()
            .and_then(|c| c.self_site().map(|s| (s.address, c.control_port())))
            .map(|(address, port)| {
                // The same name on the new box is a good guess.
                let net = if self.network == api::DEFAULT_NETWORK {
                    String::new()
                } else {
                    format!("--net {} ", self.network)
                };
                if port == config::DEFAULT_CONTROL_PORT {
                    format!("wanderlan {net}join {address} {}", invite.code)
                } else {
                    format!("wanderlan {net}join {address}:{port} {}", invite.code)
                }
            })
            .unwrap_or_default();
        Invitation {
            code: invite.code.clone(),
            command,
            created_secs: invite.created_secs,
            in_use: self.invites.is_reserved(&invite.code),
            rules: invite.rules.clone(),
        }
    }

    // ---- joining -------------------------------------------------------------

    /// A site asks to join with an invitation of this site: check the code,
    /// add the site (its address is the source of the connection) and answer
    /// with the new network. The change is confirmed as soon as the new site
    /// answers probes, or rolled back after `commit_timeout`.
    async fn admit(&mut self, from: Ipv4Addr, request: JoinRequest) -> JoinReply {
        let error = |message: String| JoinReply::Error { message };
        let now = Instant::now();
        // Addresses without recent failures are forgotten.
        self.join_failures.retain(|_, f| {
            f.back()
                .is_some_and(|t| now.duration_since(*t) <= JOIN_FAILURE_WINDOW)
        });
        let failures = self.join_failures.entry(from).or_default();
        while failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > JOIN_FAILURE_WINDOW)
        {
            failures.pop_front();
        }
        if failures.len() >= JOIN_MAX_FAILURES {
            return error(
                "too many wrong invitation codes from this address; wait a minute".into(),
            );
        }
        if request.protocol != join::PROTOCOL {
            return error(format!(
                "this site (wanderland {}) speaks join protocol {}, not {}; update both sites",
                wanderlan_core::version_line(),
                join::PROTOCOL,
                request.protocol
            ));
        }
        let Some(confirmed) = self.confirmed.clone() else {
            return error("this site is not part of a network".into());
        };
        let Some(invite) = self.invites.find_free(&request.code).cloned() else {
            failures.push_back(now);
            self.events.warn(format!(
                "Join attempt from {from} with an unknown or used invitation code"
            ));
            return error("unknown or already used invitation code".into());
        };
        if self.pending.is_some() {
            return JoinReply::Busy;
        }
        let Ok(id) = request.id.parse::<SiteId>() else {
            return error(format!(
                "the joining site sent no valid identity (wanderland {}); update it",
                request.version
            ));
        };
        // An identity that is already a site: the box proves it holds its
        // key (else anyone with an invitation could pose as a site that is
        // down and get its traffic).
        if confirmed.network.site(id).is_some()
            && let Err(reply) = self.check_join_proof(from, id, &request)
        {
            return reply;
        }
        let joiner = join::Joiner {
            name: &request.name,
            name_is_default: request.name_is_default,
            address: from,
            id,
        };
        let alive = self.running_peers();
        let admission = match join::add_site(
            &confirmed.network,
            confirmed.self_id(),
            joiner,
            &alive,
            &invite.rules,
        ) {
            Ok(a) => a,
            // Perhaps just reset: the joiner asks again shortly.
            Err(
                e @ (join::JoinError::StillRunning { .. } | join::JoinError::IdentityInUse { .. }),
            ) => {
                debug!("The site at {from} cannot join yet: {e}");
                return JoinReply::Retry {
                    message: e.to_string(),
                };
            }
            Err(e) => {
                self.events
                    .warn(format!("The site at {from} cannot join: {e}"));
                return error(e.to_string());
            }
        };
        if let Some(old) = admission.took_over {
            self.events.emit(
                Level::Warn,
                Some(&admission.name),
                format!(
                    "A new box ({}) at {from} takes over site \"{}\" from the box that does not answer any more ({})",
                    id.fingerprint(),
                    admission.name,
                    old.fingerprint()
                ),
            );
        }
        let via = confirmed.self_name().to_string();
        // Nothing changes here (the same box at the same place): the code
        // is not used up, the box may still fail to come up.
        if !admission.changed {
            self.events.emit(
                Level::Info,
                Some(&admission.name),
                format!(
                    "Site \"{}\" at {from} joined again with invitation {}; nothing changed, the invitation stays open",
                    admission.name, invite.code
                ),
            );
            let network = render_network_wire(&view::view_for(&confirmed.network, admission.site));
            return JoinReply::Ok {
                site: admission.name,
                via,
                network,
            };
        }
        // The new site gets its view of the network only, in the form every
        // version reads.
        let network = render_network_wire(&view::view_for(&admission.network, admission.site));
        let joined = admission.site;
        let site = admission.network.label(joined);
        if !admission.added {
            self.events.emit(
                Level::Info,
                Some(&site),
                format!("Site \"{site}\" joins again through this site and becomes a client of it"),
            );
        }
        let cfg = Config {
            network: admission.network,
            local: confirmed.local.clone(),
        };
        let source = ChangeSource::Join { site: site.clone() };
        match self
            .apply(cfg, source, Some(joined), None, None, false)
            .await
        {
            Ok(_) => {
                self.invites.reserve(&invite.code);
                if let Err(answer) = self.uplink_takes_join().await {
                    let why = match &answer {
                        JoinReply::Error { message } => message.clone(),
                        _ => "the server of this site is busy".into(),
                    };
                    self.events.warn(format!(
                        "Site \"{site}\" at {from} cannot join through this site now: {why}"
                    ));
                    if let Err(e) = self.rollback(&why).await {
                        self.events.error(e.message);
                    }
                    return answer;
                }
                self.events.emit(
                    Level::Info,
                    Some(&site),
                    format!(
                        "Site \"{site}\" at {from} is joining with invitation {}; waiting for it to come up",
                        invite.code
                    ),
                );
                JoinReply::Ok { site, via, network }
            }
            Err(e) => error(format!("cannot add the site here: {}", e.message)),
        }
    }

    /// Asks a box that joins with an identity of the network for its proof,
    /// or checks the proof it sent.
    fn check_join_proof(
        &mut self,
        from: Ipv4Addr,
        id: SiteId,
        request: &JoinRequest,
    ) -> Result<(), JoinReply> {
        let now = Instant::now();
        self.join_nonces
            .retain(|_, (_, at)| now.duration_since(*at) < JOIN_PROOF_TIMEOUT);
        let Some(me) = self.identity.clone() else {
            return Ok(());
        };
        let Some(proof) = &request.proof else {
            let nonce = crate::random::bytes::<16>().map_err(|e| JoinReply::Error {
                message: format!("{e:#}"),
            })?;
            self.join_nonces.insert((from, id), (nonce.to_vec(), now));
            return Err(JoinReply::Prove {
                id: me.id().to_string(),
                nonce: join::to_hex(&nonce),
            });
        };
        // The nonce stays until it expires: the box asks again with the same
        // proof after "busy" or "not yet".
        let valid = self.join_nonces.get(&(from, id)).is_some_and(|(nonce, _)| {
            join::from_hex(&proof.nonce).as_deref() == Some(nonce.as_slice())
                && me.pair(&id).is_some_and(|keys| {
                    join::from_hex(&proof.tag).is_some_and(|tag| {
                        keys.sync_verify(&join::proof_parts(nonce, &request.code), &tag)
                    })
                })
        });
        if valid {
            Ok(())
        } else {
            self.events.warn(format!(
                "The box at {from} claims the identity {} of a site but cannot prove it holds its key",
                id.fingerprint()
            ));
            Err(JoinReply::Error {
                message: "this box could not prove that it holds the key of its identity".into(),
            })
        }
    }

    /// A client that admits a site: its own server takes the change over
    /// at once (it knows the whole network: names, addresses and identities
    /// outside this site's part). The answer for the joining box if not.
    async fn uplink_takes_join(&mut self) -> Result<(), JoinReply> {
        let Some(cfg) = self.config.clone() else {
            return Ok(());
        };
        let Some(up_id) = cfg.self_site().and_then(|s| s.uplink) else {
            return Ok(());
        };
        let up = cfg.network.label(up_id);
        let unreachable = |why: String| JoinReply::Error {
            message: format!("the server of this site (\"{up}\") cannot be asked: {why}"),
        };
        let Some(target) = self.control_target(up_id) else {
            return Err(unreachable("its address is unknown".into()));
        };
        let (Some(me), Some(k)) = (self.own_id(), self.peer_keys.get(&up_id).cloned()) else {
            return Err(unreachable("its identity is not known yet".into()));
        };
        let text = render_network_wire(&cfg.network);
        match sync_net::push_sync(target, me, k.id, &k.keys, &text).await {
            Ok(sync_net::PushAnswer::Ok) => {
                self.pushed = Some(config::fingerprint(&cfg.network));
                Ok(())
            }
            Ok(sync_net::PushAnswer::Busy) => Err(JoinReply::Busy),
            Ok(sync_net::PushAnswer::Refused(reason)) => Err(JoinReply::Error {
                message: format!("the server of this site (\"{up}\") refused it: {reason}"),
            }),
            Err(e) => Err(unreachable(format!("{e:#}"))),
        }
    }

    /// The peers that answered within the last half minute (for a peer with
    /// a key: with its key), for joins: a join does not take over the entry
    /// of a site that still runs. A site this one does not probe (a client of
    /// another server) counts as gone: a reset box may join again anywhere.
    fn running_peers(&self) -> BTreeSet<SiteId> {
        let now = std::time::Instant::now();
        self.probes.read().map_or_else(
            |_| BTreeSet::new(),
            |table| {
                table
                    .iter()
                    .filter(|(_, p)| {
                        p.last_reply_at
                            .is_some_and(|t| now.duration_since(t) < ALIVE_WINDOW)
                    })
                    .map(|(name, _)| *name)
                    .collect()
            },
        )
    }

    /// Starts joining a network. The connection to the inviting site runs in
    /// the background; its answer comes back as [`Input::JoinAnswer`].
    fn start_join(
        &mut self,
        params: JoinParams,
        reply: oneshot::Sender<Result<serde_json::Value, ApiError>>,
    ) {
        let checked = if self.config.is_some() || self.pending.is_some() {
            Err("this site is already part of a network; reset it first to join another one (wanderlan reset)".to_string())
        } else if self.joining {
            Err("a join is already in progress".to_string())
        } else if let Some(msg) = config::site_name_error(&params.name) {
            Err(msg)
        } else if let Some(msg) = config::ifname_error(&params.trunk) {
            Err(format!("trunk: {msg}"))
        } else {
            join::parse_target(&params.target, config::DEFAULT_CONTROL_PORT)
        };
        let (address, port) = match checked {
            Ok(t) => t,
            Err(message) => {
                let _ = reply.send(Err(ApiError::new(message)));
                return;
            }
        };
        let target = SocketAddrV4::new(address, port);
        let Some(identity) = self.ensure_identity() else {
            let _ = reply.send(Err(ApiError::new(
                "this box has no identity (see the log); cannot join",
            )));
            return;
        };
        self.joining = true;
        self.events.info(format!(
            "Asking {target} to let this site join as \"{}\"",
            params.name
        ));
        let request = JoinRequest {
            protocol: join::PROTOCOL,
            code: params.code.clone(),
            name: params.name.clone(),
            name_is_default: params.name_is_default,
            version: wanderlan_core::version_line().into(),
            id: identity.id().to_string(),
            proof: None,
        };
        let inputs = self.inputs.clone();
        let mut request = request;
        tokio::spawn(async move {
            let start = Instant::now();
            let mut proved = false;
            let result = loop {
                match sync_net::join(target, &request).await {
                    // The identity is already a site: prove it (once).
                    Ok(JoinReply::Prove { id, nonce }) if !proved => {
                        proved = true;
                        let keys = id.parse::<SiteId>().ok().and_then(|id| identity.pair(&id));
                        match (keys, join::from_hex(&nonce)) {
                            (Some(keys), Some(bytes)) => {
                                let tag = keys.sync_tag(&join::proof_parts(&bytes, &request.code));
                                request.proof = Some(join::JoinProof {
                                    nonce,
                                    tag: join::to_hex(&tag),
                                });
                            }
                            _ => {
                                break Err("the inviting site sent an invalid proof request".into());
                            }
                        }
                    }
                    Ok(JoinReply::Busy) if start.elapsed() < JOIN_BUSY_RETRY => {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    Ok(JoinReply::Retry { .. }) if start.elapsed() < JOIN_TAKEOVER_RETRY => {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    Ok(answer) => break Ok(answer),
                    Err(e) => break Err(format!("{e:#}")),
                }
            };
            let _ = inputs
                .send(Input::JoinAnswer {
                    params,
                    result,
                    reply,
                })
                .await;
        });
    }

    /// Applies the configuration received from the inviting site. It is
    /// confirmed automatically as soon as that site answers probes.
    async fn finish_join(
        &mut self,
        params: JoinParams,
        result: Result<JoinReply, String>,
    ) -> Result<serde_json::Value, ApiError> {
        self.joining = false;
        let answer = result.map_err(|e| {
            self.events.warn(format!("Joining failed: {e}"));
            ApiError::new(format!("cannot join: {e}"))
        })?;
        let (site, via, text) = match answer {
            JoinReply::Ok { site, via, network } => (site, via, network),
            JoinReply::Busy => {
                return Err(ApiError::new(
                    "the inviting site is busy with another change; try again in a minute",
                ));
            }
            JoinReply::Prove { .. } => {
                return Err(ApiError::new(
                    "the inviting site did not accept the proof of this box's identity",
                ));
            }
            JoinReply::Error { message } | JoinReply::Retry { message } => {
                self.events.warn(format!("Joining was refused: {message}"));
                return Err(ApiError::new(format!(
                    "the inviting site refused: {message}"
                )));
            }
        };
        if self.config.is_some() || self.pending.is_some() {
            return Err(ApiError::new("this site was configured meanwhile"));
        }
        let network = match config::parse_network(&text) {
            Ok(parsed) => parsed.network,
            Err(diags) => {
                let msg = diags.first().map_or_else(String::new, ToString::to_string);
                return Err(ApiError::new(format!(
                    "the configuration from \"{via}\" cannot be read: {msg}"
                )));
            }
        }
        .ok_or_else(|| ApiError::new(format!("\"{via}\" sent an empty configuration")))?;
        // This site in it: by its identity (an inviting site of an older
        // version names it only).
        let me = self
            .own_id()
            .filter(|id| network.site(*id).is_some())
            .or_else(|| network.by_name(&site).map(|s| s.id));
        let local = config::Local {
            self_id: me,
            trunk: vec![params.trunk.clone()],
            ..Default::default()
        };
        let mut cfg = sync::accept(network, &local, None, false, self.own_id()).map_err(|e| {
            ApiError::new(format!(
                "the configuration from \"{via}\" cannot be used: {e}"
            ))
        })?;
        // Ports another network of the box has: this site takes others (its
        // own change, passed on like every other).
        if self.choose_free_ports(&mut cfg) {
            cfg.network.revision = config::next_revision(cfg.network.revision);
        }
        let revision = cfg.network.revision;
        let sites = cfg.network.sites.len();
        let inviter = cfg.self_site().and_then(|s| s.uplink);
        let source = ChangeSource::Sync { peer: via.clone() };
        self.apply(cfg, source, inviter, None, None, false).await?;
        self.events.info(format!(
            "Joining the network via \"{via}\" as \"{site}\" ({sites} sites); confirmed automatically as soon as \"{via}\" answers"
        ));
        to_value(&Joined {
            site,
            via,
            sites,
            revision,
        })
    }

    /// Gives this site ports no other network of the box takes if its own
    /// are taken (a new network, a join); true if it changed them.
    fn choose_free_ports(&self, cfg: &mut Config) -> bool {
        let taken = self.host.ports_of_others(&self.network);
        let uses = |control: u16, tunnel: u16| {
            [control, tunnel, tunnel.saturating_add(1)]
                .iter()
                .any(|p| taken.contains(p))
        };
        let me = cfg.self_id();
        let control = cfg.network.control_port_of(me);
        if !uses(control, cfg.network.tunnel_port_of(me)) {
            return false;
        }
        // In steps of 10: easy to read in a firewall.
        let Some(port) = (1..=500u16)
            .filter_map(|k| control.checked_add(k * 10))
            .filter(|p| *p < 65000)
            .find(|p| !uses(*p, config::tunnel_port_for(*p)))
        else {
            return false;
        };
        let Some(site) = cfg.network.site_mut(me) else {
            return false;
        };
        site.ports.control = Some(port);
        site.ports.tunnel = None;
        self.events.info(format!(
            "Another network of this box has UDP/TCP {control}: this site takes port {port} (and {} for its tunnels) in network \"{}\"",
            config::tunnel_port_for(port),
            self.network
        ));
        true
    }

    /// Back to the state after installation: everything wanderlan created is
    /// removed, the configuration files become the templates again and the
    /// state (confirmed copies, invitations) is deleted. Every step runs even
    /// if an earlier one fails; the end state is checked afterwards.
    async fn reset(&mut self) -> Result<ResetReport, ApiError> {
        if self.joining {
            return Err(ApiError::new(
                "a join is in progress; wait until it has finished",
            ));
        }
        self.pending = None;
        self.confirmed = None;
        self.set_config(None);
        // Nothing of the box is this network's any more.
        self.bridges.clear();
        self.conflicts.clear();
        self.runtime = Runtime::default();
        self.applied_key = None;
        self.last_error = None;
        self.rejected.clear();
        self.fetch_backoff.clear();
        self.grace.clear();
        self.grace_keys.clear();
        self.relayed.clear();
        self.own_change_up = false;
        self.flushes.clear();
        self.invites.clear();
        self.join_failures.clear();
        self.refused.clear();
        self.diverged_since = None;
        self.sync_problem = None;
        self.expected.clear();
        if let Ok(mut fdb) = self.fdb_cache.lock() {
            *fdb = None;
        }
        if let Ok(mut d) = self.derived.lock() {
            *d = None;
        }
        self.generation += 1;
        if let Ok(mut t) = self.probes.write() {
            t.clear();
        }
        if let Ok(mut r) = self.rates.write() {
            r.clear();
        }
        self.publish();
        let mut problems = Vec::new();
        let removed = match self.host.reset(&self.network, |c| info!("{c}")).await {
            Ok(changes) => changes.len(),
            Err(e) => {
                problems.push(format!("removing the interfaces and rules failed: {e}"));
                0
            }
        };
        // The box becomes a new box: a join afterwards gets a new identity.
        self.identity = None;
        if let Err(e) = self.paths.reset() {
            problems.push(format!("{e:#}"));
        }
        // What other networks of the box have stays.
        let alone = !self.host.others_configured(&self.network).await;
        match reconcile::Actual::observe(&self.sys).await {
            Ok(_) if !alone => {}
            Ok(actual) => {
                let left: Vec<String> = reconcile::plan_teardown(&actual)
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                if !left.is_empty() {
                    problems.push(format!("still present: {}", left.join("; ")));
                }
            }
            Err(e) => problems.push(format!("cannot check the interfaces: {e}")),
        }
        problems.extend(self.paths.leftovers());
        if problems.is_empty() {
            self.events.warn(format!(
                "Reset: the bridge, the tunnels and all rules were removed ({removed} changes), the configuration and the invitations deleted; this site is unconfigured again. Other sites still list it until it is removed there (Sites, d)."
            ));
        } else {
            self.events
                .error(format!("Reset incomplete: {}", problems.join("; ")));
        }
        Ok(ResetReport { removed, problems })
    }

    fn config_texts(&self) -> ConfigTexts {
        match &self.config {
            Some(cfg) => ConfigTexts {
                configured: true,
                network: render_network(&cfg.network),
                local: render_local(&cfg.local, Some(&cfg.network)),
                revision: cfg.network.revision,
                fingerprint: config::fingerprint(&cfg.network),
            },
            None => ConfigTexts {
                configured: false,
                network: String::new(),
                local: self.paths.read().map(|(_, l)| l).unwrap_or_default(),
                revision: 0,
                fingerprint: String::new(),
            },
        }
    }

    fn diff_against_running(&self, cfg: &Config) -> String {
        let (old_network, old_local) = self.config.as_ref().map_or_else(
            || (String::new(), String::new()),
            |c| {
                (
                    render_network(&c.network),
                    render_local(&c.local, Some(&c.network)),
                )
            },
        );
        let mut diff = unified_diff(
            "running/network.toml",
            &old_network,
            "new/network.toml",
            &render_network(&cfg.network),
        );
        diff.push_str(&unified_diff(
            "running/local.toml",
            &old_local,
            "new/local.toml",
            &render_local(&cfg.local, Some(&cfg.network)),
        ));
        diff
    }

    /// Loads texts given by a user. A `network.toml` written without the
    /// `id` lines (an older form, by hand) has the identities of `known`
    /// (the confirmed network); a `local.toml` naming this site by its
    /// identity then finds it. Errors keep the lines of the texts as given.
    fn load_input(
        &self,
        network: &str,
        local: &str,
        known: Option<&Network>,
    ) -> Result<(Loaded, Vec<Diagnostic>), Vec<Diagnostic>> {
        let first = load_texts(network, local);
        let Err(diags) = first else {
            return first;
        };
        let fixed = known.and_then(|known| {
            let mut parsed = config::parse_network(network).ok()?.network?;
            let before = parsed.clone();
            keep_ids(known, &mut parsed, &mut config::Local::default());
            (parsed != before).then(|| render_network(&parsed))
        });
        match fixed.map(|text| load_texts(&text, local)) {
            Some(Ok(loaded)) => Ok(loaded),
            _ => Err(diags),
        }
    }

    fn validate(&self, input: &ConfigInput) -> Validation {
        let known = self.confirmed.as_ref().map(|c| &c.network);
        match self.load_input(&input.network, &input.local, known) {
            Ok((Loaded::Configured(cfg), warnings)) => Validation {
                valid: true,
                diagnostics: warnings,
                diff: self.diff_against_running(&cfg),
            },
            Ok((Loaded::Unconfigured { .. }, warnings)) => Validation {
                valid: false,
                diagnostics: warnings,
                diff: String::new(),
            },
            Err(diagnostics) => Validation {
                valid: false,
                diagnostics,
                diff: String::new(),
            },
        }
    }

    fn candidate(&self, input: &ConfigInput) -> Result<Config, ApiError> {
        let known = self.confirmed.as_ref().map(|c| &c.network);
        match self.load_input(&input.network, &input.local, known) {
            Ok((Loaded::Configured(cfg), _)) => Ok(cfg),
            Ok((Loaded::Unconfigured { .. }, _)) => Err(ApiError::new(
                "local.toml does not set `self`; the configuration is incomplete",
            )),
            Err(diags) => Err(invalid(diags)),
        }
    }

    /// Applies a configuration and starts the confirmation timer.
    async fn apply(
        &mut self,
        mut cfg: Config,
        source: ChangeSource,
        peer: Option<SiteId>,
        timeout: Option<Duration>,
        base_revision: Option<u64>,
        from_files: bool,
    ) -> Result<Applied, ApiError> {
        if self.pending.is_some() {
            return Err(ApiError::new(
                "another change is waiting for confirmation; confirm or roll it back first",
            ));
        }
        // Identities are not edited by hand: a local change (also the first
        // configuration) keeps those of the confirmed network and carries the
        // identity of this box in its own entry, whatever the editor wrote.
        if source == ChangeSource::Local {
            if let Some(confirmed) = &self.confirmed {
                keep_ids(&confirmed.network, &mut cfg.network, &mut cfg.local);
            }
            if let Some(identity) = self.ensure_identity() {
                sync::claim_own_id(&mut cfg.network, &mut cfg.local, identity.id());
            }
        }
        if let Some(base) = base_revision {
            self.check_base(base, &cfg)?;
        }
        if source == ChangeSource::Local
            && let Some(confirmed) = &self.confirmed
        {
            policy::check_edit(&confirmed.network, &cfg.network, confirmed.self_id())
                .map_err(ApiError::new)?;
        }
        if source == ChangeSource::Local {
            // The first configuration takes ports no other network of the
            // box has (a later change of them is checked below).
            if self.confirmed.is_none() && self.choose_free_ports(&mut cfg) {
                cfg.network.revision = config::next_revision(cfg.network.revision);
            }
            // What another network of the box has stays its own: a change
            // here does not take it (a synced one is applied, and the
            // status tells what it lost).
            let wanted = desired::derive_in(&self.scope(), &cfg, &self.runtime);
            let lost = self.host.check(&self.network, &wanted).await;
            if !lost.is_empty() {
                return Err(ApiError::new(format!(
                    "another network of this box has it: {}",
                    lost.join("; ")
                )));
            }
        }
        let warnings: Vec<Diagnostic> = config::validate(&cfg.network, Some(&cfg.local))
            .into_iter()
            .filter(|d| !d.is_error())
            .collect();
        let diff = self.diff_against_running(&cfg);
        // Within the range of commit_timeout: a huge value from the API
        // would never roll back (or overflow the deadline).
        let timeout = timeout
            .unwrap_or(cfg.commit_timeout())
            .clamp(MIN_TIMEOUT, config::COMMIT_TIMEOUT_RANGE.1);
        let fingerprint = config::fingerprint(&cfg.network);
        let previous = self.config.clone();
        self.set_config(Some(cfg));
        let changes = match self.reconcile_once().await {
            Ok(c) => c,
            Err(e) => {
                self.set_config(previous);
                self.reconcile().await;
                if from_files {
                    let _ = self.paths.restore_confirmed();
                }
                return Err(ApiError::new(format!(
                    "cannot apply the configuration: {e}; the previous state was restored"
                )));
            }
        };
        if from_files && let Err(e) = self.paths.set_marker() {
            warn!("{e:#}");
        }
        let (who, label) = match &source {
            ChangeSource::Local => ("A local change".to_string(), None),
            ChangeSource::Sync { peer } => (
                format!("The configuration from \"{peer}\""),
                Some(peer.clone()),
            ),
            ChangeSource::Join { site } => (format!("The new site \"{site}\""), Some(site.clone())),
        };
        self.pending = Some(Pending {
            previous,
            deadline: Instant::now() + timeout,
            applied_at: std::time::Instant::now(),
            source,
            peer,
            from_files,
            fingerprint,
            announced: None,
            graced: std::mem::take(&mut self.grace_added),
        });
        self.events.emit(
            Level::Info,
            label.as_deref(),
            format!(
                "{who} was applied ({} system changes); it is rolled back unless confirmed within {} s",
                changes.len(),
                timeout.as_secs()
            ),
        );
        Ok(Applied {
            confirm_within_secs: timeout.as_secs(),
            diff,
            changes: changes.iter().map(ToString::to_string).collect(),
            warnings,
        })
    }

    /// Refuses a local change that is based on an outdated revision.
    fn check_base(&self, base: u64, cfg: &Config) -> Result<(), ApiError> {
        if let Some(c) = &self.confirmed
            && c.network.revision > base
        {
            return Err(ApiError::new(format!(
                "the configuration changed meanwhile (now revision {}, your change is based on {base}); reload it and try again",
                c.network.revision
            )));
        }
        // What a peer would have if it were up to date: a client of this
        // site its view, any other peer the whole of what this site knows.
        let me = cfg.self_id();
        let expected = |network: &Network, peer: SiteId| {
            let client = network.site(peer).is_some_and(|s| s.uplink == Some(me));
            if client {
                config::fingerprint(&view::view_for(network, peer))
            } else {
                config::fingerprint(network)
            }
        };
        // Only peers that stay peers (same identity and address) and answer
        // now count; a site of another or an old network must not block a
        // change.
        let current = self.direct_peers();
        let relevant: BTreeSet<SiteId> = topology::tunnels_for(&cfg.network, me)
            .into_iter()
            .filter(|t| current.get(&t.peer.id) == Some(&t.peer.address))
            .map(|t| t.peer.id)
            .collect();
        let base_network = self.confirmed.as_ref().map(|c| &c.network);
        let newer = self.probes.read().ok().and_then(|t| {
            t.iter()
                .filter(|(name, p)| p.reachable && relevant.contains(*name))
                .filter_map(|(name, p)| p.announce.map(|a| (*name, a)))
                .find(|(name, a)| {
                    let theirs = fingerprint_hex(&a.fingerprint);
                    // A peer with the content this change is based on (only
                    // a higher revision number) does not make it outdated.
                    a.revision > base
                        && theirs != expected(&cfg.network, *name)
                        && base_network.is_none_or(|m| theirs != expected(m, *name))
                })
        });
        if let Some((peer, a)) = newer {
            return Err(ApiError::new(format!(
                "peer \"{}\" has a newer network configuration (revision {}); wait for the sync, reload and try again",
                cfg.network.label(peer),
                a.revision
            )));
        }
        Ok(())
    }

    /// Makes the pending change permanent.
    fn confirm(&mut self, user: bool) -> Result<Confirmed, ApiError> {
        let Some(pending) = self.pending.take() else {
            return Err(ApiError::new("there is no change to confirm"));
        };
        let Some(mut cfg) = self.config.clone() else {
            return Err(ApiError::new("there is no configuration"));
        };
        if pending.source == ChangeSource::Local {
            let previous_version = self.confirmed.as_ref().map(|c| Version::of(&c.network));
            let changed = previous_version
                .as_ref()
                .is_none_or(|v| v.fingerprint != config::fingerprint(&cfg.network));
            if changed {
                let base = previous_version.map_or(0, |v| v.revision);
                // A revision written by hand counts, within a step: one near
                // the end of the range would leave the sync only the
                // fingerprint (the files hold at most an i64).
                let wanted = cfg
                    .network
                    .revision
                    .min(base.saturating_add(config::MAX_REVISION_STEP));
                cfg.network.revision = config::next_revision(base.max(wanted));
            }
            self.rejected.clear();
            // A change of its own: the last refusal is history.
            self.sync_problem = None;
            self.own_change_up = true;
        }
        // The invitation is used up before the new site is written down: a
        // crash in between loses the code rather than letting it in twice.
        if matches!(pending.source, ChangeSource::Join { .. }) {
            match self.invites.consume_reserved() {
                Ok(Some(code)) => self.events.info(format!("Invitation {code} is used up")),
                Ok(None) => {}
                Err(e) => self.events.error(format!("{e:#}")),
            }
        }
        if let Err(e) = self.paths.write_confirmed(&cfg) {
            // /etc does not run ahead of what is in effect (written in part).
            if let Some(previous) = &self.confirmed
                && let Err(again) = self.paths.write_confirmed(previous)
            {
                self.events.error(format!("{again:#}"));
            }
            self.pending = Some(pending);
            return Err(ApiError::new(format!(
                "cannot write the configuration files: {e:#}"
            )));
        }
        self.paths.clear_marker();
        // Peers it removed get the new network only from now on (until now
        // the confirmed one was offered): their grace starts here.
        let until = Instant::now() + GRACE;
        for name in &pending.graced {
            if let Some((_, t)) = self.grace.get_mut(name) {
                *t = until;
            }
        }
        self.confirmed = Some(cfg.clone());
        self.set_config(Some(cfg.clone()));
        // A change the user confirmed lifts the blocks of the loop guard
        // (it may have fixed the loop); an automatic confirmation does not.
        if user {
            self.runtime.blocked.clear();
        }
        if user {
            self.events.info(format!(
                "Change confirmed; network revision {} is in effect",
                cfg.network.revision
            ));
        }
        Ok(Confirmed {
            revision: cfg.network.revision,
        })
    }

    /// Reverts the pending change.
    async fn rollback(&mut self, reason: &str) -> Result<(), ApiError> {
        let Some(pending) = self.pending.take() else {
            return Err(ApiError::new("there is no change to roll back"));
        };
        let peer = match &pending.source {
            ChangeSource::Sync { peer } => {
                self.reject(pending.fingerprint.clone());
                if let Some(announced) = &pending.announced {
                    self.reject(announced.clone());
                }
                // A change of a client of this site: the client learns that
                // it was rolled back (else it sends it again at once).
                let me = pending.previous.as_ref().map(Config::self_id);
                let name = pending.previous.as_ref().map(|c| c.self_name().to_string());
                if let Some(id) = pending.peer
                    && pending.previous.as_ref().is_some_and(|c| {
                        c.network
                            .site(id)
                            .is_some_and(|s| s.uplink.is_some() && s.uplink == me)
                    })
                {
                    self.relayed.remove(&id);
                    self.refused.insert(
                        id,
                        Refusal {
                            reason: format!(
                                "it was rolled back at \"{}\": {reason}",
                                name.unwrap_or_default()
                            ),
                            at: std::time::Instant::now(),
                        },
                    );
                }
                Some(peer.clone())
            }
            ChangeSource::Join { site } => {
                self.invites.release();
                // A site that was there before only wanted to hang off this
                // site after joining again here.
                let before = pending
                    .previous
                    .as_ref()
                    .and_then(|c| Some((c, c.network.site(pending.peer?)?)))
                    .map(|(c, s)| s.uplink.map(|up| c.network.label(up)));
                let text = match before {
                    Some(Some(up)) => format!(
                        "Site \"{site}\" did not come up through this site in time; it stays a client of \"{up}\" and the invitation can be used again"
                    ),
                    Some(None) => format!(
                        "Site \"{site}\" did not come up through this site in time; it stays a server and the invitation can be used again"
                    ),
                    None => format!(
                        "The new site \"{site}\" did not come up in time; it was removed again and its invitation can be used again"
                    ),
                };
                self.events.emit(Level::Warn, Some(site), text);
                Some(site.clone())
            }
            ChangeSource::Local => None,
        };
        let had_previous = pending.previous.is_some();
        self.set_config(pending.previous);
        if had_previous {
            self.reconcile().await;
        } else {
            // Nothing before: what this network built goes, not what the
            // other networks of the box have.
            self.bridges.clear();
            self.conflicts.clear();
            if let Err(e) = self.host.reset(&self.network, |c| info!("{c}")).await {
                self.events.error(format!("cleanup failed: {e}"));
            }
        }
        if pending.from_files {
            match self.paths.restore_confirmed() {
                Ok(_) => self.events.info(
                    "The confirmed configuration files were restored; the rejected ones are kept as *.rejected",
                ),
                Err(e) => self.events.error(format!("{e:#}")),
            }
        }
        self.events.emit(
            Level::Warn,
            peer.as_deref(),
            format!("Rolled back to the previous configuration: {reason}"),
        );
        Ok(())
    }

    // ---- reconciliation ------------------------------------------------------

    /// Reconciles; errors are logged only when they change.
    async fn reconcile(&mut self) {
        let result = self.reconcile_once().await;
        // The loop guard needs the ports even when not everything worked
        // (tunnels from before a restart forward traffic all the same); on
        // success `reconcile_once` has updated them.
        if result.is_err() {
            self.update_port_map().await;
        }
        if let Err(e) = result {
            let msg = e.to_string();
            if self.last_error.as_deref() != Some(msg.as_str()) {
                self.events
                    .error(format!("Cannot apply the configuration: {msg}"));
            }
            self.last_error = Some(msg);
        }
    }

    async fn reconcile_once(&mut self) -> Result<Vec<Change>, reconcile::ReconcileError> {
        // Loaded by a firewall at any time: looked at before every round.
        self.runtime.conntrack = std::path::Path::new(CONNTRACK_MAX).exists();
        let events = self.events.clone();
        let Some(cfg) = self.config.clone() else {
            // The host waits for every network to say what it wants.
            self.host
                .reconcile(&self.network, None, |c| events.info(c.to_string()))
                .await?;
            return Ok(Vec::new());
        };
        self.runtime.underlay_mtu = self.underlay_mtus(&cfg).await;
        self.refresh_breakout(&cfg).await;
        let desired = Arc::new(desired::derive_in(&self.scope(), &cfg, &self.runtime));
        for (peer, reason) in &desired.unbuilt {
            let peer = cfg.network.label(*peer);
            self.error_once(format!("No tunnel to site \"{peer}\": {reason}"));
        }
        let applied = self
            .host
            .reconcile(&self.network, Some(desired.clone()), |c| {
                events.info(c.to_string());
            })
            .await?;
        // The box waits for another network: nothing was applied.
        self.waiting = applied.waiting;
        if applied.waiting {
            return Ok(Vec::new());
        }
        let changes = applied.changes;
        for conflict in &applied.conflicts {
            self.error_once(format!(
                "Left out because another network on this box has it: {conflict}"
            ));
        }
        self.conflicts = applied.conflicts;
        self.bridges = applied.bridges;
        self.has_ports = applied.ports;
        // A tunnel was built or now sends elsewhere: a keepalive through
        // NAT at once, so that the uplink learns where this site is now
        // (instead of with the next one, up to 15 s later).
        if changes
            .iter()
            .any(|c| matches!(c, Change::CreateTunnel { .. } | Change::SetRemote { .. }))
        {
            self.keepalive.send_modify(|k| k.kick += 1);
        }
        let key = topology::forwarding_key(&cfg.network);
        let network_changed = self.applied_key.as_ref().is_some_and(|k| *k != key);
        if (network_changed || reconcile::ports_changed(&changes)) && !self.bridges.is_empty() {
            let bridges = &self.bridges;
            info!(
                "Flushing learned MAC addresses of {} because the topology changed",
                bridges.join(", ")
            );
            for bridge in bridges {
                if let Err(e) = reconcile::flush_learned(&self.sys, bridge).await {
                    warn!("{e}");
                }
            }
            let now = Instant::now();
            self.flushes
                .extend(FOLLOW_UP_FLUSHES.iter().map(|d| now + *d));
        }
        self.applied_key = Some(key);
        if self.last_error.take().is_some() || !changes.is_empty() {
            info!(
                "Site \"{}\" is up to date ({} tunnels, VLANs {})",
                cfg.self_name(),
                desired.tunnels.len(),
                cfg.network.vlans
            );
        }
        self.update_port_map().await;
        Ok(changes)
    }

    /// What the local breakouts depend on besides the configuration: the
    /// uplinks (always: the status shows them, so the first breakout can
    /// choose one) and the sysctls (only while this site has a breakout).
    async fn refresh_breakout(&mut self, cfg: &config::Config) {
        match self.sys.uplinks().await {
            Ok(uplinks) => self.runtime.uplinks = uplinks,
            Err(e) => debug!("{e}"),
        }
        // Only a client breaks out.
        if cfg
            .self_site()
            .is_none_or(|s| s.breakout.is_empty() || wanderlan_core::topology::is_server(s))
        {
            self.runtime.sysctls.clear();
            self.forward_drops.clear();
            return;
        }
        self.forward_drops = wanderlan_core::sys::breakout::read_forward_drops().await;
        let uplinks = &self.runtime.uplinks;
        let mut devs: BTreeSet<String> = BTreeSet::new();
        devs.extend(uplinks.auto4.clone());
        devs.extend(uplinks.auto6.clone());
        devs.extend(
            cfg.self_site()
                .into_iter()
                .flat_map(|s| s.breakout.values().filter_map(|b| b.uplink.clone())),
        );
        devs.extend(
            uplinks
                .by_name
                .keys()
                .filter(|n| wanderlan_core::breakout::is_breakout_name(n))
                .cloned(),
        );
        let sysctls = &self.sys.sysctls;
        let mut values: BTreeMap<String, String> =
            desired::breakout_sysctl_paths(devs.iter().map(String::as_str))
                .into_iter()
                .filter_map(|p| sysctls.read(&p).map(|v| (p, v)))
                .collect();
        // What wanderlan changed counts with its value before: the wish
        // stays the same once it is fulfilled.
        for (path, changed) in sysctls.changed() {
            values.insert(path, changed.original);
        }
        self.runtime.sysctls = values;
        self.runtime.sysctl_writable = sysctls.writable();
        self.runtime.ipv6_disabled =
            !std::path::Path::new(wanderlan_core::sys::breakout::SYSCTL_ROOT)
                .join("ipv6")
                .exists();
    }

    /// Forgets the learned MAC addresses of the bridges of this network.
    async fn flush_bridges(&self) {
        for name in &self.bridges {
            if let Err(e) = reconcile::flush_learned(&self.sys, name).await {
                warn!("{e}");
            }
        }
    }

    /// wanderlan's bridges with segments of this network: as the host put
    /// them last (another network may share one), until then as derived.
    fn own_bridges(&self, links: &[wanderlan_core::sys::LinkState]) -> Vec<(u32, String)> {
        let mine: BTreeSet<String> = if self.bridges.is_empty() {
            self.derived()
                .desired
                .map(|d| d.bridges().map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            self.bridges.iter().cloned().collect()
        };
        owned_bridges(links)
            .into_iter()
            .filter(|(_, name)| mine.contains(name))
            .collect()
    }

    /// This network, for its desired state.
    fn scope(&self) -> desired::Scope {
        desired::Scope {
            tag: self.tag,
            name: (self.network != api::DEFAULT_NETWORK).then(|| self.network.clone()),
        }
    }

    /// True for a tunnel interface of this network (of any version).
    fn is_own_tunnel(&self, name: &str) -> bool {
        topology::tunnel_tag(name) == Some(self.tag)
            || (self.tag == topology::DEFAULT_TAG
                && name.starts_with(topology::LEGACY_TUNNEL_PREFIX))
    }

    /// Tells the loop guard which interfaces are bridges and tunnel ports.
    async fn update_port_map(&self) {
        let Ok(links) = self.sys.links().await else {
            return;
        };
        let bridges: BTreeSet<u32> = self
            .own_bridges(&links)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        let tunnels = links
            .iter()
            .filter(|l| {
                self.is_own_tunnel(&l.name) && l.master.is_some_and(|m| bridges.contains(&m))
            })
            .map(|l| (l.index, (l.name.clone(), l.isolated.unwrap_or(false))))
            .collect();
        // Further trunk ports (not the first of their list): a loop between
        // two trunk ports shuts the later one down.
        let further: BTreeSet<&str> = self.config.as_ref().map_or_else(BTreeSet::new, |cfg| {
            std::iter::once(&cfg.local.trunk)
                .chain(cfg.local.tunnel_trunks.values())
                .flat_map(|ports| ports.iter().skip(1))
                .map(String::as_str)
                .collect()
        });
        let trunks = links
            .iter()
            .filter(|l| further.contains(l.name.as_str()))
            .map(|l| (l.index, l.name.clone()))
            .collect();
        let new = PortMap {
            bridges,
            tunnels,
            trunks,
        };
        self.ports.send_if_modified(|m| {
            let changed = *m != new;
            if changed {
                *m = new.clone();
            }
            changed
        });
    }

    // ---- status --------------------------------------------------------------

    async fn interfaces(&self) -> Result<Vec<Interface>, ApiError> {
        let links = self.sys.links().await.map_err(api_err)?;
        let carries: BTreeMap<u32, Vec<String>> = self
            .tunnel_interfaces()
            .await
            .into_iter()
            .map(|(index, peers)| {
                (
                    index,
                    peers.into_iter().map(|p| self.peer_label(p)).collect(),
                )
            })
            .collect();
        Ok(interface_list(&links, &carries))
    }

    /// Peer → MTU of the interface its tunnel leaves by (an upper bound of the
    /// path MTU, for the MSS until the path is measured).
    async fn underlay_mtus(&self, cfg: &Config) -> BTreeMap<SiteId, u16> {
        let by_index = self.tunnel_interfaces().await;
        if by_index.is_empty() {
            return BTreeMap::new();
        }
        let Ok(links) = self.sys.links().await else {
            return BTreeMap::new();
        };
        let mut out = BTreeMap::new();
        for (index, peers) in by_index {
            let Some(mtu) = links
                .iter()
                .find(|l| l.index == index)
                .and_then(|l| u16::try_from(l.mtu).ok())
            else {
                continue;
            };
            for peer in peers {
                if cfg.network.site(peer).is_some() {
                    out.insert(peer, mtu);
                }
            }
        }
        out
    }

    /// Interface index → the peers whose tunnel leaves through it, as the
    /// kernel routes the tunnel packets (from our address, with the policy
    /// rules, e.g. of a VPN).
    async fn tunnel_interfaces(&self) -> BTreeMap<u32, Vec<SiteId>> {
        let mut out: BTreeMap<u32, Vec<SiteId>> = BTreeMap::new();
        let Some(cfg) = &self.config else {
            return out;
        };
        let local = cfg.self_site().map(|s| s.address);
        for tunnel in topology::tunnels_for(&cfg.network, cfg.self_id()) {
            match self.sys.output_interface(tunnel.peer.address, local).await {
                Ok(Some(index)) => out.entry(index).or_default().push(tunnel.peer.id),
                Ok(None) => {}
                Err(e) => debug!("{e}"),
            }
        }
        out
    }

    /// The traffic per VLAN on the ports of our bridges.
    /// The traffic of the tunnels and trunk ports and the round trips of
    /// the peers recorded after `since_ms` (all of the last hour without).
    fn history(&self, since_ms: Option<u64>) -> wanderlan_core::api::History {
        use wanderlan_core::api::{History as Answer, PeerHistory, PortHistory};
        let since = since_ms.unwrap_or(0);
        let trunks: Vec<String> = self.config.as_ref().map_or_else(Vec::new, |cfg| {
            let mut ports: Vec<String> = std::iter::once(&cfg.local.trunk)
                .chain(cfg.local.tunnel_trunks.values())
                .flatten()
                .cloned()
                .collect();
            ports.sort();
            ports.dedup();
            ports
        });
        let recorded = self.history.read().ok();
        let traffic = |name: &str| {
            recorded
                .as_ref()
                .map_or_else(Vec::new, |h| h.since(name, since))
        };
        Answer {
            now_ms: history::now_ms(),
            started_ms: self.started_ms,
            interval_ms: history::duration_ms(stats::INTERVAL),
            peers: self.config.as_ref().map_or_else(Vec::new, |cfg| {
                topology::tunnels_for(&cfg.network, cfg.self_id())
                    .iter()
                    .map(|t| PeerHistory {
                        traffic: traffic(&t.ifname(self.tag)),
                        rtt: recorded
                            .as_ref()
                            .map_or_else(Vec::new, |h| h.rtt_since(t.peer.id, since)),
                        name: cfg.network.label(t.peer.id),
                    })
                    .collect()
            }),
            trunks: trunks
                .into_iter()
                .map(|port| PortHistory {
                    traffic: traffic(&port),
                    port,
                })
                .collect(),
        }
    }

    async fn vlan_traffic(&self) -> Result<VlanTraffic, ApiError> {
        let links = self.sys.links().await.map_err(api_err)?;
        let bridges = self.own_bridges(&links);
        let without: Vec<&str> = links
            .iter()
            .filter(|l| bridges.iter().any(|(i, _)| *i == l.index))
            .filter(|l| !matches!(l.kind, LinkKind::Bridge(o) if o.vlan_stats))
            .map(|l| l.name.as_str())
            .collect();
        let note = if !self.sys.vlan_stats_supported() {
            Some("the kernel has no per-VLAN counters per port".to_string())
        } else if bridges.is_empty() {
            Some("there is no bridge yet".to_string())
        } else if !without.is_empty() {
            Some(format!(
                "{} does not count per VLAN yet; it is built anew when the daemon starts",
                without.join(", ")
            ))
        } else {
            None
        };
        let ports_of_ours: BTreeSet<&str> = links
            .iter()
            .filter(|l| {
                l.master
                    .is_some_and(|m| bridges.iter().any(|(i, _)| *i == m))
            })
            .map(|l| l.name.as_str())
            .collect();
        // Asked for: the sampler reads the counters from now on. Until it
        // has, the counters as they are now (rates 0).
        let fresh = match self.vlan_rates.write() {
            Ok(mut v) => {
                v.wanted = Some(std::time::Instant::now());
                v.is_fresh().then(|| (v.rates.clone(), v.at_ms))
            }
            Err(_) => None,
        };
        let (rates, sampled_ms) = match fresh {
            Some(fresh) => fresh,
            None => {
                let counters = self.sys.vlan_stats().await.unwrap_or_default();
                let names: std::collections::HashMap<u32, &str> =
                    links.iter().map(|l| (l.index, l.name.as_str())).collect();
                let rates =
                    stats::counters_only(&counters, |i| names.get(&i).map(|n| (*n).to_string()));
                (rates, None)
            }
        };
        let ports = rates
            .into_iter()
            .filter(|((port, _), _)| ports_of_ours.contains(port.as_str()))
            .map(|((port, vid), r)| VlanPortTraffic {
                peer: self.config.as_ref().and_then(|c| {
                    topology::peer_of(&c.network, self.tag, &port).map(|s| c.network.label(s.id))
                }),
                port,
                vid,
                rx_bps: r.rx_bps,
                tx_bps: r.tx_bps,
                rx_bytes: r.counters.rx_bytes,
                tx_bytes: r.counters.tx_bytes,
                rx_packets: r.counters.rx_packets,
                tx_packets: r.counters.tx_packets,
            })
            .collect();
        Ok(VlanTraffic {
            available: note.is_none(),
            note,
            ports,
            sampled_ms,
        })
    }

    /// The desired state, the fingerprints and the confirmed version, from
    /// the cache while neither the configuration nor the runtime changed.
    fn derived(&self) -> Derived {
        let mut guard = self
            .derived
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(d) = guard
            .as_ref()
            .filter(|d| d.config_id == self.config_id && d.runtime == self.runtime)
        {
            return d.clone();
        }
        let fresh = Derived {
            config_id: self.config_id,
            runtime: self.runtime.clone(),
            desired: self
                .config
                .as_ref()
                .map(|cfg| Arc::new(desired::derive_in(&self.scope(), cfg, &self.runtime))),
            fingerprint: self
                .config
                .as_ref()
                .map(|c| config::fingerprint(&c.network))
                .unwrap_or_default(),
            confirmed: self.confirmed.as_ref().map(|c| Version::of(&c.network)),
        };
        *guard = Some(fresh.clone());
        fresh
    }

    /// MAC addresses per bridge port (learned ones), at most
    /// `FDB_CACHE_TTL` old: the FDB can be large and the status is asked
    /// for every second.
    async fn macs_per_port(&self) -> MacCounts {
        let cached = self
            .fdb_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < FDB_CACHE_TTL)
            .map(|(_, counts)| counts.clone());
        if let Some(counts) = cached {
            return counts;
        }
        let fdb = self.sys.fdb().await.unwrap_or_default();
        self.count_macs(&fdb)
    }

    /// Counts the learned addresses per port and keeps the result.
    fn count_macs(
        &self,
        fdb: &[wanderlan_core::sys::observe::FdbEntry],
    ) -> Arc<BTreeMap<u32, usize>> {
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for e in fdb.iter().filter(|e| !e.permanent) {
            *counts.entry(e.port).or_default() += 1;
        }
        let counts = Arc::new(counts);
        *self
            .fdb_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Instant::now(), counts.clone()));
        counts
    }

    async fn mac_table(&self) -> Result<Vec<MacEntry>, ApiError> {
        let links = self.sys.links().await.map_err(api_err)?;
        let fdb = self.sys.fdb().await.map_err(api_err)?;
        // Fresh anyway: the status gets the counts from it too.
        self.count_macs(&fdb);
        let bridges: BTreeSet<u32> = self
            .own_bridges(&links)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        let names: std::collections::HashMap<u32, &str> =
            links.iter().map(|l| (l.index, l.name.as_str())).collect();
        let name_of = |i: u32| {
            names
                .get(&i)
                .map_or_else(|| format!("#{i}"), |n| (*n).to_string())
        };
        let mut out: Vec<MacEntry> = fdb
            .into_iter()
            .filter(|e| !e.permanent && e.bridge.is_some_and(|b| bridges.contains(&b)))
            .map(|e| {
                let port = name_of(e.port);
                MacEntry {
                    peer: self.config.as_ref().and_then(|c| {
                        topology::peer_of(&c.network, self.tag, &port)
                            .map(|s| c.network.label(s.id))
                    }),
                    mac: format_mac(&e.mac),
                    vlan: e.vlan,
                    port,
                    age_secs: e.updated_secs,
                }
            })
            .collect();
        out.sort_by(|a, b| a.port.cmp(&b.port).then(a.mac.cmp(&b.mac)));
        Ok(out)
    }

    async fn status(&self) -> Status {
        let links = self.sys.links().await.unwrap_or_default();
        let macs_per_port = self.macs_per_port().await;
        let derived = self.derived();
        let probes = self.probes.read().map(|t| t.clone()).unwrap_or_default();
        let rates = self.rates.read().map(|r| r.clone()).unwrap_or_default();
        let confirmed_fp = derived
            .confirmed
            .as_ref()
            .map(|v| v.fingerprint.clone())
            .unwrap_or_default();
        let pending = self.pending.as_ref().map(|p| PendingChange {
            remaining_secs: p
                .deadline
                .saturating_duration_since(Instant::now())
                .as_secs(),
            source: p.source.clone(),
        });
        let blocked: Vec<String> = self.runtime.blocked.iter().cloned().collect();
        let Some(cfg) = &self.config else {
            return Status {
                version: wanderlan_core::version_line().into(),
                state: SiteState::Unconfigured,
                site: None,
                address: None,
                online: false,
                trunk: None,
                vlans: None,
                revision: 0,
                fingerprint: String::new(),
                local_fingerprint: String::new(),
                pending,
                last_error: self.last_error.clone(),
                peers: Vec::new(),
                sites: Vec::new(),
                blocked,
                invites: 0,
                trunk_traffic: None,
                trunks: Vec::new(),
                trunk_ports: Vec::new(),
                missing_ports: Vec::new(),
                sync_problem: None,
                site_id: self.own_id().map(|id| id.to_string()),
                breakout: Vec::new(),
                uplinks: Vec::new(),
                network: self.network.clone(),
                tag: self.tag,
                networks: self.host.networks(),
                conflicts: Vec::new(),
            };
        };
        let desired = derived
            .desired
            .clone()
            .unwrap_or_else(|| Arc::new(desired::derive(cfg, &self.runtime)));
        let me = cfg.self_site();
        let tunnels = topology::tunnels_for(&cfg.network, cfg.self_id());
        let direct: BTreeSet<SiteId> = tunnels.iter().map(|t| t.peer.id).collect();
        let peers = tunnels
            .iter()
            .map(|t| {
                let port = t.ifname(self.tag);
                let link = links.iter().find(|l| l.name == port);
                let self_online = me.is_none_or(|s| s.online);
                let port_state = if self.runtime.blocked.contains(&port) {
                    PortState::Blocked
                } else if !self_online || !t.peer.online {
                    PortState::Offline
                } else {
                    match link {
                        None => PortState::Missing,
                        Some(l) if l.admin_up => PortState::Up,
                        Some(_) => PortState::Down,
                    }
                };
                let macs = link.map_or(0, |l| macs_per_port.get(&l.index).copied().unwrap_or(0));
                let id = t.peer.id;
                let probe = probes.get(&id);
                let mss = desired
                    .mss
                    .iter()
                    .find(|r| r.port == port && r.dir == PortDir::Out && !r.ipv6)
                    .map(|r| r.mss);
                let (rx_bps, tx_bps) = rates.get(&port).copied().unwrap_or_default();
                let announce = probe.and_then(|p| p.announce);
                let vni = desired
                    .tunnels
                    .iter()
                    .find(|d| d.name == port)
                    .map(|d| d.params.vni);
                let vlans = desired
                    .tunnels
                    .iter()
                    .find(|d| d.name == port)
                    .map(|d| {
                        d.vlans
                            .keys()
                            .copied()
                            .collect::<config::VlanSet>()
                            .to_string()
                    })
                    .unwrap_or_default();
                let down_reason = desired
                    .tunnels
                    .iter()
                    .find(|d| d.name == port)
                    .and_then(|d| d.down_reason.clone())
                    .or_else(|| desired.unbuilt.get(&id).cloned());
                PeerStatus {
                    name: cfg.network.label(id),
                    address: t.peer.address.to_string(),
                    port,
                    port_state,
                    vlans,
                    trunk: cfg.trunk_ports_for(id).join("+"),
                    reachable: probe.is_some_and(|p| p.reachable),
                    last_reply_secs: probe
                        .and_then(|p| p.last_reply)
                        .and_then(|r| r.elapsed().ok())
                        .map(|d| d.as_secs()),
                    // The last answer's, only while it answers.
                    rtt_ms: probe.filter(|p| p.reachable).and_then(|p| p.rtt_ms),
                    rtt_stats: probe.and_then(probe::PeerProbe::rtt_stats),
                    traffic: link.map(|l| l.traffic).unwrap_or_default(),
                    pmtu: probe.and_then(|p| p.pmtu),
                    mss,
                    macs,
                    rx_bps,
                    tx_bps,
                    revision: announce.map(|a| a.revision),
                    in_sync: announce.map(|a| {
                        let theirs = fingerprint_hex(&a.fingerprint);
                        if t.kind == TunnelKind::Client {
                            self.expected
                                .get(&id)
                                .is_some_and(|v| v.fingerprint == theirs)
                        } else {
                            theirs == confirmed_fp
                        }
                    }),
                    refused: self.refused.get(&id).map(|r| r.reason.clone()),
                    identity: t.peer.own_id().map(|id| id.fingerprint()),
                    identity_state: self.peer_keys.contains_key(&id).then(|| {
                        self.runtime
                            .identity
                            .get(&id)
                            .copied()
                            .unwrap_or_default()
                            .into()
                    }),
                    down_reason,
                    tunnel: self.runtime.mode(t),
                    tunnel_reason: tunnel_reason(
                        t,
                        &self.runtime,
                        probe.and_then(|p| p.direct_test),
                        // A client is tested at its port, this site at its own.
                        if t.kind == TunnelKind::Client {
                            self.runtime.tunnel_port_of(&cfg.network, id)
                        } else {
                            desired.tunnel_port
                        },
                    ),
                    vni,
                    endpoint: self.runtime.endpoints.get(&id).map(ToString::to_string),
                }
            })
            .collect();
        let sites = cfg
            .network
            .sites
            .iter()
            .map(|s| SiteSummary {
                name: cfg.network.label(s.id),
                address: s.address.to_string(),
                vlans: {
                    // Own VLANs (towards its own clients only) with a "+".
                    let own = cfg.network.own_vlans(s);
                    let vlans = cfg.network.effective_vlans(s);
                    if own.is_empty() {
                        vlans.to_string()
                    } else {
                        format!("{vlans} +{own}")
                    }
                },
                online: s.online,
                is_self: s.id == cfg.self_id(),
                direct: direct.contains(&s.id),
                uplink: s.uplink.map(|up| cfg.network.label(up)),
                id: s.own_id().map(|id| id.fingerprint()),
                via: topology::via(&cfg.network, cfg.self_id(), s.id).map(|v| cfg.network.label(v)),
            })
            .collect();
        let trunks: Vec<PortTraffic> = desired
            .trunks
            .iter()
            .filter_map(|t| links.iter().find(|l| l.name == t.name))
            .map(|l| {
                let (rx_bps, tx_bps) = rates.get(&l.name).copied().unwrap_or_default();
                PortTraffic {
                    name: l.name.clone(),
                    rx_bps,
                    tx_bps,
                    traffic: l.traffic,
                }
            })
            .collect();
        let family = |r: &Result<Egress, String>| match r {
            Ok(egress) => BreakoutFamily {
                dev: Some(egress.dev().to_string()),
                gateway: match egress {
                    Egress::Fixed {
                        gateway: Some(g), ..
                    } => Some(g.to_string()),
                    Egress::Fixed { gateway: None, .. } => Some("direct".into()),
                    Egress::Main { .. } => None,
                },
                reason: None,
            },
            Err(why) => BreakoutFamily {
                reason: Some(why.clone()),
                ..BreakoutFamily::default()
            },
        };
        let breakout = desired
            .breakout
            .iter()
            .map(|b| {
                let router = links.iter().find(|l| l.name == b.router);
                let port = links.iter().find(|l| l.name == b.port);
                let (rx_bps, tx_bps) = rates.get(&b.router).copied().unwrap_or_default();
                BreakoutStatus {
                    vid: b.vid,
                    bridge: b.bridge.clone(),
                    router: b.router.clone(),
                    uplink: me
                        .and_then(|s| s.breakout.get(&b.vid))
                        .and_then(|c| c.uplink.clone()),
                    except: b.except.iter().map(ToString::to_string).collect(),
                    ipv4: family(&b.v4),
                    ipv6: family(&b.v6),
                    ready: router.is_some_and(|l| l.admin_up)
                        && port.is_some_and(|l| l.admin_up && l.master.is_some()),
                    blocked: (!self.forward_drops.is_empty()).then(|| {
                        format!(
                            "{} drops forwarded packets (its policy): it must accept those of {} (e.g. Docker: DOCKER-USER, ufw: route allow)",
                            self.forward_drops.join(", "),
                            b.router
                        )
                    }),
                    rx_bps,
                    tx_bps,
                    traffic: router.map(|l| l.traffic).unwrap_or_default(),
                }
            })
            .collect();
        let trunk_names: BTreeSet<&str> = desired.trunks.iter().map(|t| t.name.as_str()).collect();
        // The trunk ports of the other networks of the box are none either.
        let box_trunks = self.host.trunk_ports();
        let uplinks = &self.runtime.uplinks;
        let uplinks: Vec<UplinkStatus> = uplinks
            .by_name
            .iter()
            .filter(|(name, _)| {
                name.as_str() != "lo"
                    && !trunk_names.contains(name.as_str())
                    && !box_trunks.contains(name.as_str())
                    && !links
                        .iter()
                        .any(|l| l.name == **name && reconcile::is_owned(l))
            })
            .map(|(name, u)| UplinkStatus {
                name: name.clone(),
                up: u.up,
                addresses: u
                    .v4
                    .iter()
                    .map(ToString::to_string)
                    .chain(u.v6.iter().map(ToString::to_string))
                    .collect(),
                gateway4: u
                    .default4
                    .map(|g| g.map_or_else(|| "direct".to_string(), |g| g.to_string())),
                gateway6: u
                    .default6
                    .map(|g| g.map_or_else(|| "direct".to_string(), |g| g.to_string())),
                default4: uplinks.auto4.as_deref() == Some(name.as_str()),
                default6: uplinks.auto6.as_deref() == Some(name.as_str()),
                pointopoint: u.pointopoint,
            })
            .collect();
        Status {
            version: wanderlan_core::version_line().into(),
            state: if self.last_error.is_some() {
                SiteState::Error
            } else {
                SiteState::Running
            },
            site: Some(cfg.self_name().to_string()),
            address: me.map(|s| s.address.to_string()),
            online: me.is_none_or(|s| s.online),
            trunk: Some(cfg.trunk().to_string()),
            vlans: me.map(|s| cfg.network.effective_vlans(s).to_string()),
            revision: cfg.network.revision,
            fingerprint: derived.fingerprint.clone(),
            local_fingerprint: config::local_fingerprint(&cfg.local),
            pending,
            last_error: self.last_error.clone(),
            peers,
            sites,
            blocked,
            invites: self.invites.list().len(),
            trunk_traffic: trunks.first().cloned(),
            trunks,
            sync_problem: self.sync_problem.clone(),
            trunk_ports: cfg.local.trunk.clone(),
            missing_ports: desired
                .trunks
                .iter()
                .filter(|t| !links.iter().any(|l| l.name == t.name))
                .map(|t| t.name.clone())
                .collect(),
            site_id: self.own_id().map(|id| id.to_string()),
            breakout,
            uplinks,
            network: self.network.clone(),
            tag: self.tag,
            networks: self.host.networks(),
            conflicts: self.conflicts.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_ids_keeps_the_identities() {
        let text = "[network]\nvlans = \"10\"\n\n[site.a]\naddress = \"10.0.0.1\"\nid = \"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=\"\n\n[site.b]\naddress = \"10.0.0.2\"\nid = \"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=\"\n";
        let old = config::parse_network(text).unwrap().network.unwrap();
        // Written by hand without the id lines, b renamed to c.
        let hand = "[network]\nvlans = \"10\"\n\n[site.a]\naddress = \"10.0.0.1\"\n\n[site.c]\naddress = \"10.0.0.2\"\n\n[site.d]\naddress = \"10.0.0.4\"\n";
        let mut new = config::parse_network(hand).unwrap().network.unwrap();
        let mut local = config::Local {
            self_id: Some(new.sites[1].id),
            ..config::Local::default()
        };
        keep_ids(&old, &mut new, &mut local);
        assert_eq!(local.self_id, Some(old.sites[1].id), "self follows");
        assert_eq!(new.sites[0].id, old.sites[0].id);
        assert_eq!(new.sites[1].id, old.sites[1].id, "renamed: by its address");
        assert!(!new.sites[2].has_identity(), "a new site has none");
    }

    #[test]
    fn a_refused_view_carries_its_reason_in_the_first_line() {
        let view = format!("{REFUSED_MARKER}site \"x\" is already used\n[network]\nrevision = 4\n");
        assert_eq!(refusal(&view), Some("site \"x\" is already used"));
        assert_eq!(refusal("[network]\nrevision = 4\n"), None);
        // Only the first line counts: a comment further down is no refusal.
        assert_eq!(refusal(&format!("[network]\n{REFUSED_MARKER}no\n")), None);
        // It is a TOML comment: the view still parses.
        assert!(wanderlan_core::config::parse_network(&view).is_ok());
    }

    #[test]
    fn a_rejected_configuration_gets_another_chance_later() {
        let start = Instant::now();
        let mut rejected = Rejected::default();
        rejected.insert("aa".into(), start);
        assert!(rejected.active(start).contains("aa"));
        assert!(rejected.active(start + REJECT_TTL / 2).contains("aa"));
        assert!(rejected.active(start + REJECT_TTL).is_empty());
        // Expired entries are dropped when the next one is added.
        rejected.insert("bb".into(), start + REJECT_TTL * 2);
        assert_eq!(rejected.0.len(), 1);
    }
}
