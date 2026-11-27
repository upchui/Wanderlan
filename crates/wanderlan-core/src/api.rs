//! API between the TUI/CLI and the daemon.
//!
//! Transport: Unix socket `/run/wanderlan/wanderlan.sock`, one JSON object
//! per line in both directions. A request is `{"id":1,"method":"status"}`
//! (plus `"params":{...}` for methods with parameters); the daemon answers
//! with `{"id":1,"result":...}` or `{"id":1,"error":{...}}`. After a
//! `subscribe` request the daemon also sends `{"event":{...}}` lines on that
//! connection.

use serde::{Deserialize, Serialize};

use crate::config::{Changes, Diagnostic};

/// Default socket path.
pub const SOCKET_PATH: &str = "/run/wanderlan/wanderlan.sock";
/// Environment variable that overrides the socket path.
pub const SOCKET_ENV: &str = "WANDERLAN_SOCKET";

/// Returns the socket path, honouring [`SOCKET_ENV`].
pub fn socket_path() -> std::path::PathBuf {
    std::env::var_os(SOCKET_ENV).map_or_else(|| SOCKET_PATH.into(), Into::into)
}

/// A request with its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Echoed in the response.
    pub id: u64,
    /// The network of the box the request is for; `None`: the default
    /// network ([`DEFAULT_NETWORK`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// The request.
    #[serde(flatten)]
    pub request: Request,
}

/// Requests understood by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Current status → [`Status`].
    Status,
    /// Event history, then live events on this connection → [`Vec<Event>`].
    Subscribe,
    /// Running configuration → [`ConfigTexts`].
    GetConfig,
    /// Validates a configuration without applying it → [`Validation`].
    Validate(ConfigInput),
    /// Applies a configuration; it must be confirmed in time → [`Applied`].
    Apply(ApplyParams),
    /// Applies the files in `/etc/wanderlan` as they are → [`Applied`].
    ApplyFiles(ApplyFilesParams),
    /// Makes the pending change permanent → [`Confirmed`].
    Confirm,
    /// Reverts the pending change → [`Done`].
    Rollback,
    /// Network interfaces of the host (setup wizard) → [`Vec<Interface>`].
    Interfaces,
    /// Learned MAC addresses → [`Vec<MacEntry>`].
    MacTable,
    /// Traffic per VLAN and bridge port → [`VlanTraffic`].
    VlanTraffic,
    /// The traffic of the tunnels and trunk ports and the round trips of
    /// the last hour, newer than `since_ms` → [`History`].
    History(HistoryParams),
    /// Re-enables a port shut down by the loop guard → [`Done`].
    Unblock(UnblockParams),
    /// Measures the path MTU to all peers again → [`Done`].
    ProbeNow,
    /// Creates an invitation for a new site with rules for it →
    /// [`Invitation`].
    Invite(SiteRules),
    /// Lists the open invitations of this site → [`Vec<Invitation>`].
    Invites,
    /// Deletes an open invitation → [`Done`].
    DeleteInvite(InviteParams),
    /// Joins an existing network with an invitation (unconfigured sites
    /// only) → [`Joined`].
    Join(JoinParams),
    /// Removes the bridge, the tunnels, all rules, the configuration and the
    /// invitations: the site is unconfigured again → [`ResetReport`].
    Reset,
    /// The networks of the box (for every request the same, whatever
    /// network it names) → [`Vec<NetworkInfo>`].
    Networks,
    /// Adds a network to the box, unconfigured → [`NetworkInfo`].
    AddNetwork(NetworkParams),
    /// Resets a network and removes it from the box (not the default
    /// network) → [`ResetReport`].
    RemoveNetwork(NetworkParams),
}

/// Parameters of [`Request::AddNetwork`] and [`Request::RemoveNetwork`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkParams {
    /// The name of the network.
    pub name: String,
}

/// One network of the box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInfo {
    /// Its name ([`DEFAULT_NETWORK`] for the one in `/etc/wanderlan`).
    pub name: String,
    /// Its tag in interface names.
    pub tag: char,
    /// The name of this box's site in it, once it is configured.
    pub site: Option<String>,
}

/// A configuration given as TOML texts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigInput {
    /// Contents of `network.toml`.
    #[serde(alias = "mesh")]
    pub network: String,
    /// Contents of `local.toml`.
    pub local: String,
}

