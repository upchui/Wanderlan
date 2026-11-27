//! Non-interactive subcommands for scripts.

use std::io::{BufRead as _, Write as _};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use wanderlan_core::api::{
    self, Applied, ApplyFilesParams, ApplyParams, ChangeSource, ConfigInput, ConfigTexts,
    Confirmed, Done, Event, Interface, Invitation, InviteParams, JoinParams, Joined, Level,
    MacEntry, PortState, Request, ResetReport, SiteRules, SiteState, Status, UnblockParams,
    VlanPortTraffic, VlanTraffic,
};
use wanderlan_core::config::{self, ConfigFile, Severity};
use wanderlan_core::identity::SiteId;
use wanderlan_core::join;

use crate::client::Client;
use crate::hostinfo;
use crate::tui::{self, TuiOptions};

const EXAMPLE_NETWORK: &str = include_str!("../../../packaging/examples/network.toml");
const EXAMPLE_LOCAL: &str = include_str!("../../../packaging/examples/local.toml");

/// Environment variable that chooses the network ([`Args::net`]).
const NET_ENV: &str = "WANDERLAN_NET";

/// The network chosen on the command line (`None`: the default network).
static NETWORK: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The network the requests are for.
fn chosen_network() -> Option<String> {
    NETWORK
        .get()
        .cloned()
        .flatten()
        .filter(|n| n != api::DEFAULT_NETWORK)
}

