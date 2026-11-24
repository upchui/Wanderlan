//! The controller: owns the configuration and the runtime state and keeps the
//! system in the desired state.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use wanderlan_core::config::{self, Config, ConfigFile, Loaded};
use wanderlan_core::desired::{self, Runtime};
use wanderlan_core::mtu::wire::Announce;
use wanderlan_core::reconcile;
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::topology::{self, ForwardingKey};

use crate::Args;
use crate::probe::{self, ProbeConfig, ProbeEvent, ProbeTable};

/// Delays of the extra flushes after a topology change. Sites apply a new
/// configuration a few seconds apart; until all have switched, a bridge can
/// learn addresses behind ports that are wrong in the new topology.
const FOLLOW_UP_FLUSHES: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];

/// State of the daemon.
struct Controller {
    sys: LinuxSystem,
    config_dir: PathBuf,
    config: Option<Config>,
    runtime: Runtime,
    applied_key: Option<ForwardingKey>,
    last_error: Option<String>,
    flushes: BTreeSet<Instant>,
    probe_config: watch::Sender<ProbeConfig>,
    #[allow(dead_code)]
    probes: ProbeTable,
}

/// Runs the daemon until SIGTERM or SIGINT.
pub async fn run(args: Args) -> Result<()> {
    info!("wanderland {} starting", wanderlan_core::VERSION);
    let sys = LinuxSystem::connect().context("cannot open a netlink connection")?;
    let mut sigterm = signal(SignalKind::terminate()).context("cannot handle SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot handle SIGINT")?;
    let mut sighup = signal(SignalKind::hangup()).context("cannot handle SIGHUP")?;

    let (probe_tx, probe_rx) = watch::channel(ProbeConfig::default());
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let probes: ProbeTable = Arc::new(RwLock::new(BTreeMap::new()));
    let probe_task = probe::spawn(probe_rx, probes.clone(), event_tx);

    let mut ctl = Controller {
        sys,
        config_dir: args.config_dir.clone(),
        config: None,
        runtime: Runtime::default(),
        applied_key: None,
        last_error: None,
        flushes: BTreeSet::new(),
        probe_config: probe_tx,
        probes,
    };
    ctl.set_config(load_config(&ctl.config_dir));

    let mut ticker = tokio::time::interval(Duration::from_secs(args.reconcile_interval.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let next_flush = ctl.flushes.first().copied();
        tokio::select! {
            _ = ticker.tick() => ctl.reconcile().await,
            Some(event) = event_rx.recv() => {
                if ctl.handle_probe_event(event) {
                    ctl.reconcile().await;
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
                info!("Reloading the configuration (SIGHUP)");
                if let Some(cfg) = load_config(&ctl.config_dir) {
                    ctl.set_config(Some(cfg));
                    ctl.last_error = None;
                    ticker.reset_immediately();
                }
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    probe_task.abort();
    info!("Stopping; removing everything wanderlan created");
    let removed = reconcile::teardown(&ctl.sys, |c| info!("{c}"))
        .await
        .context("cleanup failed")?;
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

impl Controller {
    /// Switches to a new configuration and updates the probe service.
    fn set_config(&mut self, cfg: Option<Config>) {
        let probe = match &cfg {
            Some(cfg) => {
                let me = cfg.self_site();
                let peers = topology::tunnels_for(&cfg.mesh, cfg.self_name())
                    .into_iter()
                    .map(|t| (t.peer.name.clone(), t.peer.address))
                    .collect();
                ProbeConfig {
                    bind: me.map(|s| SocketAddrV4::new(s.address, cfg.mesh.control_port)),
                    peers,
                    mtu_interval: cfg
                        .mesh
                        .mss
                        .is_none()
                        .then_some(cfg.mesh.mtu_probe_interval),
                    announce: Announce {
                        revision: cfg.mesh.revision,
                        fingerprint: fingerprint_bytes(&cfg.mesh),
                    },
                }
            }
            None => ProbeConfig::default(),
        };
        // Forget measurements of sites that are no longer peers.
        self.runtime
            .pmtu
            .retain(|peer, _| probe.peers.contains_key(peer));
        self.probe_config.send_replace(probe);
        self.config = cfg;
    }

    /// Handles a probe event; returns true if the desired state changed.
    fn handle_probe_event(&mut self, event: ProbeEvent) -> bool {
        match event {
            ProbeEvent::Pmtu { peer, pmtu } => {
                let old = match pmtu {
                    Some(p) => self.runtime.pmtu.insert(peer, p),
                    None => self.runtime.pmtu.remove(&peer),
                };
                old != pmtu
            }
            ProbeEvent::Announce { peer, announce } => {
                debug!(
                    "Peer \"{peer}\" announces mesh revision {}",
                    announce.revision
                );
                false
            }
        }
    }

    /// Reconciles once. Logs errors only when they change, so a missing trunk
    /// does not flood the log.
    async fn reconcile(&mut self) {
        let Some(cfg) = &self.config else { return };
        let desired = desired::derive(cfg, &self.runtime);
        match reconcile::reconcile(&self.sys, &desired, |c| info!("{c}")).await {
            Ok(changes) => {
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
                if self.last_error.is_some() || !changes.is_empty() {
                    info!(
                        "Site \"{}\" is up to date ({} tunnels, VLANs {})",
                        cfg.self_name(),
                        desired.tunnels.len(),
                        cfg.mesh.vlans
                    );
                }
                self.last_error = None;
            }
            Err(e) => {
                let msg = e.to_string();
                if self.last_error.as_deref() != Some(msg.as_str()) {
                    error!("Cannot apply the configuration: {msg}");
                }
                self.last_error = Some(msg);
            }
        }
    }
}

/// The mesh fingerprint as raw bytes.
fn fingerprint_bytes(mesh: &config::Mesh) -> [u8; 32] {
    let hex = config::fingerprint(mesh);
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = hex
            .get(2 * i..2 * i + 2)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .unwrap_or(0);
    }
    out
}

/// Loads and validates the configuration files. Returns `None` if the site
/// is not configured or the files are invalid (errors are logged).
fn load_config(dir: &Path) -> Option<Config> {
    let mesh_path = dir.join("mesh.toml");
    let local_path = dir.join("local.toml");
    let texts = read_optional(&mesh_path).and_then(|m| Ok((m, read_optional(&local_path)?)));
    let (mesh_text, local_text) = match texts {
        Ok(t) => t,
        Err(e) => {
            error!("{e:#}");
            return None;
        }
    };
    let label = |file: ConfigFile| match file {
        ConfigFile::Mesh => mesh_path.display().to_string(),
        ConfigFile::Local => local_path.display().to_string(),
    };
    match config::load(&mesh_text, &local_text) {
        Ok(outcome) => {
            for w in &outcome.warnings {
                warn!("{}", w.display_with(&label(w.file)));
            }
            match outcome.loaded {
                Loaded::Unconfigured { .. } => {
                    info!("This site is not configured yet; run `wanderlan` to set it up");
                    None
                }
                Loaded::Configured(cfg) => {
                    info!(
                        "Loaded configuration: site \"{}\", {} sites, VLANs {}, trunk {}",
                        cfg.self_name(),
                        cfg.mesh.sites.len(),
                        cfg.mesh.vlans,
                        cfg.trunk()
                    );
                    Some(cfg)
                }
            }
        }
        Err(diags) => {
            for d in &diags {
                error!("{}", d.display_with(&label(d.file)));
            }
            error!("The configuration is invalid; nothing was changed");
            None
        }
    }
}

/// Reads a file, treating a missing file as empty.
fn read_optional(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}
