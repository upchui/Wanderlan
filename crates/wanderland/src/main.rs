//! `wanderland`, the wanderlan daemon.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Parser;
use tracing::{info, warn};
use wanderlan_core::config::{self, Loaded};

/// Command line of the daemon.
#[derive(Debug, Parser)]
#[command(name = "wanderland", version = wanderlan_core::VERSION, about)]
struct Args {
    /// Directory containing mesh.toml and local.toml.
    #[arg(long, default_value = "/etc/wanderlan")]
    config_dir: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();

    let mesh_path = args.config_dir.join("mesh.toml");
    let local_path = args.config_dir.join("local.toml");
    let mesh_text = read_optional(&mesh_path)?;
    let local_text = read_optional(&local_path)?;
    match config::load(&mesh_text, &local_text) {
        Ok(outcome) => {
            for w in &outcome.warnings {
                warn!("{w}");
            }
            match outcome.loaded {
                Loaded::Unconfigured { .. } => {
                    info!("This site is not configured yet; run `wanderlan` to start the setup")
                }
                Loaded::Configured(cfg) => info!(
                    "Loaded configuration for site \"{}\" ({} sites, VLANs {})",
                    cfg.self_name(),
                    cfg.mesh.sites.len(),
                    cfg.mesh.vlans
                ),
            }
            Ok(())
        }
        Err(diags) => {
            for d in &diags {
                let label = match d.file {
                    config::ConfigFile::Mesh => mesh_path.display().to_string(),
                    config::ConfigFile::Local => local_path.display().to_string(),
                };
                tracing::error!("{}", d.display_with(&label));
            }
            anyhow::bail!("the configuration is invalid ({} problems)", diags.len())
        }
    }
}

/// Reads a file, treating a missing file as empty.
fn read_optional(path: &std::path::Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}
