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
    PortTraffic, Request, ResetReport, SiteState, SiteSummary, Status, Validation,
};
use wanderlan_core::config::{
    self, Config, ConfigFile, Diagnostic, Loaded, Mesh, render_local, render_mesh, unified_diff,
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
use crate::stats::{self, Rates};
use crate::sync_net::{self, Offer};

/// Delays of the extra flushes after a topology change. Sites apply a new
/// configuration a few seconds apart; until all have switched, a bridge can
/// learn addresses behind ports that are wrong in the new topology.
const FOLLOW_UP_FLUSHES: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];
/// How long a removed peer may still fetch the configuration from us.
const GRACE: Duration = Duration::from_secs(300);
/// Wait time after a failed fetch before trying the same peer again.
const FETCH_BACKOFF: Duration = Duration::from_secs(30);
/// Shortest accepted confirmation timeout.
const MIN_TIMEOUT: Duration = Duration::from_secs(5);

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
        /// True for a client of this site: merge its view instead of
        /// replacing the own mesh.
        merge: bool,
        /// The mesh text or an error.
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
    applied_at: SystemTime,
    source: ChangeSource,
    from_files: bool,
    fingerprint: String,
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
    inputs: mpsc::Sender<Input>,
    rejected: BTreeSet<String>,
    fetching: bool,
    fetch_backoff: BTreeMap<String, Instant>,
    grace: BTreeMap<String, (Ipv4Addr, Instant)>,
    generation: u64,
    invites: Invites,
    join_failures: BTreeMap<Ipv4Addr, VecDeque<Instant>>,
    joining: bool,
}

/// Runs the daemon until SIGTERM or SIGINT.
pub async fn run(args: Args) -> Result<()> {
    info!("wanderland {} starting", wanderlan_core::VERSION);
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

    let mut tasks = vec![
        (
            "probes",
            probe::spawn(probe_rx, probes.clone(), probe_event_tx),
        ),
        ("sync", sync_net::spawn(offer_rx, inputs_tx.clone())),
        ("statistics", stats::spawn(sys.clone(), rates.clone())),
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
        inputs: inputs_tx,
        rejected: BTreeSet::new(),
        fetching: false,
        fetch_backoff: BTreeMap::new(),
        grace: BTreeMap::new(),
        generation: 0,
        invites,
        join_failures: BTreeMap::new(),
        joining: false,
    };
    ctl.startup();
    ctl.seed_measurements().await;

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
                // Exit without tearing anything down: systemd starts the
                // daemon again, which adopts the existing interfaces.
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

    for (_, task) in tasks {
        task.abort();
    }
    info!("Stopping; removing everything wanderlan created");
    let removed = reconcile::teardown(&ctl.sys, |c| info!("{c}"))
        .await
        .context("cleanup failed")?;
    let _ = std::fs::remove_file(&args.socket);
    info!("Stopped ({} changes undone)", removed.len());
    Ok(())
}