/// Parameters of [`Request::Apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyParams {
    /// The new configuration.
    #[serde(flatten)]
    pub config: ConfigInput,
    /// Revision the change is based on; the daemon refuses the change if it
    /// knows a newer revision (from this site or a peer).
    #[serde(default)]
    pub base_revision: Option<u64>,
    /// Confirmation timeout in seconds; defaults to `commit_timeout`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Parameters of [`Request::ApplyFiles`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyFilesParams {
    /// Confirmation timeout in seconds; defaults to `commit_timeout`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Parameters of [`Request::Unblock`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnblockParams {
    /// Tunnel interface, e.g. `vx-haus2`.
    pub port: String,
}

/// Rules for the site that joins with an invitation; everything is
/// optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteRules {
    /// Name of the new site; without it, the host name of the joining site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// VLANs of the new site, e.g. `native,10-50`; without them, the
    /// network's VLANs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlans: Option<String>,
    /// Source VLAN that takes the untagged traffic of the new site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<u16>,
    /// Mappings: VLAN at the new site (`local`, the mapped VLAN) → VLAN of the
    /// network (`shared`, the source VLAN).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub translations: Vec<Translation>,
    /// Who changes the tunnel of the new site to the inviting site later;
    /// none: only the inviting site (its server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Changes>,
    /// Invitations of earlier versions: the new site may change itself too
    /// (both). Read, never written.
    #[serde(default, skip_serializing)]
    pub allow_self: bool,
}

/// A VLAN translation of [`SiteRules`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Translation {
    /// VLAN ID on the trunk of the site.
    pub local: u16,
    /// VLAN ID in the shared network.
    pub shared: u16,
}

impl SiteRules {
    /// True if the invitation sets nothing (the network's defaults apply).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Who changes the tunnel of the new site: only its server unless the
    /// invitation says otherwise.
    pub fn changes(&self) -> Changes {
        match (self.changes, self.allow_self) {
            (Some(c), _) => c,
            (None, true) => Changes::Both,
            (None, false) => Changes::Server,
        }
    }

    /// Short description, e.g. `name site5, VLANs native,10-50, source VLAN 20 =
    /// untagged, source VLAN 30 = VLAN 130`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = &self.name {
            parts.push(format!("name {n}"));
        }
        if let Some(v) = &self.vlans {
            parts.push(format!("VLANs {v}"));
        }
        if let Some(n) = self.native {
            parts.push(format!("source VLAN {n} = untagged"));
        }
        for t in &self.translations {
            parts.push(format!("source VLAN {} = VLAN {}", t.shared, t.local));
        }
        match self.changes() {
            Changes::Server => {}
            Changes::Client => parts.push("changes its tunnel itself (only the client)".into()),
            Changes::Both => parts.push("may change itself too".into()),
        }
        if parts.is_empty() {
            "defaults of the network".into()
        } else {
            parts.join(", ")
        }
    }
}

/// Parameters of [`Request::DeleteInvite`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteParams {
    /// The invitation code.
    pub code: String,
}

/// Parameters of [`Request::Join`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinParams {
    /// Site that created the invitation: `<address>` or `<address>:<port>`.
    pub target: String,
    /// The invitation code.
    pub code: String,
    /// Name of this site.
    pub name: String,
    /// True if the name was not chosen by the user (the host name); the
    /// inviting site then appends `-2`, `-3`, ... if it is taken.
    #[serde(default)]
    pub name_is_default: bool,
    /// Local trunk interface.
    pub trunk: String,
}

/// An open invitation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invitation {
    /// The code, e.g. `K7F3Q-9QX2M`.
    pub code: String,
    /// The command to run on the new site.
    pub command: String,
    /// Unix time the invitation was created.
    pub created_secs: u64,
    /// True while a site is joining with it (not used up yet).
    #[serde(default)]
    pub in_use: bool,
    /// Rules for the new site.
    #[serde(default)]
    pub rules: SiteRules,
}

/// Result of [`Request::Join`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Joined {
    /// Name this site got in the network.
    pub site: String,
    /// The site that let it join.
    pub via: String,
    /// Number of sites in the network.
    pub sites: usize,
    /// Network revision received.
    pub revision: u64,
}

/// A response with its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    /// Id of the request.
    pub id: u64,
    /// Result on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// An error answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    /// Human-readable message.
    pub message: String,
    /// Validation problems, if the error is about an invalid configuration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

impl ApiError {
    /// An error with a message only.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            diagnostics: Vec::new(),
        }
    }
}

