//! Main loop of the daemon.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use tokio::time::Instant;

use anyhow::{Context as _, Result};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use wanderlan_core::config::{self, Config, ConfigFile, Loaded};
use wanderlan_core::desired;
use wanderlan_core::reconcile;
use wanderlan_core::sys::netlink::Netlink;
use wanderlan_core::topology::{self, ForwardingKey};

use crate::Args;

/// Runs the daemon until SIGTERM or SIGINT.
pub async fn run(args: Args) -> Result<()> {
    info!("wanderland {} starting", wanderlan_core::VERSION);
    let net = Netlink::connect().context("cannot open a netlink connection")?;
    let mut sigterm = signal(SignalKind::terminate()).context("cannot handle SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot handle SIGINT")?;
    let mut sighup = signal(SignalKind::hangup()).context("cannot handle SIGHUP")?;

    let mut current = load_config(&args.config_dir);
    let mut ticker = tokio::time::interval(Duration::from_secs(args.reconcile_interval.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_error: Option<String> = None;
    let mut applied_key = None;
    // Follow-up flushes of the learned MAC addresses after a topology change.
    let mut flushes: BTreeSet<Instant> = BTreeSet::new();

    loop {
        let next_flush = flushes.first().copied();
        tokio::select! {
            _ = ticker.tick() => {
                if let Some(cfg) = &current {
                    let (error, changed) = apply(&net, cfg, last_error, &mut applied_key).await;
                    last_error = error;
                    if changed {
                        let now = Instant::now();
                        flushes.extend(FOLLOW_UP_FLUSHES.iter().map(|d| now + *d));
                    }
                }
            }
            () = sleep_until_opt(next_flush) => {
                if let Some(at) = next_flush {
                    flushes.remove(&at);
                }
                if let Err(e) = reconcile::flush_learned(&net, wanderlan_core::BRIDGE_NAME).await {
                    warn!("{e}");
                }
            }
            _ = sighup.recv() => {
                info!("Reloading the configuration (SIGHUP)");
                if let Some(cfg) = load_config(&args.config_dir) {
                    current = Some(cfg);
                    last_error = None;
                    ticker.reset_immediately();
                }
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    info!("Stopping; removing the interfaces created by wanderlan");
    let removed = reconcile::teardown(&net, |c| info!("{c}"))
        .await
        .context("cleanup failed")?;
    info!("Stopped ({} changes undone)", removed.len());
    Ok(())
}

/// Delays of the extra flushes after a topology change. Sites apply a new
/// configuration a few seconds apart; until all have switched, a bridge can
/// learn addresses behind ports that are wrong in the new topology.
const FOLLOW_UP_FLUSHES: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];

/// Sleeps until the instant, or forever if there is none.
async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Reconciles once. Logs errors only when they change, so a missing trunk
/// does not flood the log every 30 seconds. Returns the error (if any) and
/// whether the forwarding topology changed.
async fn apply(
    net: &Netlink,
    cfg: &Config,
    last_error: Option<String>,
    applied_key: &mut Option<ForwardingKey>,
) -> (Option<String>, bool) {
    let desired = desired::derive(cfg, &BTreeSet::new());
    match reconcile::reconcile(net, &desired, |c| info!("{c}")).await {
        Ok(changes) => {
            let key = topology::forwarding_key(&cfg.mesh);
            let mesh_changed = applied_key.as_ref().is_some_and(|k| *k != key);
            let topology_changed = mesh_changed || reconcile::ports_changed(&changes);
            if topology_changed {
                info!(
                    "Flushing learned MAC addresses of {} because the topology changed",
                    desired.bridge
                );
                if let Err(e) = reconcile::flush_learned(net, &desired.bridge).await {
                    warn!("{e}");
                }
            }
            *applied_key = Some(key);
            if last_error.is_some() || !changes.is_empty() {
                info!(
                    "Site \"{}\" is up to date ({} tunnels, VLANs {})",
                    cfg.self_name(),
                    desired.tunnels.len(),
                    cfg.mesh.vlans
                );
            }
            (None, topology_changed)
        }
        Err(e) => {
            let msg = e.to_string();
            if last_error.as_deref() != Some(msg.as_str()) {
                error!("Cannot apply the configuration: {msg}");
            }
            (Some(msg), false)
        }
    }
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
