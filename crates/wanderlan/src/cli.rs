//! Non-interactive subcommands for scripts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use wanderlan_core::api::{
    self, Applied, ApplyFilesParams, ApplyParams, ConfigInput, ConfigTexts, Confirmed, Done, Event,
    Level, PortState, Request, Status, UnblockParams,
};
use wanderlan_core::config::{self, ConfigFile, Severity};

use crate::client::Client;
use crate::tui::{self, TuiOptions};

const EXAMPLE_MESH: &str = include_str!("../../../packaging/examples/mesh.toml");
const EXAMPLE_LOCAL: &str = include_str!("../../../packaging/examples/local.toml");

/// Command line of `wanderlan`. Without a subcommand the interactive
/// terminal UI starts.
#[derive(Debug, Parser)]
#[command(name = "wanderlan", version = wanderlan_core::VERSION, about)]
pub struct Args {
    /// Socket of the daemon.
    #[arg(long, global = true, env = api::SOCKET_ENV, default_value = api::SOCKET_PATH)]
    pub socket: PathBuf,
    /// Enable mouse support in the terminal UI.
    #[arg(long)]
    pub mouse: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the state of this site and its peers.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check a mesh.toml (and optionally a local.toml) for errors.
    Validate {
        /// The mesh.toml to check.
        file: PathBuf,
        /// A local.toml to check together with the mesh.
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Apply a configuration; it is rolled back unless confirmed in time.
    ///
    /// Without --mesh/--local the files in /etc/wanderlan are applied.
    Apply {
        /// mesh.toml to apply.
        #[arg(long)]
        mesh: Option<PathBuf>,
        /// local.toml to apply (default: the running one).
        #[arg(long)]
        local: Option<PathBuf>,
        /// Time to confirm, e.g. 60s (default: commit_timeout).
        #[arg(long, value_parser = humantime::parse_duration)]
        timeout: Option<Duration>,
    },
    /// Make the pending change permanent.
    Confirm,
    /// Revert the pending change now.
    Rollback,
    /// Write an example mesh.toml and local.toml.
    Init {
        /// Directory to write the files to; prints them if omitted.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
    },
    /// Print the running mesh.toml (e.g. to copy it to a new site).
    Export,
    /// Re-enable a tunnel port shut down by the loop guard.
    Unblock {
        /// Tunnel interface, e.g. gt-haus2.
        port: String,
    },
    /// Show the event log.
    Log {
        /// Keep printing new events.
        #[arg(short, long)]
        follow: bool,
    },
    /// Measure the path MTU to all peers now.
    Probe,
}

/// Runs the selected subcommand.
pub async fn run(args: Args) -> Result<ExitCode> {
    let socket = args.socket.clone();
    match args.command {
        None => {
            tui::run(&socket, TuiOptions { mouse: args.mouse }).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Validate { file, local }) => validate(&file, local.as_deref()),
        Some(Command::Init { dir, force }) => init(dir.as_deref(), force),
        Some(Command::Status { json }) => status(&socket, json).await,
        Some(Command::Apply {
            mesh,
            local,
            timeout,
        }) => apply(&socket, mesh.as_deref(), local.as_deref(), timeout).await,
        Some(Command::Confirm) => {
            let client = connect(&socket).await?;
            let c: Confirmed = client.call(Request::Confirm).await?;
            println!("Confirmed; mesh revision {} is in effect.", c.revision);
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Rollback) => {
            let client = connect(&socket).await?;
            let _: Done = client.call(Request::Rollback).await?;
            println!("Rolled back to the previous configuration.");
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Export) => {
            let client = connect(&socket).await?;
            let cfg: ConfigTexts = client.call(Request::GetConfig).await?;
            if !cfg.configured {
                bail!("this site is not configured yet");
            }
            print!("{}", cfg.mesh);
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Unblock { port }) => {
            let client = connect(&socket).await?;
            let _: Done = client
                .call(Request::Unblock(UnblockParams { port: port.clone() }))
                .await?;
            println!("{port} is enabled again.");
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Log { follow }) => log(&socket, follow).await,
        Some(Command::Probe) => {
            let client = connect(&socket).await?;
            let _: Done = client.call(Request::ProbeNow).await?;
            println!("Measuring the path MTU to all peers; see `wanderlan status`.");
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn connect(socket: &Path) -> Result<Client> {
    Ok(Client::connect(socket).await?.0)
}

fn validate(file: &Path, local: Option<&Path>) -> Result<ExitCode> {
    let mesh_text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let local_text = local
        .map(|p| std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display())))
        .transpose()?;
    let diags = config::check(&mesh_text, local_text.as_deref());
    for d in &diags {
        let label = match (d.file, local) {
            (ConfigFile::Local, Some(p)) => p.display().to_string(),
            _ => file.display().to_string(),
        };
        eprintln!("{}", d.display_with(&label));
    }
    let errors = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    if errors == 0 {
        println!("{}: configuration is valid", file.display());
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("{errors} error(s) found");
        Ok(ExitCode::FAILURE)
    }
}

fn init(dir: Option<&Path>, force: bool) -> Result<ExitCode> {
    let Some(dir) = dir else {
        println!("# ---- mesh.toml ----\n{EXAMPLE_MESH}\n# ---- local.toml ----\n{EXAMPLE_LOCAL}");
        return Ok(ExitCode::SUCCESS);
    };
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for (name, content) in [("mesh.toml", EXAMPLE_MESH), ("local.toml", EXAMPLE_LOCAL)] {
        let path = dir.join(name);
        if path.exists() && !force {
            bail!(
                "{} already exists (use --force to overwrite)",
                path.display()
            );
        }
        std::fs::write(&path, content)
            .with_context(|| format!("cannot write {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

async fn apply(
    socket: &Path,
    mesh: Option<&Path>,
    local: Option<&Path>,
    timeout: Option<Duration>,
) -> Result<ExitCode> {
    let client = connect(socket).await?;
    let timeout_secs = timeout.map(|t| t.as_secs().max(1));
    let request = if mesh.is_none() && local.is_none() {
        Request::ApplyFiles(ApplyFilesParams { timeout_secs })
    } else {
        let running: ConfigTexts = client.call(Request::GetConfig).await?;
        let read = |p: &Path| {
            std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))
        };
        let mesh_text = match mesh {
            Some(p) => read(p)?,
            None => running.mesh.clone(),
        };
        let local_text = match local {
            Some(p) => read(p)?,
            None => running.local.clone(),
        };
        Request::Apply(ApplyParams {
            config: ConfigInput {
                mesh: mesh_text,
                local: local_text,
            },
            base_revision: Some(running.revision),
            timeout_secs,
        })
    };
    match client.call::<Applied>(request).await {
        Ok(applied) => {
            for w in &applied.warnings {
                eprintln!("{w}");
            }
            if applied.diff.is_empty() {
                println!("The configuration is unchanged.");
            } else {
                print!("{}", applied.diff);
            }
            for change in &applied.changes {
                println!("  {change}");
            }
            println!(
                "Applied. Run `wanderlan confirm` within {} seconds, otherwise the previous configuration is restored.",
                applied.confirm_within_secs
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(crate::client::ClientError::Api(e)) if !e.diagnostics.is_empty() => {
            for d in &e.diagnostics {
                eprintln!("{d}");
            }
            eprintln!("{}", e.message);
            Ok(ExitCode::FAILURE)
        }
        Err(e) => Err(e.into()),
    }
}

async fn status(socket: &Path, json: bool) -> Result<ExitCode> {
    let client = connect(socket).await?;
    let st: Status = client.call(Request::Status).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(ExitCode::SUCCESS);
    }
    match &st.site {
        Some(site) => println!(
            "Site {site} ({}), address {}, trunk {}, VLANs {}",
            st.role.map_or("?", |r| r.as_str()),
            st.address.as_deref().unwrap_or("?"),
            st.trunk.as_deref().unwrap_or("?"),
            st.vlans.as_deref().unwrap_or("?"),
        ),
        None => println!("This site is not configured yet; run `wanderlan` to set it up."),
    }
    println!("Mesh revision {} ({})", st.revision, short(&st.fingerprint));
    if let Some(p) = &st.pending {
        println!(
            "PENDING CHANGE: rolled back in {} s unless confirmed (`wanderlan confirm`)",
            p.remaining_secs
        );
    }
    if let Some(e) = &st.last_error {
        println!("ERROR: {e}");
    }
    if st.peers.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "\n{:<12} {:<5} {:<15} {:<8} {:>7} {:>8} {:>5} {:>5} {:>5} {:>10} {:>10}",
        "PEER", "ROLE", "ADDRESS", "PORT", "REPLY", "RTT", "PMTU", "MSS", "MACS", "RX", "TX"
    );
    for p in &st.peers {
        println!(
            "{:<12} {:<5} {:<15} {:<8} {:>7} {:>8} {:>5} {:>5} {:>5} {:>10} {:>10}",
            p.name,
            p.role.as_str(),
            p.address,
            port_state(p.port_state),
            p.last_reply_secs.map_or("-".into(), |s| format!("{s}s")),
            p.rtt_ms.map_or("-".into(), |r| format!("{r:.1}ms")),
            p.pmtu.map_or("-".into(), |v| v.to_string()),
            p.mss.map_or("-".into(), |v| v.to_string()),
            p.macs,
            rate(p.rx_bps),
            rate(p.tx_bps),
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn short(fingerprint: &str) -> &str {
    fingerprint.get(..12).unwrap_or(fingerprint)
}

fn port_state(s: PortState) -> &'static str {
    match s {
        PortState::Up => "up",
        PortState::Down => "down",
        PortState::Blocked => "BLOCKED",
        PortState::Missing => "missing",
    }
}

/// Formats a bit rate.
pub fn rate(bps: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let v = bps as f64;
    if v >= 1e9 {
        format!("{:.1}Gbit/s", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.1}Mbit/s", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1}kbit/s", v / 1e3)
    } else {
        format!("{bps}bit/s")
    }
}

async fn log(socket: &Path, follow: bool) -> Result<ExitCode> {
    let (client, mut events) = Client::connect(socket).await?;
    let history: Vec<Event> = client.call(Request::Subscribe).await?;
    for e in &history {
        print_event(e);
    }
    if follow {
        while let Some(e) = events.recv().await {
            print_event(&e);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_event(e: &Event) {
    let level = match e.level {
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    };
    let time = std::time::UNIX_EPOCH + Duration::from_millis(e.time_ms);
    let stamp = humantime::format_rfc3339_seconds(time);
    match &e.peer {
        Some(peer) => println!("{stamp} {level:<5} [{peer}] {}", e.message),
        None => println!("{stamp} {level:<5} {}", e.message),
    }
}