/// A line sent by the daemon: a response or an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    /// An asynchronous event (after `subscribe`).
    Event {
        /// The event.
        event: Event,
    },
    /// The answer to a request.
    Response(ResponseEnvelope),
}

/// Empty result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {}

/// Result of [`Request::Reset`]. Every step runs even if an earlier one
/// failed; afterwards the daemon checks that nothing is left.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetReport {
    /// System changes made (interfaces, rules removed).
    pub removed: usize,
    /// What could not be undone, one sentence each; empty on success.
    pub problems: Vec<String>,
}

/// Result of [`Request::Validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validation {
    /// True if there are no errors.
    pub valid: bool,
    /// Errors and warnings with line numbers.
    pub diagnostics: Vec<Diagnostic>,
    /// Unified diff against the running configuration (canonical form).
    pub diff: String,
}

/// Result of [`Request::Apply`] and [`Request::ApplyFiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    /// Seconds left to confirm.
    pub confirm_within_secs: u64,
    /// Unified diff against the previous configuration.
    pub diff: String,
    /// Changes made to the system, in plain language.
    pub changes: Vec<String>,
    /// Warnings from the validation.
    pub warnings: Vec<Diagnostic>,
}

/// Result of [`Request::Confirm`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmed {
    /// The network revision now in effect.
    pub revision: u64,
}

/// Result of [`Request::GetConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigTexts {
    /// True if the site has a configuration.
    pub configured: bool,
    /// Running `network.toml` in canonical form (empty if none).
    #[serde(alias = "mesh")]
    pub network: String,
    /// Running `local.toml` in canonical form.
    pub local: String,
    /// Network revision.
    pub revision: u64,
    /// Network fingerprint (hex SHA-256).
    pub fingerprint: String,
}

/// Overall state of the site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteState {
    /// No configuration yet (setup wizard).
    Unconfigured,
    /// Configuration applied.
    Running,
    /// The configuration could not be applied.
    Error,
}

/// Who started a pending change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ChangeSource {
    /// A user on this site (TUI/CLI).
    Local,
    /// The configuration sync, received from a peer.
    Sync {
        /// Peer the configuration came from.
        peer: String,
    },
    /// A new site joined with an invitation of this site.
    Join {
        /// Name of the new site.
        site: String,
    },
}

/// A change waiting for confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingChange {
    /// Seconds until the automatic rollback.
    pub remaining_secs: u64,
    /// Where the change came from.
    pub source: ChangeSource,
}

/// State of a tunnel port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortState {
    /// Up.
    Up,
    /// Down.
    Down,
    /// Shut down by the loop guard.
    Blocked,
    /// Not created (yet).
    Missing,
    /// Down on purpose: this site or the peer is offline (`online = false`).
    Offline,
}

/// Counters of an interface since it was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    /// Received bytes.
    pub rx_bytes: u64,
    /// Transmitted bytes.
    pub tx_bytes: u64,
    /// Received packets.
    pub rx_packets: u64,
    /// Transmitted packets.
    pub tx_packets: u64,
    /// Receive errors.
    pub rx_errors: u64,
    /// Transmit errors.
    pub tx_errors: u64,
    /// Received packets dropped.
    pub rx_dropped: u64,
    /// Packets dropped before sending.
    pub tx_dropped: u64,
}

/// Round-trip times of the control probes over the last minutes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RttStats {
    /// Shortest round trip in milliseconds.
    pub min_ms: f64,
    /// Average round trip in milliseconds.
    pub avg_ms: f64,
    /// Longest round trip in milliseconds.
    pub max_ms: f64,
    /// Jitter: mean difference between consecutive round trips (ms).
    pub jitter_ms: f64,
    /// Probes without answer, in percent.
    pub loss_percent: f64,
    /// Probes in the window.
    pub samples: u32,
}

/// Traffic of a local port (the trunk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortTraffic {
    /// Interface name.
    pub name: String,
    /// Receive rate in bit/s.
    pub rx_bps: u64,
    /// Transmit rate in bit/s.
    pub tx_bps: u64,
    /// Counters.
    pub traffic: Traffic,
}

