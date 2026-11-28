//! `wanderland`, the wanderlan daemon.
//!
//! It reads `/etc/wanderlan/{network,local}.toml`, derives the desired layer-2
//! setup of this site (bridge, VXLAN tunnels, VLANs) and keeps the system in
//! that state. On SIGTERM/SIGINT it removes everything it created;
//! `wanderland --cleanup` (systemd runs it after every end of the service
//! and before every start) removes whatever a crashed or killed daemon left.

mod api_server;
mod daemon;
mod events;
mod fdb_guard;
mod files;
mod history;
mod host;
mod invites;
mod keepalive;
mod logging;
mod perf;
mod probe;
mod random;
mod rawframe;
mod shared_switch;
mod sockopt;
mod speedtest;
mod stats;
mod sync_net;
mod tuning;

use std::fs::File;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

/// Command line of the daemon.
#[derive(Debug, Parser)]
#[command(name = "wanderland", version = wanderlan_core::version_line(), about)]
pub struct Args {
    /// Directory containing network.toml and local.toml.
    #[arg(long, default_value = "/etc/wanderlan")]
    pub config_dir: PathBuf,
    /// Directory for state (confirmed copies of the configuration).
    #[arg(long, default_value = "/var/lib/wanderlan")]
    pub state_dir: PathBuf,
    /// Socket of the API.
    #[arg(long, env = wanderlan_core::api::SOCKET_ENV, default_value = wanderlan_core::api::SOCKET_PATH)]
    pub socket: PathBuf,
    /// Seconds between periodic reconciliations.
    #[arg(long, default_value_t = 30)]
    pub reconcile_interval: u64,
    /// Run in this terminal. Without it, a start from an interactive shell
    /// only prints a hint: the daemon normally runs as a systemd service.
    #[arg(long)]
    pub foreground: bool,
    /// Remove everything a daemon left behind (bridges, tunnels, rules, the
    /// kernel's GRE fallback devices) and exit. systemd runs it after every
    /// end of the service; refused while a daemon runs.
    #[arg(long)]
    pub cleanup: bool,
}

/// Printed when the daemon is started from a shell, usually by mistake.
const TERMINAL_HINT: &str = "\
wanderland is the wanderlan daemon. It runs as the systemd service `wanderland`
(see `systemctl status wanderland`); it is not meant to be started by hand.

The user interface is `wanderlan` (without d).

To run a daemon in this terminal anyway (for debugging, with the service
stopped), use `wanderland --foreground`.";

fn main() -> ExitCode {
    let args = Args::parse();
    let under_systemd = std::env::var_os("INVOCATION_ID").is_some();
    if !args.foreground && !args.cleanup && !under_systemd && std::io::stdin().is_terminal() {
        eprintln!("{TERMINAL_HINT}");
        return ExitCode::from(2);
    }
    logging::init();
    log_panics();
    // Held until the process ends: a second daemon would steal the API
    // socket and tear down the network of the running one when it stops.
    let _lock = match lock(&args.socket) {
        Ok(lock) => lock,
        Err(e) => {
            tracing::error!("{e:#}");
            eprintln!("{e:#}");
            return ExitCode::FAILURE;
        }
    };
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
    if args.cleanup {
        return match runtime.block_on(daemon::cleanup(&args)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("{e:#}");
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        };
    }
    match runtime.block_on(daemon::run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Panics go to the journal with their location before the default hook
/// prints them; systemd restarts the daemon afterwards.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("Internal error: {info}");
        previous(info);
    }));
}

/// Takes the lock that makes sure only one daemon runs. The lock file lives
/// next to the API socket.
fn lock(socket: &Path) -> anyhow::Result<File> {
    use anyhow::Context as _;
    let dir = socket.parent().unwrap_or_else(|| Path::new("/run"));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join("wanderland.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "wanderland is already running (usually as the systemd service `wanderland`); \
             this one stops without changing anything. The user interface is `wanderlan` (without d)."
        ),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_daemon_cannot_take_the_lock() {
        let dir = std::env::temp_dir().join(format!("wl-lock-{}", std::process::id()));
        let socket = dir.join("wanderlan.sock");
        let first = lock(&socket).unwrap();
        let second = lock(&socket).unwrap_err().to_string();
        assert!(second.contains("already running"), "{second}");
        drop(first);
        let again = lock(&socket);
        assert!(again.is_ok(), "the lock is free again");
        drop(again);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
