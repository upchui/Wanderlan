//! Non-interactive subcommands for scripts.

use std::io::{BufRead as _, Write as _};
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
use wanderlan_core::join;

use crate::client::Client;
use crate::hostinfo;
use crate::tui::{self, TuiOptions};

const EXAMPLE_NETWORK: &str = include_str!("../../../packaging/examples/network.toml");
const EXAMPLE_LOCAL: &str = include_str!("../../../packaging/examples/local.toml");

/// Command line of `wanderlan`. Without a subcommand the interactive
/// terminal UI starts.
#[derive(Debug, Parser)]
#[command(name = "wanderlan", version = wanderlan_core::version_line(), about)]
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
    /// Set who may change a site's VLANs, mappings and online state.
    ///
    /// Example: `wanderlan manage buero --self-managed yes` lets "buero"
    /// change its own VLANs too.
    Manage {
        /// The site.
        site: String,
        /// The managing site (default: the current one, or this site). It
        /// may also rename the site.
        #[arg(long)]
        by: Option<String>,
        /// Whether the site may change its own VLANs too (yes/no).
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        self_managed: Option<bool>,
        /// Confirm the change right away.
        #[arg(long)]
        confirm: bool,
    },
    /// Rename a site: this site always, another site if this site manages
    /// it (`managed_by`). Everything that names it follows, on every site.
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
    /// The local internet breakout of VLANs of a site: their public IPv4
    /// and IPv6 destinations leave through that site's own uplink
    /// (masqueraded) instead of the tunnel; private destinations and the
    /// exceptions still go through the tunnel. "off": all through the
    /// tunnel again. Without arguments: the breakouts of this site.
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
    /// The new site may change its own VLANs and mappings too (otherwise
    /// only this site may).
    #[arg(long)]
    self_managed: bool,
}

