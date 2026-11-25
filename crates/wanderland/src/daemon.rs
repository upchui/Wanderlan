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
    ApiError, Applied, ChangeSource, ConfigInput, ConfigTexts, Confirmed, Done, Interface,
    Invitation, JoinParams, Joined, Level, MacEntry, PeerStatus, PendingChange, PortState,
    PortTraffic, Request, ResetReport, SiteState, SiteSummary, Status, Validation, VlanPortTraffic,
    VlanTraffic,
};
use wanderlan_core::config::{
    self, Config, ConfigFile, Diagnostic, Loaded, Network, render_local, render_network,
    unified_diff,
};
use wanderlan_core::desired::{self, OWNER_ALIAS, Runtime};
use wanderlan_core::join::{self, JoinReply, JoinRequest};
use wanderlan_core::loopguard::format_mac;
use wanderlan_core::mtu::wire::Announce;
use wanderlan_core::policy;
use wanderlan_core::reconcile::{self, Change};
use wanderlan_core::sync::{self, Version};
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::sys::nft::NftOps as _;
use wanderlan_core::sys::nft::PortDir;
use wanderlan_core::sys::observe::ObserveOps as _;
use wanderlan_core::sys::{LinkKind, NetOps as _};
use wanderlan_core::topology::{self, ForwardingKey, GRETAP_PREFIX, TunnelKind};
use wanderlan_core::view;

use crate::Args;
use crate::api_server;
use crate::events::EventLog;
use crate::fdb_guard::{self, LoopReport, PortMap};
use crate::files::Paths;
use crate::invites::{Invite, Invites};
use crate::probe::{self, ProbeConfig, ProbeEvent, ProbeTable};
use crate::stats::{self, Rates, VlanRates};
use crate::sync_net::{self, Offer};

/// Delays of the extra flushes after a topology change. Sites apply a new
/// configuration a few seconds apart; until all have switched, a bridge can
/// learn addresses behind ports that are wrong in the new topology.
const FOLLOW_UP_FLUSHES: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];
/// How long a removed peer may still fetch the configuration from us.
const GRACE: Duration = Duration::from_secs(300);
/// Wait time after a failed fetch before trying the same peer again.
const FETCH_BACKOFF: Duration = Duration::from_secs(30);
/// How long a rejected configuration is not fetched again.
const REJECT_TTL: Duration = Duration::from_secs(600);
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
/// The longest one reconcile may take.
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest network revision (network.toml holds an i64).
const MAX_REVISION: u64 = i64::MAX as u64;

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
        peer: String,
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