/// Status of one direct peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    /// Site name.
    pub name: String,
    /// Transport address.
    pub address: String,
    /// Tunnel interface.
    pub port: String,
    /// Tunnel port state.
    pub port_state: PortState,
    /// VLANs carried by the tunnel (both sites take part in them).
    #[serde(default)]
    pub vlans: String,
    /// The local trunk this tunnel is bridged with.
    #[serde(default)]
    pub trunk: String,
    /// For a client of this site: why its last change was not taken over.
    #[serde(default)]
    pub refused: Option<String>,
    /// The peer answers control probes.
    pub reachable: bool,
    /// Seconds since the last answered probe.
    pub last_reply_secs: Option<u64>,
    /// Round-trip time in milliseconds.
    pub rtt_ms: Option<f64>,
    /// Round-trip statistics over the last minutes.
    #[serde(default)]
    pub rtt_stats: Option<RttStats>,
    /// Measured path MTU.
    pub pmtu: Option<u16>,
    /// TCP MSS clamped on this port.
    pub mss: Option<u16>,
    /// Learned MAC addresses behind this port.
    pub macs: usize,
    /// Receive rate in bit/s.
    pub rx_bps: u64,
    /// Transmit rate in bit/s.
    pub tx_bps: u64,
    /// Counters of the tunnel port.
    #[serde(default)]
    pub traffic: Traffic,
    /// Network revision announced by the peer.
    pub revision: Option<u64>,
    /// Whether the peer runs the same network configuration.
    pub in_sync: Option<bool>,
    /// The short form of the peer's identity, if it has one.
    #[serde(default)]
    pub identity: Option<String>,
    /// For a tunnel with a key: whether the peer proved its identity.
    #[serde(default)]
    pub identity_state: Option<IdentityState>,
    /// Why the tunnel is down (while it is).
    #[serde(default)]
    pub down_reason: Option<String>,
    /// How the tunnel runs (VXLAN): through NAT or direct.
    #[serde(default)]
    pub tunnel: crate::config::TunnelMode,
    /// Why it runs so: "reached directly at …", "not reachable from
    /// outside (NAT or firewall)", …
    #[serde(default)]
    pub tunnel_reason: String,
    /// The VNI of the tunnel.
    #[serde(default)]
    pub vni: Option<u32>,
    /// For a client (behind NAT): where its tunnel packets come from.
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Whether a peer proved its identity (tunnels with a key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityState {
    /// No authenticated answer yet: the tunnel waits.
    Waiting,
    /// The peer answered with its key.
    Confirmed,
    /// Something at its address answers with another key.
    WrongKey,
}

impl IdentityState {
    /// The state as shown in the UI.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Confirmed => "confirmed",
            Self::WrongKey => "wrong key",
        }
    }
}

impl From<crate::identity::PeerIdentity> for IdentityState {
    fn from(state: crate::identity::PeerIdentity) -> Self {
        match state {
            crate::identity::PeerIdentity::Waiting => Self::Waiting,
            crate::identity::PeerIdentity::Confirmed => Self::Confirmed,
            crate::identity::PeerIdentity::WrongKey => Self::WrongKey,
        }
    }
}

/// One site of the network (for the overview).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteSummary {
    /// Name.
    pub name: String,
    /// Transport address.
    pub address: String,
    /// Shared VLANs the site takes part in.
    #[serde(default)]
    pub vlans: String,
    /// False while the site is offline: its tunnels stay down.
    #[serde(default = "online_default")]
    pub online: bool,
    /// True for this site.
    pub is_self: bool,
    /// True if there is a tunnel to this site.
    pub direct: bool,
    /// The site it is a client of; `None` for a server.
    #[serde(default)]
    pub uplink: Option<String>,
    /// For sites without a tunnel from here: the neighbour traffic to it
    /// passes ("via alex").
    #[serde(default)]
    pub via: Option<String>,
    /// The short form of its identity, if it has one.
    #[serde(default)]
    pub id: Option<String>,
}

