//! The controller: owns the configuration and the runtime state and keeps the
//! system in the desired state.
//!
//! All state changes happen here, sequentially: API requests, probe results,
//! loop guard reports, fetched configurations and timers arrive as [`Input`]s
//! or timer ticks in one select loop.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use wanderlan_core::api::{
    ApiError, Applied, ChangeSource, ConfigInput, ConfigTexts, Confirmed, Done, Interface, Level,
    MacEntry, PeerStatus, PendingChange, PortState, Request, SiteState, SiteSummary, Status,
    Validation,
};
use wanderlan_core::config::{
    self, Config, ConfigFile, Diagnostic, Loaded, render_local, render_mesh, unified_diff,
};
use wanderlan_core::desired::{self, OWNER_ALIAS, Runtime};
use wanderlan_core::loopguard::format_mac;
use wanderlan_core::mtu::wire::Announce;
use wanderlan_core::reconcile::{self, Change};
use wanderlan_core::sync::{self, Version};
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::sys::nft::NftOps as _;
use wanderlan_core::sys::nft::PortDir;
use wanderlan_core::sys::observe::ObserveOps as _;
use wanderlan_core::sys::{LinkKind, NetOps as _};
use wanderlan_core::topology::{self, ForwardingKey, GRETAP_PREFIX};