/// wanderlan's bridges (index, name): br0 and those of further trunks.
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
fn load_texts(mesh: &str, local: &str) -> Result<(Loaded, Vec<Diagnostic>), Vec<Diagnostic>> {
    config::load(mesh, local).map(|o| (o.loaded, o.warnings))
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
        if cfg.mesh.mss.is_some() {
            return;
        }
        let Ok(Some(rules)) = self.sys.mss_rules().await else {
            return;
        };
        let peers = self.direct_peers();
        for rule in rules.iter().filter(|r| r.dir == PortDir::Out && !r.ipv6) {
            if let Some(peer) = rule.port.strip_prefix(GRETAP_PREFIX)
                && peers.contains_key(peer)
            {
                self.runtime.pmtu.insert(
                    peer.to_string(),
                    rule.mss + wanderlan_core::mtu::MSS_OVERHEAD,
                );
            }
        }
    }

    /// Loads and validates the files; logs problems.
    fn load_files(&self) -> Option<Config> {
        let (mesh, local) = match self.paths.read() {
            Ok(t) => t,
            Err(e) => {
                self.events.error(format!("{e:#}"));
                return None;
            }
        };
        let label = |file: ConfigFile| match file {
            ConfigFile::Mesh => self.paths.mesh().display().to_string(),
            ConfigFile::Local => self.paths.local().display().to_string(),
        };
        match load_texts(&mesh, &local) {
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
                            "Loaded configuration: site \"{}\", {} sites, VLANs {}, trunk {}, mesh revision {}",
                            cfg.self_name(),
                            cfg.mesh.sites.len(),
                            cfg.mesh.vlans,
                            cfg.trunk(),
                            cfg.mesh.revision
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
        self.runtime
            .blocked
            .retain(|port| new_peers.keys().any(|p| topology::gretap_name(p) == *port));
        self.publish();
    }

    /// Direct peers of the configuration in effect.
    fn direct_peers(&self) -> BTreeMap<String, Ipv4Addr> {
        self.config.as_ref().map_or_else(BTreeMap::new, |cfg| {
            topology::tunnels_for(&cfg.mesh, cfg.self_name())
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
                .map(|s| SocketAddrV4::new(s.address, cfg.mesh.control_port))
        });
        let version = |m: &Mesh| Announce {
            revision: m.revision,
            fingerprint: fingerprint_bytes(&config::fingerprint(m)),
        };
        // Servers and the uplink get what this site knows; a client of this
        // site (and a removed site) only its own view of the network.
        let mut announce_to = BTreeMap::new();
        let mut views = BTreeMap::new();
        let announce = match &self.confirmed {
            None => Announce::default(),
            Some(c) => {
                let own = render_mesh(&c.mesh);
                for (name, addr) in peers.iter().chain(responders.iter()) {
                    let client = c
                        .mesh
                        .site(name)
                        .is_none_or(|s| s.uplink.as_deref() == Some(c.self_name()));
                    if client {
                        let v = view::view_for(&c.mesh, name);
                        announce_to.insert(name.clone(), version(&v));
                        views.insert(*addr, render_mesh(&v));
                    } else {
                        views.insert(*addr, own.clone());
                    }
                }
                version(&c.mesh)
            }
        };
        let mtu_interval = self
            .config
            .as_ref()
            .and_then(|c| c.mesh.mss.is_none().then_some(c.mesh.mtu_probe_interval));
        self.probe_config.send_replace(ProbeConfig {
            bind,
            peers,
            responders,
            generation: self.generation,
            mtu_interval,
            announce,
            announce_to,
        });
        self.offer.send_replace(Offer { bind, views });
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
                    "Peer \"{peer}\" announces mesh revision {}",
                    announce.revision
                );
                self.maybe_sync();
            }
            Input::Fetched {
                peer,
                merge,
                result,
            } => self.fetched(peer, merge, result).await,
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
                    .find(|(_, pp)| pp.last_reply.is_some_and(|r| r > since))
                    .map(|(name, _)| name.clone())
            });
            if let Some(via) = reachable {
                match self.confirm(false) {
                    Ok(c) => self.events.emit(
                        Level::Info,
                        Some(&source),
                        format!(
                            "Configuration from \"{source}\" confirmed automatically (peer \"{via}\" is reachable); mesh revision {}",
                            c.revision
                        ),
                    ),
                    Err(e) => self.events.error(e.message),
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
                    .and_then(|pp| pp.last_reply)
                    .is_some_and(|r| r > since)
            });
            if up {
                match self.confirm(false) {
                    Ok(c) => self.events.emit(
                        Level::Info,
                        Some(&site),
                        format!(
                            "Site \"{site}\" joined the network; mesh revision {}",
                            c.revision
                        ),
                    ),
                    Err(e) => self.events.error(e.message),
                }
            }
        }
        self.maybe_sync();
    }

    /// Fetches a newer mesh from a peer, if one announces it: from a server
    /// or the uplink (replacing the own mesh), otherwise the changes of a
    /// client (merged into the own mesh).
    fn maybe_sync(&mut self) {
        if self.pending.is_some() || self.fetching {
            return;
        }
        let Some(confirmed) = &self.confirmed else {
            return;
        };
        let me = confirmed.self_name();
        let mesh = &confirmed.mesh;
        let ours = Version::of(mesh);
        let port = mesh.control_port;
        let now = Instant::now();
        let table = self.probes.read().map(|t| t.clone()).unwrap_or_default();
        let mut upstream: Vec<(String, Version)> = Vec::new();
        let mut client_changes: Option<(String, Version)> = None;
        for t in topology::tunnels_for(mesh, me) {
            let name = t.peer.name.as_str();
            if self.fetch_backoff.get(name).is_some_and(|t| *t > now) {
                continue;
            }
            let Some(announce) = table.get(name).and_then(|p| p.announce) else {
                continue;
            };
            let theirs = Version {
                revision: announce.revision,
                fingerprint: fingerprint_hex(&announce.fingerprint),
            };
            if self.rejected.contains(&theirs.fingerprint) {
                continue;
            }
            match t.kind {
                TunnelKind::Server => upstream.push((name.to_string(), theirs)),
                // The uplink has the last word on what this site knows.
                TunnelKind::Uplink if theirs.revision > ours.revision => {
                    upstream.push((name.to_string(), theirs));
                }
                TunnelKind::Uplink => {}
                TunnelKind::Client => {
                    let expected = Version::of(&view::view_for(mesh, name));
                    if client_changes.is_none()
                        && theirs.fingerprint != expected.fingerprint
                        && theirs.revision >= expected.revision
                    {
                        client_changes = Some((name.to_string(), theirs));
                    }
                }
            }
        }
        // Newer versions from servers or the uplink first, then changes of
        // a client.
        let chosen = sync::choose(
            &ours,
            upstream.iter().map(|(n, v)| (n.as_str(), v.clone())),
            &self.rejected,
        )
        .map(|(n, v)| (n.to_string(), v, false))
        .or_else(|| client_changes.map(|(n, v)| (n, v, true)));
        let Some((peer, version, merge)) = chosen else {
            return;
        };
        let peers = self.direct_peers();
        let Some(addr) = peers.get(&peer) else { return };
        let target = SocketAddrV4::new(*addr, port);
        let text = if merge {
            format!(
                "Client \"{peer}\" has changes (revision {}, here: {}); fetching them",
                version.revision, ours.revision
            )
        } else {
            format!(
                "Peer \"{peer}\" has mesh revision {} (here: {}); fetching it",
                version.revision, ours.revision
            )
        };
        self.events.emit(Level::Info, Some(&peer), text);
        self.fetching = true;
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let result = sync_net::fetch(target).await.map_err(|e| format!("{e:#}"));
            let _ = inputs
                .send(Input::Fetched {
                    peer,
                    merge,
                    result,
                })
                .await;
        });
    }

    async fn fetched(&mut self, peer: String, merge: bool, result: Result<String, String>) {
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
        let parsed = match config::parse_mesh(&text) {
            Ok(p) => p.mesh,
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
        let Some(mesh) = parsed else { return };
        let fingerprint = config::fingerprint(&mesh);
        let mesh = if merge {
            let Some(own) = self.confirmed.as_ref().map(|c| c.mesh.clone()) else {
                return;
            };
            match view::merge(&own, &mesh, &peer) {
                Ok(merged) => merged,
                Err(e) => {
                    self.rejected.insert(fingerprint);
                    self.events.emit(
                        Level::Warn,
                        Some(&peer),
                        format!("Not using the changes of client \"{peer}\": {e}"),
                    );
                    return;
                }
            }
        } else {
            mesh
        };
        match sync::accept(mesh, &local) {
            Err(e) => {
                self.rejected.insert(fingerprint);
                self.events.emit(
                    Level::Warn,
                    Some(&peer),
                    format!("Not using the configuration from \"{peer}\": {e}"),
                );
            }
            Ok(cfg) => {
                let source = ChangeSource::Sync { peer: peer.clone() };
                if let Err(e) = self.apply(cfg, source, None, None, false).await {
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
                let (mesh, local) = self.paths.read().map_err(api_err)?;
                let cfg = self.candidate(&ConfigInput { mesh, local })?;
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
                join::check_rules(&confirmed.mesh, confirmed.self_name(), &rules)
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
            .and_then(|c| c.self_site().map(|s| (s.address, c.mesh.control_port)))
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
    /// with the new mesh. The change is confirmed as soon as the new site
    /// answers probes, or rolled back after `commit_timeout`.
    async fn admit(&mut self, from: Ipv4Addr, request: JoinRequest) -> JoinReply {
        let error = |message: String| JoinReply::Error { message };
        let now = Instant::now();
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
                wanderlan_core::VERSION,
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
            &confirmed.mesh,
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
            let mesh = render_mesh(&view::view_for(&confirmed.mesh, &admission.name));
            return JoinReply::Ok {
                site: admission.name,
                via,
                mesh,
            };
        }
        // The new site gets its view of the network only.
        let mesh = render_mesh(&view::view_for(&admission.mesh, &admission.name));
        let site = admission.name;
        if !admission.added {
            self.events.emit(
                Level::Info,
                Some(&site),
                format!("Site \"{site}\" joins again through this site and becomes a client of it"),
            );
        }
        let cfg = Config {
            mesh: admission.mesh,
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
                JoinReply::Ok { site, via, mesh }
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
            version: wanderlan_core::VERSION.into(),
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
            JoinReply::Ok { site, via, mesh } => (site, via, mesh),
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
        let mesh = match config::parse_mesh(&text) {
            Ok(parsed) => parsed.mesh,
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
            trunk: Some(params.trunk.clone()),
            ..Default::default()
        };
        let cfg = sync::accept(mesh, &local).map_err(|e| {
            ApiError::new(format!(
                "the configuration from \"{via}\" cannot be used: {e}"
            ))
        })?;
        let revision = cfg.mesh.revision;
        let sites = cfg.mesh.sites.len();
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
                mesh: render_mesh(&cfg.mesh),
                local: render_local(&cfg.local),
                revision: cfg.mesh.revision,
                fingerprint: config::fingerprint(&cfg.mesh),
            },
            None => ConfigTexts {
                configured: false,
                mesh: String::new(),
                local: self.paths.read().map(|(_, l)| l).unwrap_or_default(),
                revision: 0,
                fingerprint: String::new(),
            },
        }
    }

    fn diff_against_running(&self, cfg: &Config) -> String {
        let (old_mesh, old_local) = self.config.as_ref().map_or_else(
            || (String::new(), String::new()),
            |c| (render_mesh(&c.mesh), render_local(&c.local)),
        );
        let mut diff = unified_diff(
            "running/mesh.toml",
            &old_mesh,
            "new/mesh.toml",
            &render_mesh(&cfg.mesh),
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
        match load_texts(&input.mesh, &input.local) {
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
        match load_texts(&input.mesh, &input.local) {
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
            policy::check_edit(&confirmed.mesh, &cfg.mesh, confirmed.self_name())
                .map_err(ApiError::new)?;
        }
        let warnings: Vec<Diagnostic> = config::validate(&cfg.mesh, Some(&cfg.local))
            .into_iter()
            .filter(|d| !d.is_error())
            .collect();
        let diff = self.diff_against_running(&cfg);
        let timeout = timeout.unwrap_or(cfg.mesh.commit_timeout).max(MIN_TIMEOUT);
        let fingerprint = config::fingerprint(&cfg.mesh);
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
            applied_at: SystemTime::now(),
            source,
            from_files,
            fingerprint,
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
            && c.mesh.revision > base
        {
            return Err(ApiError::new(format!(
                "the configuration changed meanwhile (now revision {}, your change is based on {base}); reload it and try again",
                c.mesh.revision
            )));
        }
        // What a peer would have if it were up to date: a client of this
        // site its view, any other peer the whole of what this site knows.
        let me = cfg.self_name();
        let expected = |mesh: &Mesh, peer: &str| {
            let client = mesh
                .site(peer)
                .is_some_and(|s| s.uplink.as_deref() == Some(me));
            if client {
                config::fingerprint(&view::view_for(mesh, peer))
            } else {
                config::fingerprint(mesh)
            }
        };
        // Only peers that stay peers (same name and address) and answer now
        // count; a site of another or an old network must not block a change.
        let current = self.direct_peers();
        let relevant: BTreeSet<String> = topology::tunnels_for(&cfg.mesh, me)
            .into_iter()
            .filter(|t| current.get(&t.peer.name) == Some(&t.peer.address))
            .map(|t| t.peer.name.clone())
            .collect();
        let base_mesh = self.confirmed.as_ref().map(|c| &c.mesh);
        let newer = self.probes.read().ok().and_then(|t| {
            t.iter()
                .filter(|(name, p)| p.reachable && relevant.contains(*name))
                .filter_map(|(name, p)| p.announce.map(|a| (name.clone(), a)))
                .find(|(name, a)| {
                    let theirs = fingerprint_hex(&a.fingerprint);
                    // A peer with the content this change is based on (only
                    // a higher revision number) does not make it outdated.
                    a.revision > base
                        && theirs != expected(&cfg.mesh, name)
                        && base_mesh.is_none_or(|m| theirs != expected(m, name))
                })
        });
        if let Some((peer, a)) = newer {
            return Err(ApiError::new(format!(
                "peer \"{peer}\" has a newer mesh configuration (revision {}); wait for the sync, reload and try again",
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
            let previous_version = self.confirmed.as_ref().map(|c| Version::of(&c.mesh));
            let changed = previous_version
                .as_ref()
                .is_none_or(|v| v.fingerprint != config::fingerprint(&cfg.mesh));
            if changed {
                let base = previous_version.map_or(0, |v| v.revision);
                cfg.mesh.revision = base.max(cfg.mesh.revision) + 1;
            }
            self.rejected.clear();
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
        self.runtime.blocked.clear();
        if user {
            self.events.info(format!(
                "Change confirmed; mesh revision {} is in effect",
                cfg.mesh.revision
            ));
        }
        Ok(Confirmed {
            revision: cfg.mesh.revision,
        })
    }

    /// Reverts the pending change.
    async fn rollback(&mut self, reason: &str) -> Result<(), ApiError> {
        let Some(pending) = self.pending.take() else {
            return Err(ApiError::new("there is no change to roll back"));
        };
        let peer = match &pending.source {
            ChangeSource::Sync { peer } => {
                self.rejected.insert(pending.fingerprint.clone());
                Some(peer.clone())
            }
            ChangeSource::Join { site } => {
                self.invites.release();
                self.events.emit(
                    Level::Warn,
                    Some(site),
                    format!(
                        "The new site \"{site}\" did not come up in time; it was removed again and its invitation can be used again"
                    ),
                );
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
        if let Err(e) = self.reconcile_once().await {
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
        let desired = desired::derive(&cfg, &self.runtime);
        let events = self.events.clone();
        let changes =
            reconcile::reconcile(&self.sys, &desired, |c| events.info(c.to_string())).await?;
        let key = topology::forwarding_key(&cfg.mesh);
        let mesh_changed = self.applied_key.as_ref().is_some_and(|k| *k != key);
        if mesh_changed || reconcile::ports_changed(&changes) {
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
                cfg.mesh.vlans
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
        let new = PortMap { bridges, tunnels };
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
        let name_of = |i: u32| links.iter().find(|l| l.index == i).map(|l| l.name.clone());
        Ok(links
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
            })
            .collect())
    }

    async fn mac_table(&self) -> Result<Vec<MacEntry>, ApiError> {
        let links = self.sys.links().await.map_err(api_err)?;
        let fdb = self.sys.fdb().await.map_err(api_err)?;
        let bridges: BTreeSet<u32> = owned_bridges(&links).into_iter().map(|(i, _)| i).collect();
        let name_of = |i: u32| {
            links
                .iter()
                .find(|l| l.index == i)
                .map_or_else(|| format!("#{i}"), |l| l.name.clone())
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
        let fdb = self.sys.fdb().await.unwrap_or_default();
        let probes = self.probes.read().map(|t| t.clone()).unwrap_or_default();
        let rates = self.rates.read().map(|r| r.clone()).unwrap_or_default();
        let confirmed_fp = self
            .confirmed
            .as_ref()
            .map(|c| config::fingerprint(&c.mesh))
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
                version: wanderlan_core::VERSION.into(),
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
            };
        };
        let desired = desired::derive(cfg, &self.runtime);
        let me = cfg.self_site();
        let tunnels = topology::tunnels_for(&cfg.mesh, cfg.self_name());
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
                let macs = link.map_or(0, |l| {
                    fdb.iter()
                        .filter(|e| e.port == l.index && !e.permanent)
                        .count()
                });
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
                    trunk: cfg.trunk_for(&t.peer.name).to_string(),
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
                            self.confirmed.as_ref().is_some_and(|c| {
                                theirs
                                    == config::fingerprint(&view::view_for(&c.mesh, &t.peer.name))
                            })
                        } else {
                            theirs == confirmed_fp
                        }
                    }),
                }
            })
            .collect();
        let sites = cfg
            .mesh
            .sites
            .iter()
            .map(|s| SiteSummary {
                name: s.name.clone(),
                address: s.address.to_string(),
                vlans: {
                    // Own VLANs (towards its own clients only) with a "+".
                    let own = cfg.mesh.own_vlans(s);
                    let vlans = cfg.mesh.effective_vlans(s);
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
                via: topology::via(&cfg.mesh, cfg.self_name(), &s.name).map(str::to_string),
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
            version: wanderlan_core::VERSION.into(),
            state: if self.last_error.is_some() {
                SiteState::Error
            } else {
                SiteState::Running
            },
            site: Some(cfg.self_name().to_string()),
            address: me.map(|s| s.address.to_string()),
            online: me.is_none_or(|s| s.online),
            trunk: Some(cfg.trunk().to_string()),
            vlans: me.map(|s| cfg.mesh.effective_vlans(s).to_string()),
            revision: cfg.mesh.revision,
            fingerprint: config::fingerprint(&cfg.mesh),
            pending,
            last_error: self.last_error.clone(),
            peers,
            sites,
            blocked,
            invites: self.invites.list().len(),
            trunk_traffic: trunks.first().cloned(),
            trunks,
        }
    }
}