/// Result of [`Request::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// Daemon version.
    pub version: String,
    /// Overall state.
    pub state: SiteState,
    /// Name of this site.
    pub site: Option<String>,
    /// Transport address of this site.
    pub address: Option<String>,
    /// False while this site is offline: its tunnels stay down.
    #[serde(default = "online_default")]
    pub online: bool,
    /// Trunk interface.
    pub trunk: Option<String>,
    /// Shared VLANs.
    pub vlans: Option<String>,
    /// Network revision in effect.
    pub revision: u64,
    /// Network fingerprint.
    pub fingerprint: String,
    /// Fingerprint of this site's `local.toml` in effect (it changes
    /// without a new network revision, e.g. `wanderlan settings`).
    #[serde(default)]
    pub local_fingerprint: String,
    /// A change waiting for confirmation.
    pub pending: Option<PendingChange>,
    /// Last error applying the configuration.
    pub last_error: Option<String>,
    /// Direct peers with live data.
    pub peers: Vec<PeerStatus>,
    /// All sites of the network.
    pub sites: Vec<SiteSummary>,
    /// Ports shut down by the loop guard.
    pub blocked: Vec<String>,
    /// Open invitations of this site.
    #[serde(default)]
    pub invites: usize,
    /// Traffic of the (default) trunk.
    #[serde(default)]
    pub trunk_traffic: Option<PortTraffic>,
    /// Every trunk with its traffic: the default trunk first, then the
    /// trunks of single tunnels (each a segment of its own).
    #[serde(default)]
    pub trunks: Vec<PortTraffic>,
    /// On a client: why its own change was not taken over by its server
    /// (the server's state applies again).
    #[serde(default)]
    pub sync_problem: Option<String>,
    /// All ports of the default trunk, the main port first.
    #[serde(default)]
    pub trunk_ports: Vec<String>,
    /// Configured trunk ports that do not exist (e.g. unplugged).
    #[serde(default)]
    pub missing_ports: Vec<String>,
    /// The identity of this box (its public key), once it has one.
    #[serde(default)]
    pub site_id: Option<String>,
    /// The local breakouts of this site, by segment and VLAN.
    #[serde(default)]
    pub breakout: Vec<BreakoutStatus>,
    /// The interfaces of this box that can be an uplink (only looked at
    /// while this site has a breakout).
    #[serde(default)]
    pub uplinks: Vec<UplinkStatus>,
    /// The network of the box this status is of.
    #[serde(default = "default_network")]
    pub network: String,
    /// Its tag in interface names ([`crate::topology::DEFAULT_TAG`] for
    /// the default network).
    #[serde(default = "default_tag")]
    pub tag: char,
    /// The networks of the box, in their order (the default one first).
    #[serde(default)]
    pub networks: Vec<String>,
    /// What this network lost to an earlier network on the box.
    #[serde(default)]
    pub conflicts: Vec<String>,
}

/// The name of the network in `/etc/wanderlan` itself.
pub const DEFAULT_NETWORK: &str = "default";
/// The longest name of a network.
pub const MAX_NETWORK_NAME_LEN: usize = 12;

/// Why `name` cannot be the name of a further network of a box, if it
/// cannot: lowercase letters, digits and `-` (not first), at most
/// [`MAX_NETWORK_NAME_LEN`] characters, not [`DEFAULT_NETWORK`].
pub fn network_name_error(name: &str) -> Option<String> {
    if name == DEFAULT_NETWORK {
        return Some(format!(
            "\"{DEFAULT_NETWORK}\" is the network in /etc/wanderlan itself"
        ));
    }
    let valid = !name.is_empty()
        && name.len() <= MAX_NETWORK_NAME_LEN
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
    (!valid).then(|| {
        format!(
            "\"{name}\" is no network name: lowercase letters, digits and - (not first), at most {MAX_NETWORK_NAME_LEN} characters"
        )
    })
}

fn default_network() -> String {
    DEFAULT_NETWORK.to_string()
}

fn default_tag() -> char {
    crate::topology::DEFAULT_TAG
}

/// The local breakout of one VLAN in one segment of this site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakoutStatus {
    /// The VLAN (in the network).
    pub vid: u16,
    /// The bridge of the segment.
    pub bridge: String,
    /// The router end (`wlbo<vid>`).
    pub router: String,
    /// The configured uplink; `None`: the default route.
    pub uplink: Option<String>,
    /// The exceptions (through the tunnel).
    pub except: Vec<String>,
    /// How IPv4 leaves.
    pub ipv4: BreakoutFamily,
    /// How IPv6 leaves.
    pub ipv6: BreakoutFamily,
    /// The veth pair is in place (up, in its bridge and VLAN).
    pub ready: bool,
    /// Another firewall on this box drops forwarded packets: the breakout
    /// cannot work until it accepts them.
    #[serde(default)]
    pub blocked: Option<String>,
    /// Receive rate of the router in bit/s (from the devices, out to the
    /// internet).
    pub rx_bps: u64,
    /// Transmit rate in bit/s (answers to the devices).
    pub tx_bps: u64,
    /// The counters of the router.
    pub traffic: Traffic,
}

