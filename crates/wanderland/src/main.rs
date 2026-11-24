//! `wanderland`, the wanderlan daemon.
//!
//! It reads `/etc/wanderlan/{mesh,local}.toml`, derives the desired layer-2
//! setup of this site (bridge, GRETAP tunnels, VLANs) and keeps the system in
//! that state. On SIGTERM/SIGINT it removes everything it created.

mod daemon;
mod logging;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Command line of the daemon.
#[derive(Debug, Parser)]
#[command(name = "wanderland", version = wanderlan_core::VERSION, about)]
pub struct Args {
    /// Directory containing mesh.toml and local.toml.
    #[arg(long, default_value = "/etc/wanderlan")]
    pub config_dir: PathBuf,
    /// Seconds between periodic reconciliations.
    #[arg(long, default_value_t = 30)]
    pub reconcile_interval: u64,
}

fn main() -> ExitCode {
    let args = Args::parse();
    logging::init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("Cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(daemon::run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