/// Command line of `wanderlan`. Without a subcommand the interactive
/// terminal UI starts.
#[derive(Debug, Parser)]
#[command(name = "wanderlan", version = wanderlan_core::version_line(), about)]
pub struct Args {
    /// Socket of the daemon.
    #[arg(long, global = true, env = api::SOCKET_ENV, default_value = api::SOCKET_PATH)]
    pub socket: PathBuf,
    /// The network of this box to work on (`wanderlan networks` lists
    /// them); without it the default network.
    #[arg(long = "net", global = true, env = NET_ENV, value_name = "NAME")]
    pub net: Option<String>,
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
    /// Check a network.toml (and optionally a local.toml) for errors.
    Validate {
        /// The network.toml to check.
        file: PathBuf,
        /// A local.toml to check together with the network.
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Apply a configuration; it is rolled back unless confirmed in time.
    ///
    /// Without --network/--local the files in /etc/wanderlan are applied.
    Apply {
        /// network.toml to apply.
        #[arg(long)]
        network: Option<PathBuf>,
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
    /// Write an example network.toml and local.toml.
    Init {
        /// Directory to write the files to; prints them if omitted.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
    },
    /// Print the running network.toml (on a client: its part of the network).
    Export,
    /// Show the live traffic of one VLAN on every port of this site and
    /// the MAC addresses in it.
    Vlan {
        /// The VLAN (shared ID), e.g. 20, or `native` (the untagged traffic).
        vid: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Re-enable a tunnel port shut down by the loop guard.
    Unblock {
        /// Tunnel interface, e.g. vx-haus2.
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
    /// Invite a new site: prints the command to run there.
    ///
    /// Works on every site of the network. The code can be used once and
    /// stays valid until it is used or deleted.
    Invite {
        #[command(subcommand)]
        action: Option<InviteAction>,
        #[command(flatten)]
        rules: RuleArgs,
    },
    /// Take a site online: its tunnels come up (sites that joined with an
    /// invitation start offline).
    Online {
        /// The site.
        site: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Take a site offline: its tunnels go down until it is online again.
    Offline {
        /// The site.
        site: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Set who changes the tunnel of a client to its server: its VLANs,
    /// mappings, the VLANs of that tunnel, breakouts and online state.
    /// "server": only its server, "client": only the client, "both". Its
    /// address, its server and removing it stay with its server.
    ///
    /// Example: `wanderlan manage buero both` lets "buero" change its tunnel
    /// too.
    Manage {
        /// The site (a client).
        site: String,
        /// "server", "client" or "both".
        #[arg(value_parser = ["server", "client", "both"])]
        changes: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Rename a site: this site always, a client of this site too.
    /// Everything that names it follows, on every site.
    ///
    /// Example: `wanderlan rename site5 lab`.
    Rename {
        /// The site.
        site: String,
        /// Its new name (a-z, 0-9 and -, at most 12 characters).
        new_name: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Make a site a server (connected with the other servers) or a client
    /// of another site (its only tunnel leads there).
    ///
    /// Examples: `wanderlan role marcel client alex`, `wanderlan role dc server`.
    Role {
        /// The site.
        site: String,
        /// "server" or "client".
        #[arg(value_parser = ["server", "client"])]
        role: String,
        /// For "client": the site it is plugged into.
        uplink: Option<String>,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Set the own VLANs of a site: on its trunk and towards its own clients
    /// only, never towards its server. A site may always set its own.
    ///
    /// Example: `wanderlan own-vlans site2 50,60-70`, `wanderlan own-vlans site2 none`.
    #[command(name = "own-vlans")]
    OwnVlans {
        /// The site.
        site: String,
        /// VLANs, e.g. 50,60-70, or "none".
        vlans: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// The local internet breakout of VLANs of a client: their public IPv4
    /// and IPv6 destinations leave through that site's own uplink
    /// (masqueraded) instead of the tunnel to its server; private
    /// destinations and the exceptions still go through the tunnel. "off":
    /// all through the tunnel again. A server breaks out nothing (the
    /// gateways are there). Without arguments: the breakouts of this site.
    ///
    /// Examples: `wanderlan breakout office 10,20 on`,
    /// `wanderlan breakout office 20 on --uplink ens20 --except 203.0.113.0/24`,
    /// `wanderlan breakout office 20 off`.
    Breakout {
        /// The site (none: show the breakouts of this site).
        site: Option<String>,
        /// VLANs (in the network), e.g. 10,20 or 10-20.
        vlans: Option<String>,
        /// "on" or "off".
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
        /// The interface of that site the VLANs leave by; "auto": the one
        /// of its default route.
        #[arg(long)]
        uplink: Option<String>,
        /// Destinations that still go through the tunnel, e.g.
        /// 203.0.113.0/24,2001:db8::/32; "none": no exceptions.
        #[arg(long)]
        except: Option<String>,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Limit the VLANs of the tunnel between two sites (on top of the VLANs
    /// both take part in), or lift the limit ("all"). "none": the tunnel
    /// carries no VLAN.
    ///
    /// Example: `wanderlan tunnel-vlans server2 server3 30-40`.
    #[command(name = "tunnel-vlans")]
    TunnelVlans {
        /// One end of the tunnel.
        site: String,
        /// The other end.
        other: String,
        /// VLANs, e.g. 30-40; "all" (no limit) or "none".
        vlans: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Sites by hand: add one, remove one, or set its VLANs. A new site
    /// usually joins with an invitation instead (`wanderlan invite`).
    ///
    /// Examples: `wanderlan site add lab 10.8.0.5 --server hq --vlans 10,20`,
    /// `wanderlan site vlans lab default`, `wanderlan site remove lab`.
    Site {
        #[command(subcommand)]
        action: SiteAction,
    },
    /// Set the VLANs of the network: every site takes part in them unless
    /// it has VLANs of its own (`wanderlan site vlans`).
    ///
    /// Example: `wanderlan vlans native,10-50`, `wanderlan vlans all`.
    Vlans {
        /// VLANs, e.g. native,10-50, or "all" (every VLAN and the untagged
        /// traffic).
        vlans: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// VLAN mappings: a VLAN of the network (the source VLAN) has another
    /// ID on the trunk of one site (the mapped VLAN), or is untagged there
    /// ("native").
    ///
    /// Examples: `wanderlan map add lab 30 native`,
    /// `wanderlan map add office 20 120`, `wanderlan map del lab 30`.
    Map {
        #[command(subcommand)]
        action: MapAction,
    },
    /// The settings of this site: shown, or one of them changed. They change
    /// this site only; every site sets its own.
    ///
    /// Example: `wanderlan settings`, `wanderlan settings commit-timeout 2m`,
    /// `wanderlan settings control-port 52821 --confirm`.
    Settings {
        /// The setting to change; without it all are shown.
        setting: Option<SettingName>,
        /// Its value, or "default" (the network's).
        value: Option<String>,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// The trunk ports: of the tunnel to a site (a segment of its own on this
    /// site; "default": the default trunk again), or with this site's own
    /// name the ports of the default trunk. Several ports separated by
    /// commas are bridged like the ports of a switch.
    ///
    /// Example: `wanderlan trunk lab ens21,ens22`, `wanderlan trunk hq ens19,ens20`.
    Trunk {
        /// The site at the other end of the tunnel, or this site.
        site: String,
        /// Local interfaces (e.g. ens20 or ens20,ens21), or "default".
        interface: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Join an existing network with an invitation.
    ///
    /// Run on a new site: `wanderlan join <address> <code>` as printed by
    /// `wanderlan invite` on any site of the network.
    Join {
        /// Address of the site that created the invitation (`ip` or `ip:port`).
        target: String,
        /// The invitation code.
        code: String,
        /// Name of this site (default: the host name).
        #[arg(long)]
        name: Option<String>,
        /// Interface carrying the VLANs (default: the only interface that
        /// does not lead to the inviting site).
        #[arg(long)]
        trunk: Option<String>,
    },
    /// Remove everything wanderlan set up on this site: bridge, tunnels,
    /// rules, configuration and invitations. The site is unconfigured again.
    Reset {
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// The networks of this box: one box can take part in several
    /// networks, each with its own sites, identity and ports.
    Networks {
        #[command(subcommand)]
        action: Option<NetworksAction>,
    },
}

/// What `wanderlan networks` does.
#[derive(Debug, Subcommand)]
pub enum NetworksAction {
    /// List the networks (the default).
    List,
    /// Add a network, unconfigured: then `wanderlan --net <name>` (the
    /// setup wizard starts it) or `wanderlan --net <name> join ...`.
    Add {
        /// Its name: lowercase letters, digits and -.
        name: String,
    },
    /// Reset a network on this box and remove it.
    Remove {
        /// Its name.
        name: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

/// Rules for the site that joins with a new invitation.
#[derive(Debug, clap::Args)]
pub struct RuleArgs {
    /// Name of the new site (default: its host name).
    #[arg(long)]
    name: Option<String>,
    /// VLANs of the new site, e.g. native,10-50 (default: the network's).
    #[arg(long)]
    vlans: Option<String>,
    /// Source VLAN for the untagged traffic of the new site.
    #[arg(long)]
    native: Option<u16>,
    /// Mapping MAPPED:SOURCE, e.g. 130:30: VLAN 130 on the new site's trunk
    /// is source VLAN 30 (repeatable).
    #[arg(long = "map", value_name = "MAPPED:SOURCE", value_parser = parse_translation)]
    maps: Vec<api::Translation>,
    /// Who changes the tunnel of the new site later: "server" (only this
    /// site, the default), "client" (only the new site) or "both".
    #[arg(long, value_parser = ["server", "client", "both"])]
    changes: Option<String>,
}

impl RuleArgs {
    fn into_rules(self) -> SiteRules {
        SiteRules {
            name: self.name,
            vlans: self.vlans,
            native: self.native,
            translations: self.maps,
            changes: self.changes.as_deref().and_then(|c| c.parse().ok()),
            ..SiteRules::default()
        }
    }
}

fn parse_translation(text: &str) -> Result<api::Translation, String> {
    let (local, shared) = text
        .split_once(':')
        .ok_or_else(|| format!("\"{text}\" is not MAPPED:SOURCE, e.g. 130:30"))?;
    let vid = |v: &str| {
        v.trim()
            .parse::<u16>()
            .ok()
            .filter(|v| (1..=4094).contains(v))
            .ok_or_else(|| format!("\"{v}\" is not a VLAN ID (1-4094)"))
    };
    Ok(api::Translation {
        local: vid(local)?,
        shared: vid(shared)?,
    })
}

/// What `wanderlan site` does.
#[derive(Debug, Subcommand)]
pub enum SiteAction {
    /// Add a site by hand: online right away and managed by this site.
    Add {
        /// Its name (a-z, 0-9 and -, at most 12 characters).
        name: String,
        /// The IPv4 address the other sites reach it at.
        address: Ipv4Addr,
        /// The site it is a client of (its only tunnel leads there); "none":
        /// a server. Default: this site.
        #[arg(long)]
        server: Option<String>,
        /// Its VLANs, e.g. 10,20; default: those of the network.
        #[arg(long)]
        vlans: Option<String>,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Remove a site with its VLAN mappings and tunnel limits (not while
    /// it has clients).
    #[command(alias = "rm", alias = "delete")]
    Remove {
        /// The site.
        name: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Set the VLANs of a site; "default": those of the network.
    Vlans {
        /// The site.
        name: String,
        /// VLANs, e.g. 10,20 or native,30-40, or "default".
        vlans: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
}

/// What `wanderlan map` does.
#[derive(Debug, Subcommand)]
pub enum MapAction {
    /// Add a mapping, or change the mapped VLAN of a source VLAN.
    Add {
        /// The site whose trunk uses another VLAN ID.
        site: String,
        /// The VLAN in the network (one of the VLANs of the site).
        #[arg(value_parser = clap::value_parser!(u16).range(1..=4094))]
        source: u16,
        /// Its ID on the trunk of the site, or "native" (untagged there).
        mapped: String,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Delete the mapping of a source VLAN at a site.
    #[command(alias = "delete", alias = "remove", alias = "rm")]
    Del {
        /// The site.
        site: String,
        /// The source VLAN of the mapping.
        #[arg(value_parser = clap::value_parser!(u16).range(1..=4094))]
        source: u16,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
}

/// What `wanderlan invite` does.
#[derive(Debug, Subcommand)]
pub enum InviteAction {
    /// List the open invitations of this site.
    List,
    /// Delete an open invitation.
    Delete {
        /// The code.
        code: String,
    },
}

/// Runs the selected subcommand.
pub async fn run(args: Args) -> Result<ExitCode> {
    let socket = args.socket.clone();
    if let Some(name) = &args.net
        && name != api::DEFAULT_NETWORK
        && let Some(e) = api::network_name_error(name)
    {
        bail!("--net: {e}");
    }
    let _ = NETWORK.set(args.net.clone());
    match args.command {
        None => {
            // Whether the box has the network, before the screen changes.
            let network = chosen_network();
            if network.is_some() {
                connect(&socket).await?;
            }
            tui::run(
                &socket,
                TuiOptions {
                    mouse: args.mouse,
                    network,
                },
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Validate { file, local }) => validate(&file, local.as_deref()),
        Some(Command::Init { dir, force }) => init(dir.as_deref(), force),
        Some(Command::Status { json }) => status(&socket, json).await,
        Some(Command::Apply {
            network,
            local,
            timeout,
        }) => apply(&socket, network.as_deref(), local.as_deref(), timeout).await,
        Some(Command::Confirm) => {
            let client = connect(&socket).await?;
            let c: Confirmed = client.call(Request::Confirm).await?;
            println!("Confirmed; network revision {} is in effect.", c.revision);
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
            print!("{}", cfg.network);
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
        Some(Command::Vlan { vid, json }) => vlan(&socket, &vid, json).await,
        Some(Command::Probe) => {
            let client = connect(&socket).await?;
            let _: Done = client.call(Request::ProbeNow).await?;
            println!("Measuring the path MTU to all peers; see `wanderlan status`.");
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Invite { action, rules }) => invite(&socket, action, rules).await,
        Some(Command::Online { site, confirm }) => set_online(&socket, &site, true, confirm).await,
        Some(Command::Offline { site, confirm }) => {
            set_online(&socket, &site, false, confirm).await
        }
        Some(Command::Role {
            site,
            role,
            uplink,
            confirm,
        }) => {
            let uplink = match (role.as_str(), uplink) {
                ("server", None) => None,
                ("server", Some(_)) => {
                    bail!("a server is a client of no site: wanderlan role {site} server")
                }
                (_, Some(up)) => Some(up),
                (_, None) => {
                    bail!("name the site it is a client of: wanderlan role {site} client <site>")
                }
            };
            edit_site(&socket, &site, confirm, |entry, known| {
                let uplink = uplink.as_deref().map(|up| known.find(up)).transpose()?;
                let role = role_text(uplink.map(|up| known.label(up)).as_deref(), false);
                if entry.uplink == uplink {
                    return Ok(Edit::Nothing(format!(
                        "{} is already a {role}.",
                        entry.name
                    )));
                }
                entry.uplink = uplink;
                // A server has no option of who changes its tunnel.
                if uplink.is_none() {
                    entry.changes = config::Changes::default();
                }
                // A server breaks out nothing: the gateways are there.
                let gone = if uplink.is_none() && !entry.breakout.is_empty() {
                    entry.breakout.clear();
                    "; its local breakout goes (a server has none)"
                } else {
                    ""
                };
                Ok(Edit::Done(format!("{} is a {role}{gone}", entry.name)))
            })
            .await
        }
        Some(Command::Trunk {
            site,
            interface,
            confirm,
        }) => set_trunk(&socket, &site, &interface, confirm).await,
        Some(Command::Settings {
            setting,
            value,
            confirm,
        }) => settings(&socket, setting, value.as_deref(), confirm).await,
        Some(Command::OwnVlans {
            site,
            vlans,
            confirm,
        }) => {
            let own = match vlans.trim() {
                "" | "none" => None,
                text => Some(
                    text.parse::<config::VlanSet>()
                        .map_err(|e| anyhow!("{text}: {e}"))?,
                ),
            };
            edit_site(&socket, &site, confirm, |entry, _| {
                if entry.own_vlans == own {
                    return Ok(Edit::Nothing(format!(
                        "{} already has these own VLANs.",
                        entry.name
                    )));
                }
                entry.own_vlans.clone_from(&own);
                Ok(Edit::Done(match &own {
                    Some(v) => format!("{} has the own VLANs {v}", entry.name),
                    None => format!("{} has no own VLANs", entry.name),
                }))
            })
            .await
        }
        Some(Command::Site { action }) => site_command(&socket, action).await,
        Some(Command::Map { action }) => map_command(&socket, action).await,
        Some(Command::Vlans { vlans, confirm }) => {
            let vlans = match vlans.trim() {
                "" | "all" => config::VlanSet::all(),
                text => text.parse().map_err(|e| anyhow!("{text}: {e}"))?,
            };
            edit_network(&socket, confirm, |network, _| {
                if network.vlans == vlans {
                    return Ok(Edit::Nothing(format!(
                        "The network has the VLANs {vlans} already."
                    )));
                }
                network.vlans = vlans.clone();
                Ok(Edit::Done(format!("The network has the VLANs {vlans}")))
            })
            .await
        }
        Some(Command::TunnelVlans {
            site,
            other,
            vlans,
            confirm,
        }) => set_tunnel_vlans(&socket, &site, &other, &vlans, confirm).await,
        Some(Command::Breakout {
            site,
            vlans,
            state,
            uplink,
            except,
            confirm,
        }) => {
            let (Some(site), Some(vlans), Some(state)) = (site, vlans, state) else {
                return show_breakout(&socket).await;
            };
            let vlans: config::VlanSet = vlans.parse().map_err(|e| anyhow!("{vlans}: {e}"))?;
            let except = except.map(|text| parse_except(&text)).transpose()?;
            let uplink = uplink.map(|u| {
                let u = u.trim().to_string();
                (!u.is_empty() && u != "auto").then_some(u)
            });
            edit_network(&socket, confirm, |network, known| {
                let id = known.find(&site)?;
                let Some(entry) = network.site_mut(id) else {
                    bail!("there is no site \"{site}\"");
                };
                // A server is where the gateways are: it breaks out nothing.
                if state != "off" && entry.uplink.is_none() {
                    bail!(
                        "{0} is a server: its devices reach the internet through their own gateway; only clients break out (wanderlan role {0} client <site> makes it one)",
                        entry.name
                    );
                }
                let before = entry.breakout.clone();
                for vid in vlans.iter() {
                    if state == "off" {
                        entry.breakout.remove(&vid);
                        continue;
                    }
                    let b = entry.breakout.entry(vid).or_default();
                    if let Some(uplink) = &uplink {
                        b.uplink.clone_from(uplink);
                    }
                    if let Some(except) = &except {
                        b.except.clone_from(except);
                    }
                }
                if entry.breakout == before {
                    return Ok(Edit::Nothing(format!(
                        "Nothing to change for {}.",
                        entry.name
                    )));
                }
                Ok(Edit::Done(if entry.breakout.is_empty() {
                    format!("{} has no local breakout", entry.name)
                } else {
                    let list: config::VlanSet = entry.breakout.keys().copied().collect();
                    format!(
                        "{} breaks out VLANs {list} to the internet through its own uplink",
                        entry.name
                    )
                }))
            })
            .await
        }
        Some(Command::Rename {
            site,
            new_name,
            confirm,
        }) => rename(&socket, &site, &new_name, confirm).await,
        Some(Command::Manage {
            site,
            changes,
            confirm,
        }) => {
            let changes: config::Changes = changes.parse().map_err(|e: String| anyhow!(e))?;
            edit_site(&socket, &site, confirm, |entry, known| {
                let Some(server) = entry.uplink else {
                    bail!(
                        "\"{}\" is a server: it has no such option, every server may change it",
                        entry.name
                    );
                };
                let who = match changes {
                    config::Changes::Server => {
                        format!("only its server \"{}\" changes", known.label(server))
                    }
                    config::Changes::Client => format!("only \"{}\" itself changes", entry.name),
                    config::Changes::Both => format!(
                        "its server \"{}\" and \"{}\" itself change",
                        known.label(server),
                        entry.name
                    ),
                };
                let who = format!("{who} its tunnel (VLANs, mappings, breakouts, online state)");
                if entry.changes == changes {
                    return Ok(Edit::Nothing(format!("Nothing to change: {who}.")));
                }
                entry.changes = changes;
                Ok(Edit::Done(format!("{}: {who}", entry.name)))
            })
            .await
        }
        Some(Command::Join {
            target,
            code,
            name,
            trunk,
        }) => {
            let added = add_chosen_network(&socket).await?;
            let joined = join_network(&socket, &target, &code, name, trunk).await;
            // A network added for a join that did not work goes again.
            if joined.is_err()
                && let Some(name) = added
            {
                let client = Client::connect(&socket).await?.0;
                let removed: Result<ResetReport, _> = client
                    .call(Request::RemoveNetwork(api::NetworkParams {
                        name: name.clone(),
                    }))
                    .await;
                if removed.is_ok() {
                    eprintln!("Network {name} was removed from this box again.");
                }
            }
            joined
        }
        Some(Command::Reset { yes }) => reset(&socket, yes).await,
        Some(Command::Networks { action }) => networks(&socket, action).await,
    }
}

/// `wanderlan networks`.
async fn networks(socket: &Path, action: Option<NetworksAction>) -> Result<ExitCode> {
    let client = Client::connect(socket).await?.0;
    match action.unwrap_or(NetworksAction::List) {
        NetworksAction::List => {
            let list: Vec<api::NetworkInfo> = client.call(Request::Networks).await?;
            for n in &list {
                println!(
                    "{:<12}  tag {}  {}",
                    n.name,
                    n.tag,
                    n.site
                        .as_deref()
                        .map_or_else(|| "not configured".to_string(), |s| format!("site {s}"))
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        NetworksAction::Add { name } => {
            let n: api::NetworkInfo = client
                .call(Request::AddNetwork(api::NetworkParams { name }))
                .await?;
            println!(
                "Network {} added (tag {}). Start it with `wanderlan --net {0}` or join with `wanderlan --net {0} join <address> <code>`.",
                n.name, n.tag
            );
            Ok(ExitCode::SUCCESS)
        }
        NetworksAction::Remove { name, yes } => {
            if !yes {
                println!(
                    "This resets network {name} on this box (its tunnels, rules, configuration,\n\
                     identity and invitations here) and removes it."
                );
                print!("Type \"remove\" to continue: ");
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().lock().read_line(&mut answer)?;
                if answer.trim() != "remove" {
                    println!("Nothing was changed.");
                    return Ok(ExitCode::FAILURE);
                }
            }
            let report: ResetReport = client
                .call(Request::RemoveNetwork(api::NetworkParams {
                    name: name.clone(),
                }))
                .await?;
            if report.problems.is_empty() {
                println!("Network {name} was removed ({} changes).", report.removed);
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!(
                    "Network {name} was removed, but: {}",
                    report.problems.join("; ")
                );
                Ok(ExitCode::FAILURE)
            }
        }
    }
}

async fn invite(socket: &Path, action: Option<InviteAction>, rules: RuleArgs) -> Result<ExitCode> {
    let client = connect(socket).await?;
    match action {
        None => {
            let inv: Invitation = client.call(Request::Invite(rules.into_rules())).await?;
            println!("Invitation {} created. On the new site run:\n", inv.code);
            println!("    {}\n", inv.command);
            println!("Rules for the new site: {}.", inv.rules.summary());
            println!("It starts offline; take it online with `wanderlan online <site>`.");
            println!("The code works once and stays valid until it is used or deleted");
            println!("(`wanderlan invite delete {}`).", inv.code);
        }
        Some(InviteAction::List) => {
            let list: Vec<Invitation> = client.call(Request::Invites).await?;
            if list.is_empty() {
                println!("No open invitations.");
            }
            for inv in &list {
                let created = humantime::format_rfc3339_seconds(
                    std::time::UNIX_EPOCH + Duration::from_secs(inv.created_secs),
                );
                let state = if inv.in_use {
                    "  (a site is joining with it)"
                } else {
                    ""
                };
                println!("{}  created {created}  {}{state}", inv.code, inv.command);
                println!("             rules: {}", inv.rules.summary());
            }
        }
        Some(InviteAction::Delete { code }) => {
            let _: Done = client
                .call(Request::DeleteInvite(InviteParams { code: code.clone() }))
                .await?;
            println!("Invitation {code} deleted.");
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn join_network(
    socket: &Path,
    target: &str,
    code: &str,
    name: Option<String>,
    trunk: Option<String>,
) -> Result<ExitCode> {
    let client = connect(socket).await?;
    let (address, port) =
        join::parse_target(target, config::DEFAULT_CONTROL_PORT).map_err(|e| anyhow!(e))?;
    let (name, name_is_default) = match name {
        Some(n) => (n, false),
        None => (join::site_name_from_hostname(&hostinfo::hostname()), true),
    };
    let trunk = match trunk {
        Some(t) => t,
        None => {
            let interfaces: Vec<Interface> = client.call(Request::Interfaces).await?;
            let transport = hostinfo::route_interface(address);
            hostinfo::choose_trunk(&interfaces, transport.as_deref()).map_err(|candidates| {
                let via = transport.map_or_else(String::new, |t| format!(" ({t} leads to {address})"));
                if candidates.is_empty() {
                    anyhow!("no interface is left for the trunk{via}; give it with --trunk <interface>")
                } else {
                    anyhow!(
                        "more than one interface could be the trunk: {}{via}; choose one with --trunk <interface>",
                        candidates.join(", ")
                    )
                }
            })?
        }
    };
    println!("Joining via {target} as \"{name}\", trunk {trunk} ...");
    let joined: Joined = client
        .call(Request::Join(JoinParams {
            target: target.to_string(),
            code: code.to_string(),
            name,
            name_is_default,
            trunk,
        }))
        .await
        .map_err(|e| {
            let text = e.to_string();
            if text.contains("choose another name") {
                anyhow!("{text} (`wanderlan join {target} {code} --name <name>`)")
            } else {
                anyhow!(e)
            }
        })?;
    println!(
        "Received the network configuration from \"{}\": this site is \"{}\", {} sites.",
        joined.via, joined.site, joined.sites
    );
    println!("Waiting until \"{}\" answers ...", joined.via);
    // As long as the daemon waits for the confirmation (the commit timeout
    // of the network), and a little more.
    let mut deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let st: Status = client.call(Request::Status).await?;
        if st.state == SiteState::Unconfigured {
            bail!(
                "\"{}\" did not answer in time; the change was rolled back. Check that this site reaches it in the transport network (TCP and UDP {port}, the tunnels UDP {}) and join again (the invitation works again).",
                joined.via,
                port.saturating_add(1)
            );
        }
        if let Some(p) = &st.pending {
            deadline = std::time::Instant::now() + Duration::from_secs(p.remaining_secs + 10);
        }
        if st.pending.is_none() {
            // The inviting site confirms on its own once this site answers;
            // only then can it take this site online.
            let mut in_sync = false;
            for _ in 0..30 {
                let st: Status = client.call(Request::Status).await?;
                in_sync = st.peers.iter().any(|p| {
                    // Its name, with the start of its identity where
                    // another site has the same name.
                    let named =
                        p.name == joined.via || p.name.starts_with(&format!("{}#", joined.via));
                    named && p.in_sync == Some(true)
                });
                if in_sync {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            println!(
                "Done: \"{}\" is part of the network (network revision {}). See `wanderlan status`.",
                joined.site, st.revision
            );
            if !in_sync {
                println!(
                    "\"{}\" has not confirmed the new site yet; check `wanderlan status` there.",
                    joined.via
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
    }
    bail!("no confirmation in time; see `wanderlan status` and `wanderlan log`")
}

/// Waits until no automatic change (a site joining, a synchronised change)
/// is pending, so that a change made now is not refused. A change made by a
/// user must be confirmed or rolled back first.
async fn settle(client: &Client) -> Result<()> {
    let mut announced = false;
    for _ in 0..120 {
        let st: Status = client.call(Request::Status).await?;
        let Some(pending) = st.pending else {
            return Ok(());
        };
        match pending.source {
            ChangeSource::Local => bail!(
                "a change is waiting for confirmation ({} s left): run `wanderlan confirm` or `wanderlan rollback` first",
                pending.remaining_secs
            ),
            ChangeSource::Join { site } if !announced => {
                println!("Waiting until the joining site \"{site}\" is confirmed ...");
                announced = true;
            }
            ChangeSource::Sync { peer } if !announced => {
                println!("Waiting until the change from \"{peer}\" is confirmed ...");
                announced = true;
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("the pending change was not confirmed within two minutes; see `wanderlan status`")
}

async fn set_online(socket: &Path, site: &str, online: bool, confirm: bool) -> Result<ExitCode> {
    let word = if online { "online" } else { "offline" };
    edit_site(socket, site, confirm, |entry, _| {
        if entry.online == online {
            return Ok(Edit::Nothing(format!("{site} is already {word}.")));
        }
        entry.online = online;
        Ok(Edit::Done(format!("{site} is {word}")))
    })
    .await
}

/// A setting of this site (`wanderlan settings <name> <value>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SettingName {
    /// Time to confirm a change on this site (10s to 1h).
    CommitTimeout,
    /// How often this site measures the path MTU (at least 10s).
    MtuCheck,
    /// A fixed TCP MSS on this site's tunnels (536-1460).
    Mss,
    /// UDP and TCP port of this site's daemon.
    ControlPort,
    /// UDP port of this site's tunnels.
    TunnelPort,
}

/// `wanderlan settings`: shows the settings of this site or changes one,
/// always of this site (local.toml, or its own ports in the network).
async fn settings(
    socket: &Path,
    setting: Option<SettingName>,
    value: Option<&str>,
    confirm: bool,
) -> Result<ExitCode> {
    let client = connect(socket).await?;
    settle(&client).await?;
    let running: ConfigTexts = client.call(Request::GetConfig).await?;
    if !running.configured {
        bail!("this site is not part of a network");
    }
    let (mut network, mut local) = running_config(&running)?;
    let me = local
        .self_id
        .context("this site has no identity in its network")?;
    let Some(setting) = setting else {
        let own = |own: bool| if own { "" } else { "  (network default)" };
        let ports = network.site(me).map(|s| s.ports).unwrap_or_default();
        println!(
            "commit-timeout  {}{}",
            humantime::format_duration(local.commit_timeout.unwrap_or(network.commit_timeout)),
            own(local.commit_timeout.is_some())
        );
        println!(
            "mtu-check       every {}{}",
            humantime::format_duration(
                local
                    .mtu_probe_interval
                    .unwrap_or(network.mtu_probe_interval)
            ),
            own(local.mtu_probe_interval.is_some())
        );
        match local.mss.or(network.mss) {
            Some(mss) => println!("mss             fixed {mss}{}", own(local.mss.is_some())),
            None => println!("mss             from the path MTU"),
        }
        println!(
            "control-port    {} (UDP, TCP){}",
            network.control_port_of(me),
            own(ports.control.is_some())
        );
        println!(
            "tunnel-port     {} (UDP){}",
            network.tunnel_port_of(me),
            if ports.tunnel.is_some() {
                ""
            } else {
                "  (control port + 1)"
            }
        );
        return Ok(ExitCode::SUCCESS);
    };
    let value = value.context("give a value, or \"default\" for the network's")?;
    let default = value.trim() == "default";
    let duration = || -> Result<Option<Duration>> {
        if default {
            return Ok(None);
        }
        humantime::parse_duration(value.trim())
            .map(Some)
            .map_err(|_| anyhow!("\"{value}\" is not a duration like 60s or 5m"))
    };
    let number = |max: u16| -> Result<Option<u16>> {
        if default {
            return Ok(None);
        }
        match value.trim().parse::<u16>() {
            Ok(v) if (1..=max).contains(&v) => Ok(Some(v)),
            _ => bail!("\"{value}\" is no number between 1 and {max}"),
        }
    };
    let shown = if default {
        "the network's default".to_string()
    } else {
        value.trim().to_string()
    };
    let done = match setting {
        SettingName::CommitTimeout => {
            local.commit_timeout = duration()?;
            format!("The commit timeout of this site is {shown}")
        }
        SettingName::MtuCheck => {
            local.mtu_probe_interval = duration()?;
            format!("This site checks the path MTU every {shown}")
        }
        SettingName::Mss => {
            local.mss = number(u16::MAX)?;
            format!("The TCP MSS of this site is {shown}")
        }
        SettingName::ControlPort | SettingName::TunnelPort => {
            let port = number(if setting == SettingName::TunnelPort {
                u16::MAX - 1
            } else {
                u16::MAX
            })?;
            let site = network
                .site_mut(me)
                .context("this site is not in its network")?;
            if setting == SettingName::ControlPort {
                site.ports.control = port;
                format!("The control port of this site is {shown}")
            } else {
                site.ports.tunnel = port;
                format!("The tunnel port of this site is {shown}")
            }
        }
    };
    let local = config::render_local(&local, Some(&network));
    send_edit(&client, &network, local, running.revision, confirm, &done).await
}

/// Sets the trunk of the tunnel to `site` (local.toml) and applies it.
async fn set_trunk(socket: &Path, site: &str, interface: &str, confirm: bool) -> Result<ExitCode> {
    let client = connect(socket).await?;
    settle(&client).await?;
    let running: ConfigTexts = client.call(Request::GetConfig).await?;
    if !running.configured {
        bail!("this site is not part of a network");
    }
    let (network, mut local) = running_config(&running)?;
    let site_id = network.find(site).map_err(|e| anyhow!(e))?.id;
    let site = &network.label(site_id);
    let own = local.self_id == Some(site_id);
    let mut ports: Vec<String> = Vec::new();
    for port in interface.split([',', ' ']).map(str::trim) {
        // Once each, in the order given.
        if !port.is_empty() && !ports.iter().any(|p| p == port) {
            ports.push(port.to_string());
        }
    }
    let back = interface == "default";
    if own && back {
        bail!("\"default\" is for the tunnel to another site; give the ports of the default trunk");
    }
    // A trunk of its own is only for a tunnel ("default" still removes a
    // stale entry).
    if !own && !back {
        let me = local.self_id.context("this site has no identity")?;
        if !wanderlan_core::topology::tunnels_for(&network, me)
            .iter()
            .any(|t| t.peer.id == site_id)
        {
            bail!(
                "this site has no tunnel to \"{site}\"; a trunk of its own is only for the tunnel to a peer"
            );
        }
    }
    if !back {
        if ports.is_empty() {
            bail!("no interface given");
        }
        for port in &ports {
            if let Some(msg) = config::ifname_error(port) {
                bail!("{port}: {msg}");
            }
        }
    }
    // Ports the tunnels run over (as the kernel routes them): allowed, but
    // bridged they no longer carry this host's own traffic.
    let interfaces: Vec<Interface> = client.call(Request::Interfaces).await?;
    let risky: Vec<String> = ports
        .iter()
        .filter_map(|p| interfaces.iter().find(|i| i.name == *p))
        .filter(|i| !i.carries.is_empty())
        .map(|i| format!("{} carries the tunnels to {}", i.name, i.carries.join(", ")))
        .collect();
    if !back && !risky.is_empty() {
        eprintln!(
            "WARNING: {}. As a bridge port it no longer carries this host's own traffic; if the tunnels drop, do not confirm: the change is rolled back by itself.",
            risky.join("; ")
        );
    }
    let before = local.clone();
    // Ports of another segment move here; that segment keeps the rest.
    let moved = local
        .assign_trunk(
            (!own).then_some(site_id),
            if back { Vec::new() } else { ports },
            |id| network.label(id),
        )
        .map_err(|e| anyhow!(e))?;
    let now = local.trunk_ports_for(site_id).join(", ");
    if local == before {
        if own {
            println!("The default trunk already is {}.", local.trunk.join(", "));
        } else {
            println!("The tunnel to {site} already uses {now}.");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let applied = send_apply(
        &client,
        ApplyParams {
            config: ConfigInput {
                network: running.network,
                local: config::render_local(&local, Some(&network)),
            },
            base_revision: Some(running.revision),
            timeout_secs: None,
        },
    )
    .await?;
    let done = if own {
        format!("The default trunk is {}", local.trunk.join(", "))
    } else if !local.tunnel_trunks.contains_key(&site_id) {
        format!(
            "The tunnel to {site} uses the default trunk {}",
            local.trunk.join(", ")
        )
    } else {
        format!("The tunnel to {site} uses its own trunk {now} (a segment of its own)")
    };
    for note in &moved {
        println!("{note}.");
    }
    if confirm && risky.is_empty() {
        let _: Confirmed = client.call(Request::Confirm).await?;
        println!("{done}.");
    } else if confirm {
        // Confirming now would keep a change that cuts this site off.
        println!(
            "{done} now, not confirmed: the tunnels run over it. Check that they stay up, then run `wanderlan confirm` within {} seconds, otherwise it is rolled back.",
            applied.confirm_within_secs
        );
    } else {
        println!(
            "{done} now. Run `wanderlan confirm` within {} seconds to keep it, otherwise it is rolled back.",
            applied.confirm_within_secs
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `wanderlan tunnel-vlans`: sets or lifts the VLAN limit of one tunnel.
async fn set_tunnel_vlans(
    socket: &Path,
    a: &str,
    b: &str,
    vlans: &str,
    confirm: bool,
) -> Result<ExitCode> {
    let limit = match vlans.trim() {
        "" | "all" => None,
        "none" => Some(config::VlanSet::new()),
        text => Some(
            text.parse::<config::VlanSet>()
                .map_err(|e| anyhow!("{text}: {e}"))?,
        )
        .filter(|v| !v.is_all()),
    };
    let client = connect(socket).await?;
    settle(&client).await?;
    let running: ConfigTexts = client.call(Request::GetConfig).await?;
    if !running.configured {
        bail!("this site is not part of a network");
    }
    let (mut network, _) = running_config(&running)?;
    let find = |text: &str| network.find(text).map(|s| s.id).map_err(|e| anyhow!(e));
    let (a_id, b_id) = (find(a)?, find(b)?);
    let (a, b) = (&network.label(a_id), &network.label(b_id));
    if wanderlan_core::topology::tunnel_vlans(&network, a_id, b_id).is_none() {
        bail!("\"{a}\" and \"{b}\" have no tunnel with each other");
    }
    let show = |v: &config::VlanSet| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.to_string()
        }
    };
    if network.tunnel_limit(a_id, b_id) == limit.as_ref() {
        println!("The tunnel between {a} and {b} already has this limit.");
        return Ok(ExitCode::SUCCESS);
    }
    let (a, b) = (a.clone(), b.clone());
    network.set_tunnel_limit(a_id, b_id, limit.clone());
    let carries = wanderlan_core::topology::tunnel_vlans(&network, a_id, b_id).unwrap_or_default();
    let applied = send_apply(
        &client,
        ApplyParams {
            config: ConfigInput {
                network: config::render_network(&network),
                local: running.local,
            },
            base_revision: Some(running.revision),
            timeout_secs: None,
        },
    )
    .await?;
    let done = match &limit {
        Some(l) => format!(
            "The tunnel between {a} and {b} carries VLANs {} (limited to {})",
            show(&carries),
            show(l)
        ),
        None => format!(
            "The tunnel between {a} and {b} carries VLANs {} (no limit of its own)",
            show(&carries)
        ),
    };
    if confirm {
        let c: Confirmed = client.call(Request::Confirm).await?;
        println!("{done} (network revision {}).", c.revision);
    } else {
        println!(
            "{done} now. Run `wanderlan confirm` within {} seconds to keep it, otherwise it is rolled back.",
            applied.confirm_within_secs
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Changes one site of the running configuration and applies the change.
/// `edit` gets the site and the network as it was; it returns what changed,
/// or why nothing needs to change.
async fn edit_site(
    socket: &Path,
    site: &str,
    confirm: bool,
    edit: impl FnOnce(&mut config::Site, &Known) -> Result<Edit>,
) -> Result<ExitCode> {
    edit_network(socket, confirm, |network, known| {
        let id = known.find(site)?;
        let Some(entry) = network.site_mut(id) else {
            bail!("there is no site \"{site}\"");
        };
        edit(entry, known)
    })
    .await
}

/// The running network before an edit, to find the sites a user names and
/// to tell their names.
struct Known {
    network: config::Network,
    /// This site.
    me: SiteId,
}

impl Known {
    /// The site the user means (see [`config::Network::find`]).
    fn find(&self, text: &str) -> Result<SiteId> {
        self.network
            .find(text)
            .map(|s| s.id)
            .map_err(|e| anyhow!(e))
    }

    /// How a site is called.
    fn label(&self, id: SiteId) -> String {
        self.network.label(id)
    }
}

/// The running configuration: the network and `local.toml` (whose sites
/// are those of the network).
fn running_config(running: &ConfigTexts) -> Result<(config::Network, config::Local)> {
    let network = config::parse_network(&running.network)
        .ok()
        .and_then(|p| p.network)
        .context("cannot read the running configuration")?;
    let local = config::parse_local(&running.local, Some(&network))
        .map_err(|_| anyhow!("cannot read the running local.toml"))?
        .local;
    Ok((network, local))
}

/// What an edit of the running network came to.
enum Edit {
    /// Changed: what to print once applied.
    Done(String),
    /// Nothing to change: printed, nothing is applied.
    Nothing(String),
}

/// Edits the running network (`edit` gets it and the name of this site)
/// and applies it like the TUI would. Who may is checked by the daemon.
async fn edit_network(
    socket: &Path,
    confirm: bool,
    edit: impl FnOnce(&mut config::Network, &Known) -> Result<Edit>,
) -> Result<ExitCode> {
    let client = connect(socket).await?;
    settle(&client).await?;
    let running: ConfigTexts = client.call(Request::GetConfig).await?;
    if !running.configured {
        bail!("this site is not part of a network");
    }
    let (mut network, local) = running_config(&running)?;
    let me = local
        .self_id
        .context("this site has no identity in its network")?;
    let known = Known {
        network: network.clone(),
        me,
    };
    match edit(&mut network, &known)? {
        Edit::Nothing(text) => {
            println!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Edit::Done(done) => {
            let local = running.local.clone();
            send_edit(&client, &network, local, running.revision, confirm, &done).await
        }
    }
}

/// `wanderlan site add|remove|vlans`.
async fn site_command(socket: &Path, action: SiteAction) -> Result<ExitCode> {
    match action {
        SiteAction::Add {
            name,
            address,
            server,
            vlans,
            confirm,
        } => {
            if let Some(msg) = config::site_name_error(&name) {
                bail!("\"{name}\": {msg}");
            }
            let vlans = vlans
                .map(|text| {
                    text.parse::<config::VlanSet>()
                        .map_err(|e| anyhow!("{text}: {e}"))
                })
                .transpose()?;
            edit_network(socket, confirm, |network, known| {
                // Known by its name until its box joins: one site per name.
                if network.named(&name).next().is_some() {
                    bail!(
                        "\"{name}\" is already a site of the network (a site added by hand is known by its name; a box that joins with an invitation may share it)"
                    );
                }
                // Like a new site in the TUI: a client of this site.
                let uplink = match server.as_deref() {
                    Some("none") => None,
                    Some(s) => Some(known.find(s).map_err(|e| anyhow!("server: {e}"))?),
                    None => Some(known.me),
                };
                let role = role_text(uplink.map(|up| known.label(up)).as_deref(), false);
                network.sites.push(config::Site {
                    vlans: vlans.clone(),
                    // Like an invitation: its server changes its tunnel.
                    changes: config::Changes::Server,
                    uplink,
                    ..config::Site::new(&name, address)
                });
                Ok(Edit::Done(format!(
                    "Site \"{name}\" ({address}, {role}) is added"
                )))
            })
            .await
        }
        SiteAction::Remove { name, confirm } => edit_network(socket, confirm, |network, known| {
            let id = known.find(&name)?;
            if id == known.me {
                bail!(
                    "\"{name}\" is this site: `wanderlan reset` removes wanderlan from this host"
                );
            }
            let clients: Vec<String> = network
                .sites
                .iter()
                .filter(|s| s.uplink == Some(id))
                .map(|s| known.label(s.id))
                .collect();
            if !clients.is_empty() {
                bail!(
                    "\"{name}\" still has clients ({}): remove them first",
                    clients.join(", ")
                );
            }
            network.sites.retain(|s| s.id != id);
            let maps = network.maps.len();
            network.maps.retain(|m| m.site != id);
            let maps = maps - network.maps.len();
            network.tunnels.retain(|t| !t.touches(id));
            let extra = if maps > 0 {
                format!(" with its {maps} VLAN mapping(s)")
            } else {
                String::new()
            };
            Ok(Edit::Done(format!("Site \"{name}\" is removed{extra}")))
        })
        .await,
        SiteAction::Vlans {
            name,
            vlans,
            confirm,
        } => {
            let vlans = match vlans.trim() {
                "" | "default" => None,
                text => Some(
                    text.parse::<config::VlanSet>()
                        .map_err(|e| anyhow!("{text}: {e}"))?,
                ),
            };
            edit_site(socket, &name, confirm, |entry, _| {
                if entry.vlans == vlans {
                    return Ok(Edit::Nothing(format!(
                        "{} has these VLANs already.",
                        entry.name
                    )));
                }
                entry.vlans.clone_from(&vlans);
                Ok(Edit::Done(match &vlans {
                    Some(v) => format!("{} has the VLANs {v}", entry.name),
                    None => format!("{} has the VLANs of the network", entry.name),
                }))
            })
            .await
        }
    }
}

/// `wanderlan map add|del`.
async fn map_command(socket: &Path, action: MapAction) -> Result<ExitCode> {
    match action {
        MapAction::Add {
            site,
            source,
            mapped,
            confirm,
        } => {
            let local = if mapped.trim().eq_ignore_ascii_case("native") {
                config::LocalVid::Native
            } else {
                match mapped.trim().parse::<u16>() {
                    Ok(v) if (1..=4094).contains(&v) => config::LocalVid::Vid(v),
                    _ => bail!("\"{mapped}\": the mapped VLAN is \"native\" or a VLAN ID (1-4094)"),
                }
            };
            edit_network(socket, confirm, |network, known| {
                let id = known.find(&site)?;
                let there = match local {
                    config::LocalVid::Native => format!("untagged at {site}"),
                    config::LocalVid::Vid(v) => format!("VLAN {v} at {site}"),
                };
                let text = format!("source VLAN {source} = {there}");
                match network
                    .maps
                    .iter_mut()
                    .find(|m| m.site == id && m.shared == source)
                {
                    Some(m) if m.local == local => {
                        return Ok(Edit::Nothing(format!(
                            "The mapping {text} is there already."
                        )));
                    }
                    Some(m) => m.local = local,
                    None => network.maps.push(config::VlanMap {
                        site: id,
                        local,
                        shared: source,
                    }),
                }
                Ok(Edit::Done(format!("Mapping {text}")))
            })
            .await
        }
        MapAction::Del {
            site,
            source,
            confirm,
        } => {
            edit_network(socket, confirm, |network, known| {
                let id = known.find(&site)?;
                let Some(i) = network
                    .maps
                    .iter()
                    .position(|m| m.site == id && m.shared == source)
                else {
                    bail!("site \"{site}\" has no mapping of source VLAN {source}");
                };
                network.maps.remove(i);
                Ok(Edit::Done(format!(
                    "The mapping of source VLAN {source} at {site} is deleted"
                )))
            })
            .await
        }
    }
}

/// Sends an edited configuration: the warnings of a success are printed;
/// a refusal prints its diagnostics (errors first) and fails with its
/// reason.
async fn send_apply(client: &Client, params: ApplyParams) -> Result<Applied> {
    match client.call::<Applied>(Request::Apply(params)).await {
        Ok(applied) => {
            for w in &applied.warnings {
                eprintln!("{w}");
            }
            Ok(applied)
        }
        Err(crate::client::ClientError::Api(e)) if !e.diagnostics.is_empty() => {
            let (errors, rest): (Vec<_>, Vec<_>) = e.diagnostics.iter().partition(|d| d.is_error());
            for d in errors.iter().chain(rest.iter()) {
                eprintln!("{d}");
            }
            bail!("{}", e.message)
        }
        Err(e) => Err(e.into()),
    }
}

/// Applies an edited network (and `local.toml`), confirmed right away or
/// with the usual countdown.
async fn send_edit(
    client: &Client,
    network: &config::Network,
    local: String,
    base_revision: u64,
    confirm: bool,
    done: &str,
) -> Result<ExitCode> {
    let applied = send_apply(
        client,
        ApplyParams {
            config: ConfigInput {
                network: config::render_network(network),
                local,
            },
            base_revision: Some(base_revision),
            timeout_secs: None,
        },
    )
    .await?;
    if confirm {
        let c: Confirmed = client.call(Request::Confirm).await?;
        println!("{done} (network revision {}).", c.revision);
    } else {
        println!(
            "{done} now. Run `wanderlan confirm` within {} seconds to keep it, otherwise it is rolled back.",
            applied.confirm_within_secs
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `wanderlan rename`: the site and everything that names it, also
/// `self` and the trunk of the tunnel to it in `local.toml`. Who may is
/// checked by the daemon.
async fn rename(socket: &Path, site: &str, new: &str, confirm: bool) -> Result<ExitCode> {
    if let Some(msg) = config::site_name_error(new) {
        bail!("\"{new}\": {msg}");
    }
    let client = connect(socket).await?;
    settle(&client).await?;
    let running: ConfigTexts = client.call(Request::GetConfig).await?;
    if !running.configured {
        bail!("this site is not part of a network");
    }
    let (mut network, local) = running_config(&running)?;
    let entry = network.find(site).map_err(|e| anyhow!(e))?;
    let (id, old, provisional) = (entry.id, entry.name.clone(), !entry.has_identity());
    if old == new {
        println!("Nothing to change: the site is called \"{new}\" already.");
        return Ok(ExitCode::SUCCESS);
    }
    // A site known by its name (no box with an identity yet) needs a name
    // of its own; two boxes with identities may share one.
    if provisional && let Some(other) = network.named(new).next() {
        bail!(
            "\"{new}\" is already the name of a site ({}); \"{old}\" is known by its name until its box has an identity, so it needs a name of its own",
            other.address
        );
    }
    let shared = network.named(new).count();
    network.rename_site(id, new);
    let mut local = local;
    // A site known by its name got another provisional identity.
    if provisional {
        local.rekey(id, SiteId::provisional(new));
    }
    let done = if shared > 0 {
        format!(
            "Site \"{old}\" is called \"{new}\" (as {shared} other site{}: they are told apart by their identities)",
            if shared == 1 { "" } else { "s" }
        )
    } else {
        format!("Site \"{old}\" is called \"{new}\"")
    };
    let local = config::render_local(&local, Some(&network));
    send_edit(&client, &network, local, running.revision, confirm, &done).await
}

async fn reset(socket: &Path, yes: bool) -> Result<ExitCode> {
    let client = connect(socket).await?;
    // Its name, for the hint to the other sites.
    let name = client
        .call::<Status>(Request::Status)
        .await
        .ok()
        .and_then(|s| s.site)
        .unwrap_or_else(|| "<name>".to_string());
    if !yes {
        println!(
            "This removes everything wanderlan set up on this site: the bridge {}, all tunnels,\n\
             the VLAN and MSS rules, the configuration and the open invitations.",
            wanderlan_core::BRIDGE_NAME
        );
        print!("Type \"reset\" to continue: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if answer.trim() != "reset" {
            println!("Nothing was changed.");
            return Ok(ExitCode::FAILURE);
        }
    }
    let report: ResetReport = client.call(Request::Reset).await?;
    if !report.problems.is_empty() {
        eprintln!("The reset is incomplete:");
        for problem in &report.problems {
            eprintln!("  - {problem}");
        }
        eprintln!("Run `wanderlan reset` again; the log has details (`wanderlan log`).");
        return Ok(ExitCode::FAILURE);
    }
    println!(
        "Reset done ({} changes): this site is unconfigured again.",
        report.removed
    );
    println!(
        "Other sites still list it until it is removed there: `wanderlan site remove {name}` (or in `wanderlan`: Sites, d)."
    );
    Ok(ExitCode::SUCCESS)
}

/// Adds the chosen network to the box if it does not have it yet (a join
/// into a new network).
async fn add_chosen_network(socket: &Path) -> Result<Option<String>> {
    let Some(name) = chosen_network() else {
        return Ok(None);
    };
    let client = Client::connect(socket).await?.0;
    let list: Vec<api::NetworkInfo> = client
        .call(Request::Networks)
        .await
        .map_err(|e| anyhow!("the daemon cannot choose a network (an older version?): {e}"))?;
    if list.iter().any(|n| n.name == name) {
        return Ok(None);
    }
    let n: api::NetworkInfo = client
        .call(Request::AddNetwork(api::NetworkParams { name }))
        .await?;
    println!("Network {} added to this box (tag {}).", n.name, n.tag);
    Ok(Some(n.name))
}

/// Connects to the daemon, for the chosen network; one the box does not
/// have is an error (with the ones it has).
async fn connect(socket: &Path) -> Result<Client> {
    connect_to(socket, chosen_network()).await
}

async fn connect_to(socket: &Path, network: Option<String>) -> Result<Client> {
    let client = Client::connect(socket).await?.0;
    let Some(name) = network else {
        return Ok(client);
    };
    let list: Vec<api::NetworkInfo> = client
        .call(Request::Networks)
        .await
        .map_err(|e| anyhow!("the daemon cannot choose a network (an older version?): {e}"))?;
    if !list.iter().any(|n| n.name == name) {
        let names: Vec<&str> = list.iter().map(|n| n.name.as_str()).collect();
        bail!(
            "this box has no network {name} (it has: {}); `wanderlan networks add {name}` adds it",
            names.join(", ")
        );
    }
    Ok(client.for_network(Some(name)))
}

fn validate(file: &Path, local: Option<&Path>) -> Result<ExitCode> {
    let network_text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let local_text = local
        .map(|p| std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display())))
        .transpose()?;
    let diags = config::check(&network_text, local_text.as_deref());
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
        println!(
            "# ---- network.toml ----\n{EXAMPLE_NETWORK}\n# ---- local.toml ----\n{EXAMPLE_LOCAL}"
        );
        return Ok(ExitCode::SUCCESS);
    };
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for (name, content) in [
        ("network.toml", EXAMPLE_NETWORK),
        ("local.toml", EXAMPLE_LOCAL),
    ] {
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
    network: Option<&Path>,
    local: Option<&Path>,
    timeout: Option<Duration>,
) -> Result<ExitCode> {
    let client = connect(socket).await?;
    settle(&client).await?;
    let timeout_secs = timeout.map(|t| t.as_secs().max(1));
    let request = if network.is_none() && local.is_none() {
        Request::ApplyFiles(ApplyFilesParams { timeout_secs })
    } else {
        let running: ConfigTexts = client.call(Request::GetConfig).await?;
        let read = |p: &Path| {
            std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))
        };
        let network_text = match network {
            Some(p) => read(p)?,
            None => running.network.clone(),
        };
        let local_text = match local {
            Some(p) => read(p)?,
            None => running.local.clone(),
        };
        Request::Apply(ApplyParams {
            config: ConfigInput {
                network: network_text,
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

/// `--except`: networks or addresses separated by commas, or "none".
fn parse_except(text: &str) -> Result<std::collections::BTreeSet<wanderlan_core::breakout::IpNet>> {
    if matches!(text.trim(), "" | "none") {
        return Ok(std::collections::BTreeSet::new());
    }
    text.split(',')
        .map(|part| {
            wanderlan_core::breakout::parse_net(part).ok_or_else(|| {
                anyhow!(
                    "\"{}\" is not an address or a network like 203.0.113.0/24",
                    part.trim()
                )
            })
        })
        .collect()
}

/// One family of a breakout in words.
fn breakout_family(f: &api::BreakoutFamily) -> String {
    match (&f.dev, &f.reason) {
        (Some(dev), _) => match &f.gateway {
            Some(g) if g != "direct" => format!("out of {dev} via {g}"),
            _ => format!("out of {dev}"),
        },
        (None, Some(why)) => format!("through the tunnel ({why})"),
        (None, None) => "through the tunnel".into(),
    }
}

/// The breakout lines of `wanderlan status` and `wanderlan breakout`.
fn print_breakouts(st: &Status) {
    for b in &st.breakout {
        let segment = if b.bridge == wanderlan_core::BRIDGE_NAME {
            String::new()
        } else {
            format!(" ({})", b.bridge)
        };
        println!(
            "  VLAN {}{segment}: IPv4 {}, IPv6 {}{}",
            b.vid,
            breakout_family(&b.ipv4),
            breakout_family(&b.ipv6),
            if b.except.is_empty() {
                String::new()
            } else {
                format!(", except {}", b.except.join(", "))
            }
        );
        if b.ready {
            println!(
                "    {}: {} out, {} back",
                b.router,
                format_rate(b.rx_bps),
                format_rate(b.tx_bps)
            );
        } else {
            println!("    {}: being set up", b.router);
        }
        if let Some(why) = &b.blocked {
            println!("    WARNING: {why}");
        }
    }
}

/// A rate in bit/s, short.
fn format_rate(bps: u64) -> String {
    crate::tui::format::rate_unit(bps)
}

/// `wanderlan breakout` without arguments.
async fn show_breakout(socket: &Path) -> Result<ExitCode> {
    let client = connect(socket).await?;
    let st: Status = client.call(Request::Status).await?;
    if st.breakout.is_empty() {
        println!(
            "This site has no local breakout. Example: wanderlan breakout {} 10 on",
            st.site.as_deref().unwrap_or("<site>")
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "Local breakout of {}:",
        st.site.as_deref().unwrap_or("this site")
    );
    print_breakouts(&st);
    if !st.uplinks.is_empty() {
        println!("\nUplinks of this box:");
        for u in st
            .uplinks
            .iter()
            .filter(|u| u.gateway4.is_some() || u.gateway6.is_some())
        {
            let mark = match (u.default4, u.default6) {
                (true, true) => " (default route)",
                (true, false) => " (IPv4 default route)",
                (false, true) => " (IPv6 default route)",
                (false, false) => "",
            };
            println!(
                "  {}{mark}: {}{}",
                u.name,
                u.addresses.join(", "),
                if u.up { "" } else { " (down)" }
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn status(socket: &Path, json: bool) -> Result<ExitCode> {
    let client = connect(socket).await?;
    let st: Status = client.call(Request::Status).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(ExitCode::SUCCESS);
    }
    // A site with clients of its own is their server.
    let serves = |name: &str| st.sites.iter().any(|s| s.uplink.as_deref() == Some(name));
    let own_role = st.sites.iter().find(|s| s.is_self).map_or_else(
        || role_text(None, false),
        |s| role_text(s.uplink.as_deref(), serves(&s.name)),
    );
    match &st.site {
        Some(site) => println!(
            "Site {site} ({}){}, address {}, trunk {}, VLANs {}",
            own_role,
            if st.online { "" } else { " (OFFLINE)" },
            st.address.as_deref().unwrap_or("?"),
            if st.trunk_ports.is_empty() {
                st.trunk.clone().unwrap_or_else(|| "?".into())
            } else {
                st.trunk_ports.join(", ")
            },
            st.vlans.as_deref().unwrap_or("?"),
        ),
        None => println!(
            "This site is not configured yet: run `wanderlan` to start a new network, or `wanderlan join <address> <code>` with an invitation."
        ),
    }
    if st.network != api::DEFAULT_NETWORK {
        println!("Network {} of this box (tag {})", st.network, st.tag);
    }
    let others: Vec<&str> = st
        .networks
        .iter()
        .map(String::as_str)
        .filter(|n| *n != st.network)
        .collect();
    if !others.is_empty() {
        println!(
            "Other networks on this box: {} (`wanderlan --net <name> status`)",
            others.join(", ")
        );
    }
    for conflict in &st.conflicts {
        println!("LEFT OUT (another network of this box has it): {conflict}");
    }
    if let Some(id) = &st.site_id {
        println!("Identity of this box: {id}");
    }
    println!(
        "Network revision {} ({})",
        st.revision,
        short(&st.fingerprint)
    );
    if let Some(p) = &st.pending {
        println!(
            "PENDING CHANGE: rolled back in {} s unless confirmed (`wanderlan confirm`)",
            p.remaining_secs
        );
    }
    if let Some(e) = &st.last_error {
        println!("ERROR: {e}");
    }
    if let Some(problem) = &st.sync_problem {
        println!("WARNING: {problem}");
    }
    if st.invites > 0 {
        println!(
            "{} open invitation(s) (`wanderlan invite list`)",
            st.invites
        );
    }
    if !st.breakout.is_empty() {
        println!("Local breakout (`wanderlan breakout`):");
        print_breakouts(&st);
    }
    if !st.sites.is_empty() {
        println!(
            "\n{:<12} {:<15} {:<9} {:<26} LINK",
            "SITE", "ADDRESS", "ID", "ROLE"
        );
        // Servers, each with its clients below.
        let pairs: Vec<(&str, Option<&str>)> = st
            .sites
            .iter()
            .map(|s| (s.name.as_str(), s.uplink.as_deref()))
            .collect();
        for (i, depth) in wanderlan_core::topology::tree_order_by(&pairs) {
            let s = &st.sites[i];
            let link = if s.is_self {
                "this site".to_string()
            } else if let Some(hop) = &s.via {
                format!("via {hop}")
            } else if s.direct {
                "tunnel".to_string()
            } else {
                "-".to_string()
            };
            println!(
                "{:<12} {:<15} {:<9} {:<26} {link}",
                wanderlan_core::topology::tree_name(&s.name, depth),
                s.address,
                s.id.as_deref().unwrap_or("-"),
                role_text(s.uplink.as_deref(), serves(&s.name))
            );
        }
    }
    if st.peers.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "\n{:<12} {:<15} {:<8} {:>7} {:>8} {:>5} {:>5} {:>5} {:>10} {:>10}  VLANS",
        "PEER", "ADDRESS", "PORT", "REPLY", "RTT", "PMTU", "MSS", "MACS", "RX", "TX"
    );
    for p in &st.peers {
        println!(
            "{:<12} {:<15} {:<8} {:>7} {:>8} {:>5} {:>5} {:>5} {:>10} {:>10}  {}",
            p.name,
            p.address,
            port_state(p.port_state),
            p.last_reply_secs.map_or("-".into(), |s| format!("{s}s")),
            p.rtt_ms.map_or("-".into(), |r| format!("{r:.1}ms")),
            p.pmtu.map_or("-".into(), |v| v.to_string()),
            p.mss.map_or("-".into(), |v| v.to_string()),
            p.macs,
            rate(p.rx_bps),
            rate(p.tx_bps),
            p.vlans,
        );
    }
    for p in st.peers.iter().filter(|p| p.refused.is_some()) {
        println!(
            "CHANGE REFUSED from {}: {}",
            p.name,
            p.refused.as_deref().unwrap_or_default()
        );
    }
    println!();
    for p in &st.peers {
        let how = if p.tunnel_reason.is_empty() {
            p.tunnel.to_string()
        } else {
            p.tunnel_reason.clone()
        };
        println!("Tunnel to {}: VXLAN {how}", p.name);
        if let Some(why) = &p.down_reason {
            println!("  down: {why}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `server`, `client of <uplink>`, or `client of <uplink> + server` for a
/// client with clients of its own.
fn role_text(uplink: Option<&str>, has_clients: bool) -> String {
    wanderlan_core::topology::role(uplink, has_clients)
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
        PortState::Offline => "offline",
    }
}

/// Formats a byte count.
fn bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let v = n as f64;
    if v >= 1e12 {
        format!("{:.1} TB", v / 1e12)
    } else if v >= 1e9 {
        format!("{:.1} GB", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.1} MB", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1} KB", v / 1e3)
    } else {
        format!("{n} B")
    }
}

/// `wanderlan vlan <vid>`: the traffic of one VLAN per port and its MACs.
async fn vlan(socket: &Path, vid: &str, json: bool) -> Result<ExitCode> {
    let vid = match vid.trim() {
        "native" => config::NATIVE_VID,
        text => text
            .parse::<u16>()
            .ok()
            .filter(|v| (config::VID_MIN..=config::VID_MAX).contains(v))
            .ok_or_else(|| {
                anyhow!(
                    "{text}: a VLAN ID between {} and {}, or native",
                    config::VID_MIN,
                    config::VID_MAX
                )
            })?,
    };
    let client = connect(socket).await?;
    let mut traffic: VlanTraffic = client.call(Request::VlanTraffic).await?;
    // The daemon counts per VLAN only while someone asks: without rates yet,
    // ask again once it has measured (two of its 2 s samples).
    let measured = traffic.ports.iter().any(|p| p.rx_bps > 0 || p.tx_bps > 0);
    if traffic.available && !measured {
        if !json {
            eprintln!("Measuring the traffic for 4 s ...");
        }
        tokio::time::sleep(std::time::Duration::from_millis(4500)).await;
        traffic = client.call(Request::VlanTraffic).await?;
    }
    let macs: Vec<MacEntry> = client.call(Request::MacTable).await?;
    let ports: Vec<&VlanPortTraffic> = traffic.ports.iter().filter(|p| p.vid == vid).collect();
    let macs: Vec<&MacEntry> = macs.iter().filter(|m| m.vlan == Some(vid)).collect();
    if json {
        let out = serde_json::json!({
            "vid": vid,
            "available": traffic.available,
            "note": traffic.note,
            "ports": ports,
            "macs": macs,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(note) = &traffic.note {
        println!("No traffic per VLAN: {note}.");
        return Ok(ExitCode::FAILURE);
    }
    if ports.is_empty() {
        println!("VLAN {vid} is on no port of this site.");
        return Ok(ExitCode::FAILURE);
    }
    println!("VLAN {vid} (in: from the port into the bridge, out: from the bridge to the port)");
    println!(
        "{:<16} {:>11} {:>11} {:>10} {:>10} {:>21}  MACS",
        "PORT", "IN", "OUT", "RECEIVED", "SENT", "PACKETS IN/OUT"
    );
    // A tunnel by the site it leads to (its interface is named by the
    // identity of that site).
    let shown = |port: &str| {
        ports
            .iter()
            .find(|p| p.port == port)
            .and_then(|p| p.peer.clone())
            .unwrap_or_else(|| port.to_string())
    };
    for p in &ports {
        let name = match &p.peer {
            Some(peer) => peer.clone(),
            None => format!("{} (trunk)", p.port),
        };
        let count = macs.iter().filter(|m| m.port == p.port).count();
        println!(
            "{name:<16} {:>11} {:>11} {:>10} {:>10} {:>21}  {count}",
            rate(p.rx_bps),
            rate(p.tx_bps),
            bytes(p.rx_bytes),
            bytes(p.tx_bytes),
            format!("{} / {}", p.rx_packets, p.tx_packets),
        );
    }
    if !macs.is_empty() {
        println!("MAC addresses in VLAN {vid}:");
        for m in &macs {
            println!("  {} on {}", m.mac, shown(&m.port));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Formats a bit rate.
pub fn rate(bps: u64) -> String {
    format!("{}bit/s", crate::tui::format::rate(bps))
}

async fn log(socket: &Path, follow: bool) -> Result<ExitCode> {
    let (client, mut events) = Client::connect(socket).await?;
    let client = client.for_network(chosen_network());
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    fn parse(args: &[&str]) -> Option<Command> {
        Args::try_parse_from(std::iter::once("wanderlan").chain(args.iter().copied()))
            .map(|a| a.command)
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    #[test]
    fn the_site_vlan_and_map_commands_parse() {
        Args::command().debug_assert();
        assert!(matches!(
            parse(&["site", "add", "lab", "10.8.0.5", "--server", "hq", "--vlans", "10,20"]),
            Some(Command::Site { action: SiteAction::Add { name, server: Some(s), vlans: Some(_), .. } })
                if name == "lab" && s == "hq"
        ));
        assert!(matches!(
            parse(&["site", "rm", "lab", "--confirm"]),
            Some(Command::Site {
                action: SiteAction::Remove { confirm: true, .. }
            })
        ));
        assert!(matches!(
            parse(&["map", "add", "lab", "30", "native"]),
            Some(Command::Map {
                action: MapAction::Add { source: 30, .. }
            })
        ));
        assert!(matches!(
            parse(&["vlans", "all"]),
            Some(Command::Vlans { .. })
        ));
        for wrong in [
            &["site", "add", "lab", "lab.example"][..],
            &["map", "del", "lab", "5000"],
        ] {
            assert!(
                Args::try_parse_from(std::iter::once("wanderlan").chain(wrong.iter().copied()))
                    .is_err(),
                "{wrong:?}"
            );
        }
    }
}
