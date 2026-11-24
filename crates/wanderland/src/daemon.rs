//! Main loop of the daemon.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use wanderlan_core::config::{self, Config, ConfigFile, Loaded};
use wanderlan_core::desired;
use wanderlan_core::reconcile;
use wanderlan_core::sys::netlink::Netlink;

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

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Some(cfg) = &current {
                    last_error = apply(&net, cfg, last_error).await;
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

/// Reconciles once. Logs errors only when they change, so a missing trunk
/// does not flood the log every 30 seconds.
async fn apply(net: &Netlink, cfg: &Config, last_error: Option<String>) -> Option<String> {
    let desired = desired::derive(cfg, &BTreeSet::new());
    match reconcile::reconcile(net, &desired, |c| info!("{c}")).await {
        Ok(changes) => {
            if last_error.is_some() || !changes.is_empty() {
                info!(
                    "Site \"{}\" is up to date ({} tunnels, VLANs {})",
                    cfg.self_name(),
                    desired.tunnels.len(),
                    cfg.mesh.vlans
                );
            }
            None
        }
        Err(e) => {
            let msg = e.to_string();
            if last_error.as_deref() != Some(msg.as_str()) {
                error!("Cannot apply the configuration: {msg}");
            }
            Some(msg)
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