/// One family of a breakout: the interface it leaves by, or why it goes
/// through the tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakoutFamily {
    /// The interface it leaves by.
    pub dev: Option<String>,
    /// The next hop of a fixed uplink.
    pub gateway: Option<String>,
    /// Why it does not break out (it goes through the tunnel).
    pub reason: Option<String>,
}

/// An interface of this box as an uplink.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UplinkStatus {
    /// Interface name.
    pub name: String,
    /// Operationally up.
    pub up: bool,
    /// Its addresses.
    pub addresses: Vec<String>,
    /// The gateway of its IPv4 default route (`"direct"` without one).
    pub gateway4: Option<String>,
    /// The same for IPv6.
    pub gateway6: Option<String>,
    /// The box's own traffic leaves by it (IPv4, IPv6).
    pub default4: bool,
    /// The same for IPv6.
    pub default6: bool,
    /// A point-to-point link (e.g. a WireGuard interface): an uplink
    /// without a default route of its own.
    #[serde(default)]
    pub pointopoint: bool,
}

fn online_default() -> bool {
    true
}

/// Severity of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Informational.
    Info,
    /// Something needs attention.
    Warn,
    /// Something failed.
    Error,
}

/// An entry of the event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Unix time in milliseconds.
    pub time_ms: u64,
    /// Severity.
    pub level: Level,
    /// Peer the event is about, if any.
    pub peer: Option<String>,
    /// Message in plain language.
    pub message: String,
}

/// A network interface of the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    /// Name.
    pub name: String,
    /// Kernel kind ("" for physical interfaces).
    pub kind: String,
    /// Administratively up.
    pub up: bool,
    /// Master device, if attached to a bridge or bond.
    pub master: Option<String>,
    /// MTU.
    pub mtu: u32,
    /// True if the interface was created by wanderlan.
    pub managed: bool,
    /// The peers whose tunnel leaves this host through this interface, as
    /// the kernel routes it. Bridged, it no longer carries this host's own
    /// traffic, so these tunnels would drop.
    #[serde(default)]
    pub carries: Vec<String>,
}

/// Devices the kernel creates by itself when a tunnel module is loaded
/// (`ip_gre` creates `gre0`, `gretap0` and `erspan0`). They carry no traffic
/// and are never offered as a trunk.
pub const KERNEL_FALLBACK_DEVICES: [&str; 9] = [
    "gre0", "gretap0", "erspan0", "ip6gre0", "ip6tnl0", "sit0", "tunl0", "ip_vti0", "ip6_vti0",
];

impl Interface {
    /// True for an interface worth showing in a list of possible trunks:
    /// not the loopback, not created by wanderlan, not a kernel fallback
    /// device.
    pub fn is_listed(&self) -> bool {
        self.name != "lo" && !self.managed && !KERNEL_FALLBACK_DEVICES.contains(&self.name.as_str())
    }

    /// True for an interface that can be the trunk: physical or a VLAN,
    /// bond or veth device, not created by wanderlan, not attached to another
    /// bridge or bond (attached to wanderlan's own bridge is fine).
    pub fn is_trunk_candidate(&self) -> bool {
        let kind_ok = matches!(self.kind.as_str(), "" | "vlan" | "bond" | "veth");
        let master_ok = self.master.as_deref().is_none_or(|m| {
            m == crate::BRIDGE_NAME || m.starts_with(crate::desired::SEGMENT_PREFIX)
        });
        self.is_listed() && kind_ok && master_ok
    }
}

/// Parameters of [`Request::History`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryParams {
    /// Only what came after this time (ms since the Unix epoch); without
    /// it, the whole hour.
    #[serde(default)]
    pub since_ms: Option<u64>,
}

/// What the daemon recorded for the graphs ([`Request::History`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct History {
    /// The daemon's clock now (ms since the Unix epoch).
    pub now_ms: u64,
    /// When the daemon started (ms): another value means another history.
    pub started_ms: u64,
    /// How often the traffic is sampled (ms).
    pub interval_ms: u64,
    /// Per direct peer: the traffic of its tunnel and its round trips.
    pub peers: Vec<PeerHistory>,
    /// Per trunk port: its traffic.
    pub trunks: Vec<PortHistory>,
}