use crate::Args;
use crate::api_server;
use crate::events::EventLog;
use crate::fdb_guard::{self, LoopReport, PortMap};
use crate::files::Paths;
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
        /// The mesh text or an error.
        result: Result<String, String>,
    },
    /// The loop guard detected a loop.
    Loop(LoopReport),
}

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
        probe::spawn(probe_rx, probes.clone(), probe_event_tx),
        sync_net::spawn(offer_rx),
        stats::spawn(sys.clone(), rates.clone()),
        api_server::spawn(args.socket.clone(), events.clone(), inputs_tx.clone())
            .context("cannot start the API")?,
    ];
    tasks.extend(fdb_guard::spawn(&sys, ports_rx, inputs_tx.clone()));

    let paths = Paths {
        config_dir: args.config_dir.clone(),
        state_dir: args.state_dir.clone(),
    };
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
                if let Err(e) = reconcile::flush_learned(&ctl.sys, wanderlan_core::BRIDGE_NAME).await {
                    warn!("{e}");
                }
            }
            _ = sighup.recv() => {
                info!("SIGHUP: reloading the configuration files");
                ctl.reload_files().await;
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    for task in tasks {
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

/// Sleeps until the instant, or forever if there is none.
async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
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
        let announce = self
            .confirmed
            .as_ref()
            .map_or_else(Announce::default, |c| Announce {
                revision: c.mesh.revision,
                fingerprint: fingerprint_bytes(&config::fingerprint(&c.mesh)),
            });
        let mtu_interval = self
            .config
            .as_ref()
            .and_then(|c| c.mesh.mss.is_none().then_some(c.mesh.mtu_probe_interval));
        let allowed = peers.values().chain(responders.values()).copied().collect();
        self.probe_config.send_replace(ProbeConfig {
            bind,
            peers,
            responders,
            generation: self.generation,
            mtu_interval,
            announce,
        });
        self.offer.send_replace(Offer {
            bind,
            allowed,
            mesh: self
                .confirmed
                .as_ref()
                .map(|c| render_mesh(&c.mesh))
                .unwrap_or_default(),
        });
    }

    async fn handle(&mut self, input: Input) {
        match input {
            Input::Api { request, reply } => {
                let result = self.api(request).await;
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
            Input::Fetched { peer, result } => self.fetched(peer, result).await,
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
        self.maybe_sync();
    }

    /// Fetches a newer mesh from a peer, if one announces it.
    fn maybe_sync(&mut self) {
        if self.pending.is_some() || self.fetching {
            return;
        }
        let Some(confirmed) = &self.confirmed else {
            return;
        };
        let ours = Version::of(&confirmed.mesh);
        let port = confirmed.mesh.control_port;
        let peers = self.direct_peers();
        let now = Instant::now();
        let announced: Vec<(String, Version)> = self
            .probes
            .read()
            .map(|t| {
                t.iter()
                    .filter(|(name, _)| peers.contains_key(*name))
                    .filter(|(name, _)| self.fetch_backoff.get(*name).is_none_or(|t| *t <= now))
                    .filter_map(|(name, p)| {
                        p.announce.map(|a| {
                            (
                                name.clone(),
                                Version {
                                    revision: a.revision,
                                    fingerprint: fingerprint_hex(&a.fingerprint),
                                },
                            )
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let chosen = sync::choose(
            &ours,
            announced.iter().map(|(n, v)| (n.as_str(), v.clone())),
            &self.rejected,
        );
        let Some((peer, version)) = chosen else {
            return;
        };
        let Some(addr) = peers.get(peer) else { return };
        let target = SocketAddrV4::new(*addr, port);
        let peer = peer.to_string();
        self.events.emit(
            Level::Info,
            Some(&peer),
            format!(
                "Peer \"{peer}\" has mesh revision {} (here: {}); fetching it",
                version.revision, ours.revision
            ),
        );
        self.fetching = true;
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let result = sync_net::fetch(target).await.map_err(|e| format!("{e:#}"));
            let _ = inputs.send(Input::Fetched { peer, result }).await;
        });
    }

    async fn fetched(&mut self, peer: String, result: Result<String, String>) {
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
        }
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
        let fingerprint = config::fingerprint(&cfg.mesh);
        let newer = self.probes.read().ok().and_then(|t| {
            t.iter()
                .filter_map(|(name, p)| p.announce.map(|a| (name.clone(), a)))
                .find(|(_, a)| a.revision > base && fingerprint_hex(&a.fingerprint) != fingerprint)
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
            info!(
                "Flushing learned MAC addresses of {} because the topology changed",
                desired.bridge
            );
            if let Err(e) = reconcile::flush_learned(&self.sys, &desired.bridge).await {
                warn!("{e}");
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

    /// Tells the loop guard which interfaces are tunnel ports.
    async fn update_port_map(&self) {
        let Ok(links) = self.sys.links().await else {
            return;
        };
        let bridge = links
            .iter()
            .find(|l| {
                l.name == wanderlan_core::BRIDGE_NAME && l.alias.as_deref() == Some(OWNER_ALIAS)
            })
            .map(|l| l.index);
        let tunnels = links
            .iter()
            .filter(|l| l.name.starts_with(GRETAP_PREFIX) && bridge.is_some() && l.master == bridge)
            .map(|l| (l.index, (l.name.clone(), l.isolated.unwrap_or(false))))
            .collect();
        let new = PortMap { bridge, tunnels };
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
        let bridge = links
            .iter()
            .find(|l| l.name == wanderlan_core::BRIDGE_NAME)
            .map(|l| l.index);
        let name_of = |i: u32| {
            links
                .iter()
                .find(|l| l.index == i)
                .map_or_else(|| format!("#{i}"), |l| l.name.clone())
        };
        let mut out: Vec<MacEntry> = fdb
            .into_iter()
            .filter(|e| !e.permanent && bridge.is_some() && e.bridge == bridge)
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
                role: None,
                address: None,
                trunk: None,
                vlans: None,
                revision: 0,
                fingerprint: String::new(),
                pending,
                last_error: self.last_error.clone(),
                peers: Vec::new(),
                sites: Vec::new(),
                blocked,
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
                let port_state = if self.runtime.blocked.contains(&port) {
                    PortState::Blocked
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
                PeerStatus {
                    name: t.peer.name.clone(),
                    role: t.peer.role,
                    address: t.peer.address.to_string(),
                    port,
                    port_state,
                    reachable: probe.is_some_and(|p| p.reachable),
                    last_reply_secs: probe
                        .and_then(|p| p.last_reply)
                        .and_then(|r| r.elapsed().ok())
                        .map(|d| d.as_secs()),
                    rtt_ms: probe.and_then(|p| p.rtt_ms),
                    pmtu: probe.and_then(|p| p.pmtu),
                    mss,
                    macs,
                    rx_bps,
                    tx_bps,
                    revision: announce.map(|a| a.revision),
                    in_sync: announce.map(|a| fingerprint_hex(&a.fingerprint) == confirmed_fp),
                }
            })
            .collect();
        let sites = cfg
            .mesh
            .sites
            .iter()
            .map(|s| SiteSummary {
                name: s.name.clone(),
                role: s.role,
                hub: s.hub.clone(),
                address: s.address.to_string(),
                is_self: s.name == cfg.self_name(),
                direct: direct.contains(s.name.as_str()),
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
            role: me.map(|s| s.role),
            address: me.map(|s| s.address.to_string()),
            trunk: Some(cfg.trunk().to_string()),
            vlans: Some(cfg.mesh.vlans.to_string()),
            revision: cfg.mesh.revision,
            fingerprint: config::fingerprint(&cfg.mesh),
            pending,
            last_error: self.last_error.clone(),
            peers,
            sites,
            blocked,
        }
    }
}