/// How long a joining site retries while the inviting site is busy.
const JOIN_BUSY_RETRY: Duration = Duration::from_secs(60);
/// Wrong invitation codes allowed per address within [`JOIN_FAILURE_WINDOW`].
const JOIN_MAX_FAILURES: usize = 5;
/// Window for counting wrong invitation codes.
const JOIN_FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// A change waiting for confirmation.
#[derive(Debug)]
struct Pending {
    previous: Option<Config>,
    deadline: Instant,
    /// When it was applied (monotonic: compared with the probe replies).
    applied_at: std::time::Instant,
    source: ChangeSource,
    from_files: bool,
    fingerprint: String,
    /// For changes of a client merged here: the fingerprint the client
    /// announced, rejected too on a rollback (else fetched again at once).
    announced: Option<String>,
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

/// State of the daemon.
struct Controller {
    sys: LinuxSystem,
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
    inputs: mpsc::Sender<Input>,
    /// Fingerprints not to fetch again, until when.
    rejected: Rejected,
    /// Clients whose last change was refused here, with the reason.
    refused: BTreeMap<String, String>,
    /// Since when the uplink has not taken over a change of this site.
    diverged_since: Option<Instant>,
    /// Why the last change of this site was not taken over by its uplink.
    sync_problem: Option<String>,
    /// The version each client of this site should have (its view).
    expected: BTreeMap<String, Version>,
    /// The FDB as read last, for the status.
    /// The MAC addresses per port as counted last, for the status.
    fdb_cache: std::sync::Mutex<Option<(Instant, MacCounts)>>,
    /// Counts the changes of the configuration in effect (cache key).
    config_id: u64,
    /// What the status needs from the configuration, until it changes.
    derived: std::sync::Mutex<Option<Derived>>,
    fetching: bool,
    fetch_backoff: BTreeMap<String, Instant>,
    grace: BTreeMap<String, (Ipv4Addr, Instant)>,
    generation: u64,
    invites: Invites,
    join_failures: BTreeMap<Ipv4Addr, VecDeque<Instant>>,
    joining: bool,
    /// The last error of a step retried every second, logged once.
    last_repeated: Option<String>,
}

/// Runs the daemon until SIGTERM or SIGINT.
pub async fn run(args: Args) -> Result<()> {
    info!("wanderland {} starting", wanderlan_core::version_line());
    let sys = LinuxSystem::connect().context("cannot open a netlink connection")?;
    let mut sigterm = signal(SignalKind::terminate()).context("cannot handle SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot handle SIGINT")?;
    let mut sighup = signal(SignalKind::hangup()).context("cannot handle SIGHUP")?;

    let events = EventLog::new();
    let (inputs_tx, mut inputs) = mpsc::channel::<Input>(256);
    let (probe_tx, probe_rx) = watch::channel(ProbeConfig::default());
    let (probe_event_tx, mut probe_events) = mpsc::channel(64);
    let probes: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
    let (offer_tx, offer_rx) = watch::channel(Offer::default());
    let (ports_tx, ports_rx) = watch::channel(PortMap::default());
    let rates: Rates = Arc::new(RwLock::new(BTreeMap::new()));
    let vlan_rates: VlanRates = Arc::new(RwLock::new(stats::VlanTraffic::default()));

    let mut tasks = vec![
        (
            "probes",
            probe::spawn(probe_rx, probes.clone(), probe_event_tx),
        ),
        ("sync", sync_net::spawn(offer_rx, inputs_tx.clone())),
        (
            "statistics",
            stats::spawn(sys.clone(), rates.clone(), vlan_rates.clone()),
        ),
        (
            "API",
            api_server::spawn(args.socket.clone(), events.clone(), inputs_tx.clone())
                .context("cannot start the API")?,
        ),
    ];
    tasks.push((
        "loop guard",
        fdb_guard::spawn(&sys, ports_rx, inputs_tx.clone()),
    ));
    // Without the netlink connection nothing works any more: its end ends
    // the daemon, and systemd starts it again.
    if let Some(connection) = sys.net.take_connection() {
        tasks.push((NETLINK_TASK, connection));
    }

    let paths = Paths {
        config_dir: args.config_dir.clone(),
        state_dir: args.state_dir.clone(),
    };
    let invites = Invites::load(paths.invites());
    let mut ctl = Controller {
        sys,
        paths,
        events,
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
        rates,
        vlan_rates,
        inputs: inputs_tx,
        rejected: Rejected::default(),
        refused: BTreeMap::new(),
        diverged_since: None,
        sync_problem: None,
        expected: BTreeMap::new(),
        fdb_cache: std::sync::Mutex::new(None),
        config_id: 0,
        derived: std::sync::Mutex::new(None),
        fetching: false,
        fetch_backoff: BTreeMap::new(),
        grace: BTreeMap::new(),
        generation: 0,
        invites,
        join_failures: BTreeMap::new(),
        joining: false,
        last_repeated: None,
    };
    ctl.startup();
    ctl.seed_measurements().await;
    if let Some(warning) = bridge_netfilter_warning() {
        warn!("{warning}");
        ctl.events.warn(warning);
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(args.reconcile_interval.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut second = tokio::time::interval(Duration::from_secs(1));
    second.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let next_flush = ctl.flushes.first().copied();
        let deadline = ctl.pending.as_ref().map(|p| p.deadline);
        tokio::select! {
            _ = ticker.tick() => ctl.reconcile().await,
            _ = second.tick() => ctl.every_second().await,
            Some(input) = inputs.recv() => ctl.handle(input).await,
            Some(event) = probe_events.recv() => ctl.handle(Input::Probe(event)).await,
            () = sleep_until_opt(deadline) => {
                if let Err(e) = ctl.rollback("the change was not confirmed in time").await {
                    ctl.events.error(e.message);
                }
            }
            () = sleep_until_opt(next_flush) => {
                if let Some(at) = next_flush {
                    ctl.flushes.remove(&at);
                }
                ctl.flush_bridges().await;
            }
            _ = sighup.recv() => {
                info!("SIGHUP: reloading the configuration files");
                ctl.reload_files().await;
            }
            (name, why) = first_finished(&mut tasks) => {
                // Clean up as far as possible (systemd's ExecStopPost does
                // the rest), then exit: systemd starts the daemon again,
                // which builds everything anew.
                for (task_name, task) in &tasks {
                    if *task_name != NETLINK_TASK {
                        task.abort();
                    }
                }
                if let Err(e) = teardown_for_exit(&ctl.sys, STOP_TEARDOWN).await {
                    warn!("{e:#}");
                }
                for (_, task) in &tasks {
                    task.abort();
                }
                anyhow::bail!(
                    "internal error: the {name} service stopped ({why}); exiting so that systemd restarts wanderland"
                );
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    // The netlink connection stays until everything is removed.
    let mut connection = None;
    for (name, task) in tasks {
        if name == NETLINK_TASK {
            connection = Some(task);
        } else {
            task.abort();
        }
    }
    info!("Stopping; removing everything wanderlan created");
    let removed = teardown_for_exit(&ctl.sys, STOP_TEARDOWN).await?;
    if let Some(task) = connection {
        task.abort();
    }
    let _ = std::fs::remove_file(&args.socket);
    info!("Stopped ({} changes undone)", removed.len());
    Ok(())
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
                LinkKind::Gretap(_) => "gretap".into(),
                LinkKind::Other(k) => k.clone(),
            },
            up: l.admin_up,
            master: l.master.and_then(name_of),
            mtu: l.mtu,
            managed: l.alias.as_deref() == Some(OWNER_ALIAS),
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
/// after every end of the service (also a crash or a kill) and when the
/// package is removed, so nothing stays behind. Does nothing if nothing is
/// there.
pub async fn cleanup(args: &Args) -> Result<()> {
    let sys = LinuxSystem::connect().context("cannot open a netlink connection")?;
    let removed = tokio::time::timeout(
        CLEANUP_TIMEOUT,
        reconcile::teardown(&sys, |c| info!("Cleanup: {c}")),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "the cleanup did not finish within {} s",
            CLEANUP_TIMEOUT.as_secs()
        )
    })?
    .context("cleanup failed")?;
    let _ = std::fs::remove_file(&args.socket);
    if removed.is_empty() {
        debug!("Cleanup: nothing of wanderlan left");
    } else {
        info!("Cleanup: {} changes undone", removed.len());
    }
    // gre0, gretap0 and erspan0 of the kernel module ip_gre.
    let links = sys.links().await.unwrap_or_default();
    if let Some(message) =
        wanderlan_core::kernel::remove_gre_fallback(&interface_list(&links, &BTreeMap::new()))
            .message()
    {
        info!("Cleanup: {message}");
    }
    Ok(())
}

/// Removes everything at an end of the daemon, at most `limit` long.
async fn teardown_for_exit(sys: &LinuxSystem, limit: Duration) -> Result<Vec<Change>> {
    tokio::time::timeout(limit, reconcile::teardown(sys, |c| info!("{c}")))
        .await
        .map_err(|_| anyhow::anyhow!("did not finish within {} s", limit.as_secs()))?
        .context("cleanup failed")
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
            l.alias.as_deref() == Some(OWNER_ALIAS) && matches!(l.kind, LinkKind::Bridge(_))
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
        let cfg = self.load_files();
        if cfg.is_some()
            && let Err(e) = self.paths.save_copy()
        {
            warn!("{e:#}");
        }
        self.confirmed = cfg.clone();
        self.set_config(cfg);
    }

    /// Takes the MSS rules left by a previous run as preliminary
    /// measurements, so that a restart does not remove and re-add them.
    async fn seed_measurements(&mut self) {
        let Some(cfg) = &self.config else { return };
        if cfg.network.mss.is_some() {
            return;
        }
        let Ok(Some(rules)) = self.sys.nft_state().await.map(|s| s.map(|s| s.mss)) else {
            return;
        };
        let peers = self.direct_peers();
        for rule in rules.iter().filter(|r| r.dir == PortDir::Out && !r.ipv6) {
            if let Some(peer) = rule.port.strip_prefix(GRETAP_PREFIX)
                && peers.contains_key(peer)
            {
                self.runtime.pmtu.insert(
                    peer.to_string(),
                    rule.mss.saturating_add(wanderlan_core::mtu::MSS_OVERHEAD),
                );
            }
        }
    }

    /// Loads and validates the files; logs problems.
    fn load_files(&self) -> Option<Config> {
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
        match load_texts(&network, &local) {
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

    /// SIGHUP: the files were changed by hand. Without a pending change the
    /// files are taken as confirmed (like a restart would).
    async fn reload_files(&mut self) {
        if self.pending.is_some() {
            self.events
                .warn("Ignoring SIGHUP while a change is waiting for confirmation");
            return;
        }
        if let Some(cfg) = self.load_files() {
            let _ = self.paths.save_copy();
            self.confirmed = Some(cfg.clone());
            self.set_config(Some(cfg));
            self.last_error = None;
            self.reconcile().await;
        }
    }

    /// Switches to a new configuration in effect and updates the background
    /// services. The announced version is always the confirmed one.
    fn set_config(&mut self, cfg: Option<Config>) {
        let old_peers = self.direct_peers();
        self.config = cfg;
        self.config_id += 1;
        let new_peers = self.direct_peers();
        let now = Instant::now();
        for (name, addr) in old_peers {
            if !new_peers.contains_key(&name) {
                self.grace.insert(name, (addr, now + GRACE));
            }
        }
        for name in new_peers.keys() {
            self.grace.remove(name);
        }
        self.grace.retain(|_, (_, until)| *until > now);
        self.runtime
            .pmtu
            .retain(|peer, _| new_peers.contains_key(peer));
        // A port shut by the loop guard stays shut while it exists: tunnels
        // of peers that are gone and trunk ports no longer used are freed.
        let trunk_ports: BTreeSet<&String> =
            self.config.as_ref().map_or_else(BTreeSet::new, |cfg| {
                std::iter::once(&cfg.local.trunk)
                    .chain(cfg.local.tunnel_trunks.values())
                    .flatten()
                    .collect()
            });
        self.runtime.blocked.retain(|port| {
            new_peers.keys().any(|p| topology::gretap_name(p) == *port)
                || trunk_ports.contains(port)
        });
        self.publish();
    }

    /// Direct peers of the configuration in effect.
    fn direct_peers(&self) -> BTreeMap<String, Ipv4Addr> {
        self.config.as_ref().map_or_else(BTreeMap::new, |cfg| {
            topology::tunnels_for(&cfg.network, cfg.self_name())
                .into_iter()
                .map(|t| (t.peer.name.clone(), t.peer.address))
                .collect()
        })
    }

    /// Updates the probe service and the sync server.
    fn publish(&mut self) {
        let peers = self.direct_peers();
        let responders: BTreeMap<String, Ipv4Addr> = self
            .grace
            .iter()
            .map(|(n, (a, _))| (n.clone(), *a))
            .collect();
        let bind = self.config.as_ref().and_then(|cfg| {
            cfg.self_site()
                .map(|s| SocketAddrV4::new(s.address, cfg.network.control_port))
        });
        let version = |m: &Network| Announce {
            revision: m.revision,
            fingerprint: fingerprint_bytes(&config::fingerprint(m)),
        };
        // Servers and the uplink get what this site knows; a client of this
        // site (and a removed site) only its own view of the network.
        let mut announce_to = BTreeMap::new();
        let mut views = BTreeMap::new();
        let mut expected = BTreeMap::new();
        let announce = match &self.confirmed {
            None => Announce::default(),
            Some(c) => {
                let own = render_network(&c.network);
                for (name, addr) in peers.iter().chain(responders.iter()) {
                    let client = c
                        .network
                        .site(name)
                        .is_none_or(|s| s.uplink.as_deref() == Some(c.self_name()));
                    if client {
                        let v = view::view_for(&c.network, name);
                        announce_to.insert(name.clone(), version(&v));
                        expected.insert(name.clone(), Version::of(&v));
                        // A refused change: the client learns why.
                        let text = match self.refused.get(name) {
                            Some(reason) => format!(
                                "{REFUSED_MARKER}{}\n{}",
                                reason.replace('\n', " "),
                                render_network(&v)
                            ),
                            None => render_network(&v),
                        };
                        views.insert(*addr, text);
                    } else {
                        views.insert(*addr, own.clone());
                    }
                }
                version(&c.network)
            }
        };
        self.expected = expected;
        let mtu_interval = self.config.as_ref().and_then(|c| {
            c.network
                .mss
                .is_none()
                .then_some(c.network.mtu_probe_interval)
        });
        self.probe_config.send_replace(ProbeConfig {
            bind,
            peers,
            responders,
            generation: self.generation,
            mtu_interval,
            announce,
            announce_to,
        });
        self.offer.send_replace(Offer {
            bind,
            views: Arc::new(views),
        });
    }

    async fn handle(&mut self, input: Input) {
        match input {
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
            Input::Probe(ProbeEvent::Announce { peer, announce }) => {
                debug!(
                    "Peer \"{peer}\" announces network revision {}",
                    announce.revision
                );
                self.maybe_sync();
            }
            Input::Fetched { peer, mode, result } => self.fetched(peer, mode, result).await,
            Input::Loop(report) => {
                if self.runtime.blocked.insert(report.port.clone()) {
                    let peer = report.port.strip_prefix(GRETAP_PREFIX).map(str::to_string);
                    self.events
                        .emit(Level::Warn, peer.as_deref(), report.message);
                    self.reconcile().await;
                }
            }
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

    /// Housekeeping: automatic confirmation of synced changes, sync retries.
    async fn every_second(&mut self) {
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
                    .map(|(name, _)| name.clone())
            });
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
        {
            let since = p.applied_at;
            let site = site.clone();
            let up = self.probes.read().ok().is_some_and(|t| {
                t.get(&site)
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
        let me = confirmed.self_name().to_string();
        let ours = match self.derived().confirmed {
            Some(v) => v,
            None => Version::of(&confirmed.network),
        };
        let port = confirmed.network.control_port;
        let tunnels: Vec<(String, TunnelKind)> = topology::tunnels_for(&confirmed.network, &me)
            .into_iter()
            .map(|t| (t.peer.name.clone(), t.kind))
            .collect();
        let now = Instant::now();
        let table = self.probes.read().map(|t| t.clone()).unwrap_or_default();
        let rejected = self.rejected.active(now);
        let mut upstream: Vec<(String, Version)> = Vec::new();
        let mut client_changes: Option<(String, Version)> = None;
        let mut adopt: Option<(String, u64)> = None;
        let mut resync: Option<String> = None;
        let mut in_sync_again: Vec<String> = Vec::new();
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
            // A revision that no network.toml can hold is not adopted.
            if announce.revision > MAX_REVISION {
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
                        adopt = Some((name.clone(), theirs.revision));
                    }
                    if *kind == TunnelKind::Uplink {
                        self.diverged_since = None;
                    }
                }
                _ if rejected.contains(&theirs.fingerprint) => {}
                TunnelKind::Server => upstream.push((name.clone(), theirs)),
                // The uplink has the last word on what this site knows.
                TunnelKind::Uplink if theirs.revision > ours.revision => {
                    self.diverged_since = None;
                    upstream.push((name.clone(), theirs));
                }
                TunnelKind::Uplink => {
                    // A change of this site that the uplink has not taken
                    // over (yet): after a while, ask whether it refused it.
                    let since = *self.diverged_since.get_or_insert(now);
                    if probe.reachable && now.duration_since(since) >= RESYNC_AFTER {
                        resync = Some(name.clone());
                    }
                }
                TunnelKind::Client => {
                    let Some(expected) = self.expected.get(name) else {
                        continue;
                    };
                    if theirs.fingerprint == expected.fingerprint {
                        in_sync_again.push(name.clone());
                    } else if client_changes.is_none() && theirs.revision >= expected.revision {
                        client_changes = Some((name.clone(), theirs));
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
            self.adopt_revision(&peer, revision);
            return;
        }
        // Newer versions from servers or the uplink first, then changes of
        // a client, then the question whether the uplink refused ours.
        let chosen = sync::choose(
            &ours,
            upstream.iter().map(|(n, v)| (n.as_str(), v.clone())),
            &rejected,
        )
        .map(|(n, v)| (n.to_string(), Some(v), FetchMode::Replace))
        .or_else(|| client_changes.map(|(n, v)| (n, Some(v), FetchMode::Merge)))
        .or_else(|| resync.map(|n| (n, None, FetchMode::Resync)));
        let Some((peer, version, mode)) = chosen else {
            return;
        };
        let peers = self.direct_peers();
        let Some(addr) = peers.get(&peer) else { return };
        let target = SocketAddrV4::new(*addr, port);
        match (&mode, version) {
            (FetchMode::Merge, Some(v)) => self.events.emit(
                Level::Info,
                Some(&peer),
                format!(
                    "Client \"{peer}\" has changes (revision {}, here: {}); fetching them",
                    v.revision, ours.revision
                ),
            ),
            (FetchMode::Resync, _) => {
                // Asked again only after another wait.
                self.diverged_since = Some(now);
                debug!("Asking \"{peer}\" whether it took over the change of this site");
            }
            (_, Some(v)) => self.events.emit(
                Level::Info,
                Some(&peer),
                format!(
                    "Peer \"{peer}\" has network revision {} (here: {}); fetching it",
                    v.revision, ours.revision
                ),
            ),
            (_, None) => {}
        }
        self.fetching = true;
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let result = sync_net::fetch(target).await.map_err(|e| format!("{e:#}"));
            let _ = inputs.send(Input::Fetched { peer, mode, result }).await;
        });
    }

    /// Remembers a fingerprint that is not fetched again for a while.
    fn reject(&mut self, fingerprint: String) {
        self.rejected.insert(fingerprint, Instant::now());
    }

    /// Takes over a newer revision number with the same content, without a
    /// fetch, an apply or a countdown.
    fn adopt_revision(&mut self, peer: &str, revision: u64) {
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
        debug!("Peer \"{peer}\" has revision {revision} with the same content; taken over");
        self.confirmed = Some(cfg.clone());
        self.set_config(Some(cfg));
    }

    async fn fetched(&mut self, peer: String, mode: FetchMode, result: Result<String, String>) {
        self.fetching = false;
        let text = match result {
            Ok(t) => t,
            Err(e) => {
                self.fetch_backoff
                    .insert(peer.clone(), Instant::now() + FETCH_BACKOFF);
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!("Cannot fetch the configuration from \"{peer}\": {e}"),
                );
                return;
            }
        };
        let Some(local) = self.confirmed.as_ref().map(|c| c.local.clone()) else {
            return;
        };
        let parsed = match config::parse_network(&text) {
            Ok(p) => p.network,
            Err(diags) => {
                let msg = diags.first().map_or_else(String::new, ToString::to_string);
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!("The configuration from \"{peer}\" cannot be read: {msg}"),
                );
                self.fetch_backoff
                    .insert(peer, Instant::now() + FETCH_BACKOFF);
                return;
            }
        };
        let Some(network) = parsed else {
            // Nothing in it (e.g. only comments): not again every second.
            self.fetch_backoff
                .insert(peer, Instant::now() + FETCH_BACKOFF);
            return;
        };
        let fingerprint = config::fingerprint(&network);
        let network = match mode {
            FetchMode::Merge => {
                let Some(own) = self.confirmed.as_ref().map(|c| c.network.clone()) else {
                    return;
                };
                match view::merge(&own, &network, &peer) {
                    Ok(merged) => {
                        self.refused.remove(&peer);
                        merged
                    }
                    Err(e) => {
                        self.reject(fingerprint);
                        self.events.emit(
                            Level::Warn,
                            Some(&peer),
                            format!("Not taking over the changes of client \"{peer}\": {e}"),
                        );
                        // The client learns why with its next fetch.
                        self.refused.insert(peer, e);
                        self.publish();
                        return;
                    }
                }
            }
            FetchMode::Resync => {
                let Some(reason) = refusal(&text) else {
                    // Not refused: the uplink may still take it over.
                    return;
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
                network
            }
            FetchMode::Replace => network,
        };
        match sync::accept(network, &local) {
            Err(e) => {
                self.reject(fingerprint);
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!("Not using the configuration from \"{peer}\": {e}"),
                );
            }
            Ok(cfg) => {
                let source = ChangeSource::Sync { peer: peer.clone() };
                let result = self.apply(cfg, source, None, None, false).await;
                if result.is_ok()
                    && mode == FetchMode::Merge
                    && let Some(p) = &mut self.pending
                {
                    p.announced = Some(fingerprint.clone());
                }
                if let Err(e) = result {
                    // Possibly transient (e.g. a busy kernel); try again later.
                    self.fetch_backoff
                        .insert(peer.clone(), Instant::now() + FETCH_BACKOFF);
                    self.events.emit(
                        Level::Warn,
                        Some(&peer),
                        format!(
                            "Cannot apply the configuration from \"{peer}\": {}",
                            e.message
                        ),
                    );
                }
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
                    .apply(cfg, ChangeSource::Local, timeout, None, true)
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
                join::check_rules(&confirmed.network, confirmed.self_name(), &rules)
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
        }
    }

    /// An invitation as shown to the user.
    fn invitation(&self, invite: &Invite) -> Invitation {
        let command = self
            .confirmed
            .as_ref()
            .and_then(|c| c.self_site().map(|s| (s.address, c.network.control_port)))
            .map(|(address, port)| {
                if port == config::DEFAULT_CONTROL_PORT {
                    format!("wanderlan join {address} {}", invite.code)
                } else {
                    format!("wanderlan join {address}:{port} {}", invite.code)
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
        let admission = match join::add_site(
            &confirmed.network,
            confirmed.self_name(),
            &request.name,
            request.name_is_default,
            from,
            &invite.rules,
        ) {
            Ok(a) => a,
            Err(e) => {
                self.events
                    .warn(format!("The site at {from} cannot join: {e}"));
                return error(e.to_string());
            }
        };
        let via = confirmed.self_name().to_string();
        if !admission.changed {
            if let Err(e) = self.invites.remove(&invite.code) {
                warn!("{e:#}");
            }
            self.events.emit(
                Level::Info,
                Some(&admission.name),
                format!(
                    "Site \"{}\" at {from} joined again with invitation {}",
                    admission.name, invite.code
                ),
            );
            let network = render_network(&view::view_for(&confirmed.network, &admission.name));
            return JoinReply::Ok {
                site: admission.name,
                via,
                network,
            };
        }
        // The new site gets its view of the network only.
        let network = render_network(&view::view_for(&admission.network, &admission.name));
        let site = admission.name;
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
        match self.apply(cfg, source, None, None, false).await {
            Ok(_) => {
                self.invites.reserve(&invite.code);
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
        };
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let give_up = Instant::now() + JOIN_BUSY_RETRY;
            let result = loop {
                match sync_net::join(target, &request).await {
                    Ok(JoinReply::Busy) if Instant::now() < give_up => {
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
            JoinReply::Error { message } => {
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
        let local = config::Local {
            self_name: Some(site.clone()),
            trunk: vec![params.trunk.clone()],
            ..Default::default()
        };
        let cfg = sync::accept(network, &local).map_err(|e| {
            ApiError::new(format!(
                "the configuration from \"{via}\" cannot be used: {e}"
            ))
        })?;
        let revision = cfg.network.revision;
        let sites = cfg.network.sites.len();
        let source = ChangeSource::Sync { peer: via.clone() };
        self.apply(cfg, source, None, None, false).await?;
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
        self.runtime = Runtime::default();
        self.applied_key = None;
        self.last_error = None;
        self.rejected.clear();
        self.fetch_backoff.clear();
        self.grace.clear();
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
        let removed = match reconcile::teardown(&self.sys, |c| info!("{c}")).await {
            Ok(changes) => changes.len(),
            Err(e) => {
                problems.push(format!("removing the interfaces and rules failed: {e}"));
                0
            }
        };
        if let Err(e) = self.paths.reset() {
            problems.push(format!("{e:#}"));
        }
        match reconcile::Actual::observe(&self.sys).await {
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
                local: render_local(&cfg.local),
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
            |c| (render_network(&c.network), render_local(&c.local)),
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
            &render_local(&cfg.local),
        ));
        diff
    }

    fn validate(&self, input: &ConfigInput) -> Validation {
        match load_texts(&input.network, &input.local) {
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
        match load_texts(&input.network, &input.local) {
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
        cfg: Config,
        source: ChangeSource,
        timeout: Option<Duration>,
        base_revision: Option<u64>,
        from_files: bool,
    ) -> Result<Applied, ApiError> {
        if self.pending.is_some() {
            return Err(ApiError::new(
                "another change is waiting for confirmation; confirm or roll it back first",
            ));
        }
        if let Some(base) = base_revision {
            self.check_base(base, &cfg)?;
        }
        if source == ChangeSource::Local
            && let Some(confirmed) = &self.confirmed
        {
            policy::check_edit(&confirmed.network, &cfg.network, confirmed.self_name())
                .map_err(ApiError::new)?;
        }
        let warnings: Vec<Diagnostic> = config::validate(&cfg.network, Some(&cfg.local))
            .into_iter()
            .filter(|d| !d.is_error())
            .collect();
        let diff = self.diff_against_running(&cfg);
        // Within the range of commit_timeout: a huge value from the API
        // would never roll back (or overflow the deadline).
        let timeout = timeout
            .unwrap_or(cfg.network.commit_timeout)
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
        let (who, peer) = match &source {
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
            from_files,
            fingerprint,
            announced: None,
        });
        self.events.emit(
            Level::Info,
            peer.as_deref(),
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
        let me = cfg.self_name();
        let expected = |network: &Network, peer: &str| {
            let client = network
                .site(peer)
                .is_some_and(|s| s.uplink.as_deref() == Some(me));
            if client {
                config::fingerprint(&view::view_for(network, peer))
            } else {
                config::fingerprint(network)
            }
        };
        // Only peers that stay peers (same name and address) and answer now
        // count; a site of another or an old network must not block a change.
        let current = self.direct_peers();
        let relevant: BTreeSet<String> = topology::tunnels_for(&cfg.network, me)
            .into_iter()
            .filter(|t| current.get(&t.peer.name) == Some(&t.peer.address))
            .map(|t| t.peer.name.clone())
            .collect();
        let base_network = self.confirmed.as_ref().map(|c| &c.network);
        let newer = self.probes.read().ok().and_then(|t| {
            t.iter()
                .filter(|(name, p)| p.reachable && relevant.contains(*name))
                .filter_map(|(name, p)| p.announce.map(|a| (name.clone(), a)))
                .find(|(name, a)| {
                    let theirs = fingerprint_hex(&a.fingerprint);
                    // A peer with the content this change is based on (only
                    // a higher revision number) does not make it outdated.
                    a.revision > base
                        && theirs != expected(&cfg.network, name)
                        && base_network.is_none_or(|m| theirs != expected(m, name))
                })
        });
        if let Some((peer, a)) = newer {
            return Err(ApiError::new(format!(
                "peer \"{peer}\" has a newer network configuration (revision {}); wait for the sync, reload and try again",
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
                // The files hold at most an i64.
                cfg.network.revision = base
                    .max(cfg.network.revision)
                    .saturating_add(1)
                    .min(MAX_REVISION);
            }
            self.rejected.clear();
            // A change of its own: the last refusal is history.
            self.sync_problem = None;
        }
        if let Err(e) = self.paths.write_confirmed(&cfg) {
            self.pending = Some(pending);
            return Err(ApiError::new(format!(
                "cannot write the configuration files: {e:#}"
            )));
        }
        self.paths.clear_marker();
        if matches!(pending.source, ChangeSource::Join { .. }) {
            match self.invites.consume_reserved() {
                Ok(Some(code)) => self.events.info(format!("Invitation {code} is used up")),
                Ok(None) => {}
                Err(e) => self.events.error(format!("{e:#}")),
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
                Some(peer.clone())
            }
            ChangeSource::Join { site } => {
                self.invites.release();
                // A site that was there before only wanted to hang off this
                // site after joining again here.
                let before = pending
                    .previous
                    .as_ref()
                    .and_then(|c| c.network.site(site))
                    .map(|s| s.uplink.clone());
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
        } else if let Err(e) = reconcile::teardown(&self.sys, |c| info!("{c}")).await {
            self.events.error(format!("cleanup failed: {e}"));
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
        let Some(cfg) = self.config.clone() else {
            return Ok(Vec::new());
        };
        self.runtime.underlay_mtu = self.underlay_mtus(&cfg).await;
        let desired = desired::derive(&cfg, &self.runtime);
        let events = self.events.clone();
        // A hanging kernel or nft call must not block the daemon (and with
        // it the API and the rollback deadline).
        let changes = tokio::time::timeout(
            RECONCILE_TIMEOUT,
            reconcile::reconcile(&self.sys, &desired, |c| events.info(c.to_string())),
        )
        .await
        .map_err(|_| {
            reconcile::ReconcileError::Sys(wanderlan_core::sys::SysError::new(
                "apply the configuration",
                format!("no answer within {} s", RECONCILE_TIMEOUT.as_secs()),
            ))
        })??;
        let key = topology::forwarding_key(&cfg.network);
        let network_changed = self.applied_key.as_ref().is_some_and(|k| *k != key);
        if network_changed || reconcile::ports_changed(&changes) {
            let bridges: Vec<&str> = desired.bridges().collect();
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

    /// Forgets the learned MAC addresses of all of wanderlan's bridges.
    async fn flush_bridges(&self) {
        let Ok(links) = self.sys.links().await else {
            return;
        };
        for (_, name) in owned_bridges(&links) {
            if let Err(e) = reconcile::flush_learned(&self.sys, &name).await {
                warn!("{e}");
            }
        }
    }

    /// Tells the loop guard which interfaces are bridges and tunnel ports.
    async fn update_port_map(&self) {
        let Ok(links) = self.sys.links().await else {
            return;
        };
        let bridges: BTreeSet<u32> = owned_bridges(&links).into_iter().map(|(i, _)| i).collect();
        let tunnels = links
            .iter()
            .filter(|l| {
                l.name.starts_with(GRETAP_PREFIX) && l.master.is_some_and(|m| bridges.contains(&m))
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
        let carries = self.tunnel_interfaces().await;
        Ok(interface_list(&links, &carries))
    }

    /// Peer → MTU of the interface its tunnel leaves by (an upper bound of the
    /// path MTU, for the MSS until the path is measured).
    async fn underlay_mtus(&self, cfg: &Config) -> BTreeMap<String, u16> {
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
                if cfg.network.site(&peer).is_some() {
                    out.insert(peer, mtu);
                }
            }
        }
        out
    }

    /// Interface index → the peers whose tunnel leaves through it, as the
    /// kernel routes the tunnel packets (from our address, with the policy
    /// rules, e.g. of a VPN).
    async fn tunnel_interfaces(&self) -> BTreeMap<u32, Vec<String>> {
        let mut out: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        let Some(cfg) = &self.config else {
            return out;
        };
        let local = cfg.self_site().map(|s| s.address);
        for tunnel in topology::tunnels_for(&cfg.network, cfg.self_name()) {
            match self.sys.output_interface(tunnel.peer.address, local).await {
                Ok(Some(index)) => out.entry(index).or_default().push(tunnel.peer.name.clone()),
                Ok(None) => {}
                Err(e) => debug!("{e}"),
            }
        }
        out
    }

    /// The traffic per VLAN on the ports of our bridges.
    async fn vlan_traffic(&self) -> Result<VlanTraffic, ApiError> {
        let links = self.sys.links().await.map_err(api_err)?;
        let bridges = owned_bridges(&links);
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
                v.is_fresh().then(|| v.rates.clone())
            }
            Err(_) => None,
        };
        let rates = match fresh {
            Some(rates) => rates,
            None => {
                let counters = self.sys.vlan_stats().await.unwrap_or_default();
                let names: std::collections::HashMap<u32, &str> =
                    links.iter().map(|l| (l.index, l.name.as_str())).collect();
                stats::counters_only(&counters, |i| names.get(&i).map(|n| (*n).to_string()))
            }
        };
        let ports = rates
            .into_iter()
            .filter(|((port, _), _)| ports_of_ours.contains(port.as_str()))
            .map(|((port, vid), r)| VlanPortTraffic {
                peer: port.strip_prefix(GRETAP_PREFIX).map(str::to_string),
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
                .map(|cfg| Arc::new(desired::derive(cfg, &self.runtime))),
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
        let bridges: BTreeSet<u32> = owned_bridges(&links).into_iter().map(|(i, _)| i).collect();
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
            .map(|e| MacEntry {
                mac: format_mac(&e.mac),
                vlan: e.vlan,
                port: name_of(e.port),
                age_secs: e.updated_secs,
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
            };
        };
        let desired = derived
            .desired
            .clone()
            .unwrap_or_else(|| Arc::new(desired::derive(cfg, &self.runtime)));
        let me = cfg.self_site();
        let tunnels = topology::tunnels_for(&cfg.network, cfg.self_name());
        let direct: BTreeSet<&str> = tunnels.iter().map(|t| t.peer.name.as_str()).collect();
        let peers = tunnels
            .iter()
            .map(|t| {
                let port = t.ifname();
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
                let probe = probes.get(&t.peer.name);
                let mss = desired
                    .mss
                    .iter()
                    .find(|r| r.port == port && r.dir == PortDir::Out && !r.ipv6)
                    .map(|r| r.mss);
                let (rx_bps, tx_bps) = rates.get(&port).copied().unwrap_or_default();
                let announce = probe.and_then(|p| p.announce);
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
                PeerStatus {
                    name: t.peer.name.clone(),
                    address: t.peer.address.to_string(),
                    port,
                    port_state,
                    vlans,
                    trunk: cfg.trunk_ports_for(&t.peer.name).join("+"),
                    reachable: probe.is_some_and(|p| p.reachable),
                    last_reply_secs: probe
                        .and_then(|p| p.last_reply)
                        .and_then(|r| r.elapsed().ok())
                        .map(|d| d.as_secs()),
                    rtt_ms: probe.and_then(|p| p.rtt_ms),
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
                                .get(&t.peer.name)
                                .is_some_and(|v| v.fingerprint == theirs)
                        } else {
                            theirs == confirmed_fp
                        }
                    }),
                    refused: self.refused.get(&t.peer.name).cloned(),
                }
            })
            .collect();
        let sites = cfg
            .network
            .sites
            .iter()
            .map(|s| SiteSummary {
                name: s.name.clone(),
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
                is_self: s.name == cfg.self_name(),
                direct: direct.contains(s.name.as_str()),
                uplink: s.uplink.clone(),
                via: topology::via(&cfg.network, cfg.self_name(), &s.name).map(str::to_string),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