/// The history of one peer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerHistory {
    /// The peer.
    pub name: String,
    /// The traffic of its tunnel: in = from the peer, out = to it.
    pub traffic: Vec<TrafficPoint>,
    /// Every control probe: its round trip, or lost.
    pub rtt: Vec<RttPoint>,
}

/// The history of one trunk port.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PortHistory {
    /// The port.
    pub port: String,
    /// Its traffic: in = into the bridge, out = out of it.
    pub traffic: Vec<TrafficPoint>,
}

/// One sample: `[at_ms, rx_bps, tx_bps]` (an array on the wire).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficPoint(pub u64, pub u64, pub u64);

impl TrafficPoint {
    /// When (ms since the Unix epoch).
    pub fn at(&self) -> u64 {
        self.0
    }
}

/// One probe: `[at_ms, round trip in ms]`, `null` for a lost one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RttPoint(pub u64, pub Option<f32>);

/// Traffic per VLAN on the ports of this site's bridges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlanTraffic {
    /// True if the bridges count per VLAN and port.
    pub available: bool,
    /// Why not, or what is missing.
    pub note: Option<String>,
    /// One entry per port and VLAN, sorted by port, then VLAN.
    pub ports: Vec<VlanPortTraffic>,
    /// When the rates were sampled (ms since the Unix epoch); `None` for
    /// counters without rates (the sampler had not caught up yet).
    #[serde(default)]
    pub sampled_ms: Option<u64>,
}

/// Traffic of one VLAN on one bridge port. Received: from the port into the
/// bridge (e.g. from the peer); sent: from the bridge out of the port.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlanPortTraffic {
    /// Bridge port (`vx-<peer>` or a trunk port).
    pub port: String,
    /// The peer behind a tunnel port; `None` for a trunk port.
    pub peer: Option<String>,
    /// VLAN (shared ID).
    pub vid: u16,
    /// Receive rate in bit/s.
    pub rx_bps: u64,
    /// Transmit rate in bit/s.
    pub tx_bps: u64,
    /// Bytes received.
    pub rx_bytes: u64,
    /// Bytes sent.
    pub tx_bytes: u64,
    /// Packets received.
    pub rx_packets: u64,
    /// Packets sent.
    pub tx_packets: u64,
}

/// A learned MAC address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacEntry {
    /// MAC address (lowercase, colon separated).
    pub mac: String,
    /// VLAN.
    pub vlan: Option<u16>,
    /// Bridge port.
    pub port: String,
    /// Seconds since the entry was last updated.
    pub age_secs: Option<u64>,
    /// The site behind a tunnel port; `None` for a port of this site.
    #[serde(default)]
    pub peer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape() {
        let req = RequestEnvelope {
            id: 1,
            network: None,
            request: Request::Status,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"id":1,"method":"status"}"#
        );
        let req = RequestEnvelope {
            network: Some("lab".into()),
            ..req
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"id":1,"network":"lab","method":"status"}"#
        );
        let req: RequestEnvelope =
            serde_json::from_str(r#"{"id":2,"method":"unblock","params":{"port":"vx-a"}}"#)
                .unwrap();
        assert_eq!(
            req.request,
            Request::Unblock(UnblockParams {
                port: "vx-a".into()
            })
        );
        let req: RequestEnvelope = serde_json::from_str(
            r#"{"id":3,"method":"apply","params":{"network":"m","local":"l","timeout_secs":10}}"#,
        )
        .unwrap();
        match req.request {
            Request::Apply(p) => {
                assert_eq!(p.config.network, "m");
                assert_eq!(p.timeout_secs, Some(10));
                assert_eq!(p.base_revision, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn server_messages_are_distinguishable() {
        let ev = ServerMessage::Event {
            event: Event {
                time_ms: 1,
                level: Level::Warn,
                peer: Some("a".into()),
                message: "m".into(),
            },
        };
        let text = serde_json::to_string(&ev).unwrap();
        assert!(text.starts_with(r#"{"event":"#));
        assert_eq!(serde_json::from_str::<ServerMessage>(&text).unwrap(), ev);
        let resp = ServerMessage::Response(ResponseEnvelope {
            id: 4,
            result: None,
            error: Some(ApiError::new("nope")),
        });
        let text = serde_json::to_string(&resp).unwrap();
        assert_eq!(text, r#"{"id":4,"error":{"message":"nope"}}"#);
        assert_eq!(serde_json::from_str::<ServerMessage>(&text).unwrap(), resp);
    }
}