impl RuleArgs {
    fn into_rules(self) -> SiteRules {
        SiteRules {
            name: self.name,
            vlans: self.vlans,
            native: self.native,
            translations: self.maps,
            allow_self: self.self_managed,
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
    match args.command {
        None => {
            tui::run(&socket, TuiOptions { mouse: args.mouse }).await?;
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
            edit_site(&socket, &site, confirm, |entry, _| {
                if entry.uplink == uplink {
                    return Err(format!(
                        "{} is already a {}.",
                        entry.name,
                        role_text(uplink.as_deref())
                    ));
                }
                entry.uplink.clone_from(&uplink);
                Ok(format!(
                    "{} is a {}",
                    entry.name,
                    role_text(uplink.as_deref())
                ))
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
                    return Err(format!("{} already has these own VLANs.", entry.name));
                }
                entry.own_vlans.clone_from(&own);
                Ok(match &own {
                    Some(v) => format!("{} has the own VLANs {v}", entry.name),
                    None => format!("{} has no own VLANs", entry.name),
                })
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
            edit_site(&socket, &site, confirm, |entry, _| {
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
                    return Err(format!("Nothing to change for {}.", entry.name));
                }
                Ok(if entry.breakout.is_empty() {
                    format!("{} has no local breakout", entry.name)
                } else {
                    let list: config::VlanSet = entry.breakout.keys().copied().collect();
                    format!(
                        "{} breaks out VLANs {list} to the internet through its own uplink",
                        entry.name
                    )
                })
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
            by,
            self_managed,
            confirm,
        }) => {
            edit_site(&socket, &site, confirm, |entry, me| {
                let before = (entry.managed_by.clone(), entry.self_managed);
                let manager = by
                    .or_else(|| entry.managed_by.clone())
                    .unwrap_or_else(|| me.to_string());
                entry.managed_by = Some(manager);
                if let Some(v) = self_managed {
                    entry.self_managed = v;
                }
                let who = match (&entry.managed_by, entry.self_managed) {
                    (None, _) => "every site may change it".to_string(),
                    (Some(m), false) => format!("only \"{m}\" may change and rename it"),
                    (Some(m), true) => format!(
                        "\"{m}\" and \"{}\" itself may change it, \"{m}\" may rename it",
                        entry.name
                    ),
                };
                if before == (entry.managed_by.clone(), entry.self_managed) {
                    Err(format!("Nothing to change: {who}."))
                } else {
                    Ok(format!("{}: {who}", entry.name))
                }
            })
            .await
        }
        Some(Command::Join {
            target,
            code,
            name,
            trunk,
        }) => join_network(&socket, &target, &code, name, trunk).await,
        Some(Command::Reset { yes }) => reset(&socket, yes).await,
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
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let st: Status = client.call(Request::Status).await?;
        if st.state == SiteState::Unconfigured {
            bail!(
                "\"{}\" did not answer in time; the change was rolled back. Check that this site reaches it in the transport network (TCP and UDP {port}, the tunnels UDP {}) and join again with a new invitation.",
                joined.via,
                port.saturating_add(1)
            );
        }
        if st.pending.is_none() {
            // The inviting site confirms on its own once this site answers;
            // only then can it take this site online.
            let mut in_sync = false;
            for _ in 0..30 {
                let st: Status = client.call(Request::Status).await?;
                in_sync = st
                    .peers
                    .iter()
                    .any(|p| p.name == joined.via && p.in_sync == Some(true));
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
    bail!("no confirmation after two minutes; see `wanderlan status` and `wanderlan log`")
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
            return Err(format!("{site} is already {word}."));
        }
        entry.online = online;
        Ok(format!("{site} is {word}"))
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
    let mut network = config::parse_network(&running.network)
        .ok()
        .and_then(|p| p.network)
        .context("cannot read the running configuration")?;
    let mut local = config::parse_local(&running.local)
        .map_err(|_| anyhow!("cannot read the running local.toml"))?
        .local;
    let me = local.self_name.clone().context("this site has no name")?;
    let Some(setting) = setting else {
        let own = |own: bool| if own { "" } else { "  (network default)" };
        let ports = network.site(&me).map(|s| s.ports).unwrap_or_default();
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
            network.control_port_of(&me),
            own(ports.control.is_some())
        );
        println!(
            "tunnel-port     {} (UDP){}",
            network.tunnel_port_of(&me),
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
                .sites
                .iter_mut()
                .find(|s| s.name == me)
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
    send_edit(
        &client,
        &network,
        config::render_local(&local),
        running.revision,
        confirm,
        &done,
    )
    .await
}

/// Sets the trunk of the tunnel to `site` (local.toml) and applies it.
async fn set_trunk(socket: &Path, site: &str, interface: &str, confirm: bool) -> Result<ExitCode> {
    let client = connect(socket).await?;
    settle(&client).await?;
    let running: ConfigTexts = client.call(Request::GetConfig).await?;
    if !running.configured {
        bail!("this site is not part of a network");
    }
    let network = config::parse_network(&running.network)
        .ok()
        .and_then(|p| p.network)
        .context("cannot read the running configuration")?;
    let mut local = config::parse_local(&running.local)
        .map_err(|_| anyhow!("cannot read the running local.toml"))?
        .local;
    let own = local.self_name.as_deref() == Some(site);
    if network.site(site).is_none() {
        bail!("there is no site \"{site}\"");
    }
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
        let me = local
            .self_name
            .as_deref()
            .context("this site has no name")?;
        if !wanderlan_core::topology::tunnels_for(&network, me)
            .iter()
            .any(|t| t.peer.name == site)
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
            (!own).then_some(site),
            if back { Vec::new() } else { ports },
        )
        .map_err(|e| anyhow!(e))?;
    let now = local.trunk_ports_for(site).join(", ");
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
                local: config::render_local(&local),
            },
            base_revision: Some(running.revision),
            timeout_secs: None,
        },
    )
    .await?;
    let done = if own {
        format!("The default trunk is {}", local.trunk.join(", "))
    } else if !local.tunnel_trunks.contains_key(site) {
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
    let mut network = config::parse_network(&running.network)
        .ok()
        .and_then(|p| p.network)
        .context("cannot read the running configuration")?;
    if let Some(unknown) = [a, b].into_iter().find(|n| network.site(n).is_none()) {
        bail!("there is no site \"{unknown}\" in the network of this site");
    }
    if wanderlan_core::topology::tunnel_vlans(&network, a, b).is_none() {
        bail!("\"{a}\" and \"{b}\" have no tunnel with each other");
    }
    let show = |v: &config::VlanSet| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.to_string()
        }
    };
    if network.tunnel_limit(a, b) == limit.as_ref() {
        println!("The tunnel between {a} and {b} already has this limit.");
        return Ok(ExitCode::SUCCESS);
    }
    network.set_tunnel_limit(a, b, limit.clone());
    let carries = wanderlan_core::topology::tunnel_vlans(&network, a, b).unwrap_or_default();
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
/// `edit` gets the site and the name of this site; it returns what changed,
/// or why nothing needs to change.
async fn edit_site(
    socket: &Path,
    site: &str,
    confirm: bool,
    edit: impl FnOnce(&mut config::Site, &str) -> Result<String, String>,
) -> Result<ExitCode> {
    let client = connect(socket).await?;
    settle(&client).await?;
    let status: Status = client.call(Request::Status).await?;
    let running: ConfigTexts = client.call(Request::GetConfig).await?;
    if !running.configured {
        bail!("this site is not part of a network");
    }
    let mut network = config::parse_network(&running.network)
        .ok()
        .and_then(|p| p.network)
        .context("cannot read the running configuration")?;
    let Some(entry) = network.sites.iter_mut().find(|s| s.name == site) else {
        bail!("there is no site \"{site}\"");
    };
    let me = status.site.unwrap_or_default();
    let done = match edit(entry, &me) {
        Ok(done) => done,
        Err(nothing) => {
            println!("{nothing}");
            return Ok(ExitCode::SUCCESS);
        }
    };
    let local = running.local.clone();
    send_edit(&client, &network, local, running.revision, confirm, &done).await
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
    let mut network = config::parse_network(&running.network)
        .ok()
        .and_then(|p| p.network)
        .context("cannot read the running configuration")?;
    let mut local = config::parse_local(&running.local)
        .ok()
        .map(|p| p.local)
        .context("cannot read the running local.toml")?;
    if network.site(site).is_none() {
        bail!("there is no site \"{site}\" in the network of this site");
    }
    if site == new {
        println!("Nothing to change: the site is called \"{new}\" already.");
        return Ok(ExitCode::SUCCESS);
    }
    if network.site(new).is_some() {
        bail!("\"{new}\" is already a site of the network");
    }
    network.rename_site(site, new);
    local.rename_site(site, new);
    let done = format!("Site \"{site}\" is called \"{new}\"");
    let local = config::render_local(&local);
    send_edit(&client, &network, local, running.revision, confirm, &done).await
}

async fn reset(socket: &Path, yes: bool) -> Result<ExitCode> {
    let client = connect(socket).await?;
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
    println!("Other sites still list it until it is removed there (`wanderlan`, Sites, d).");
    Ok(ExitCode::SUCCESS)
}

async fn connect(socket: &Path) -> Result<Client> {
    Ok(Client::connect(socket).await?.0)
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
    }
}

/// A rate in bit/s, short.
fn format_rate(bps: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let v = bps as f64;
    if v >= 1e9 {
        format!("{:.1} Gbit/s", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.1} Mbit/s", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.0} kbit/s", v / 1e3)
    } else {
        format!("{bps} bit/s")
    }
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
    let own_uplink = st
        .sites
        .iter()
        .find(|s| s.is_self)
        .and_then(|s| s.uplink.as_deref());
    match &st.site {
        Some(site) => println!(
            "Site {site} ({}){}, address {}, trunk {}, VLANs {}",
            role_text(own_uplink),
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
            "\n{:<12} {:<15} {:<9} {:<18} LINK",
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
                "{:<12} {:<15} {:<9} {:<18} {link}",
                wanderlan_core::topology::tree_name(&s.name, depth),
                s.address,
                s.id.as_deref().unwrap_or("-"),
                role_text(s.uplink.as_deref())
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

/// `server`, or `client of <uplink>`.
fn role_text(uplink: Option<&str>) -> String {
    uplink.map_or_else(|| "server".to_string(), |u| format!("client of {u}"))
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
    let traffic: VlanTraffic = client.call(Request::VlanTraffic).await?;
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
    for p in &ports {
        let name = match &p.peer {
            Some(_) => p.port.clone(),
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
            println!("  {} on {}", m.mac, m.port);
        }
    }
    Ok(ExitCode::SUCCESS)
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
