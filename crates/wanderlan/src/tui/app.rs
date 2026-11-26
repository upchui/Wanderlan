//! Application state and logic of the TUI.
//!
//! There is no I/O in here: the event loop in `mod.rs` feeds [`Msg`]s
//! (keys, ticks, daemon answers and events) into [`App::update`] and executes
//! the [`Effect`]s it returns (connect, send a request, quit). This keeps all
//! behaviour unit-testable.

use std::time::Duration;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::collections::{BTreeMap, VecDeque};

use wanderlan_core::api::{
    ApiError, Applied, ApplyParams, ChangeSource, ConfigInput, ConfigTexts, Confirmed, Event,
    Interface, Invitation, InviteParams, Joined, MacEntry, PeerStatus, PendingChange, PortState,
    Request, ResetReport, SiteState, SiteSummary, Status, UnblockParams, VlanTraffic,
};
use wanderlan_core::config::{Diagnostic, VlanMap, has_errors, render_local, render_network};
use wanderlan_core::policy;
use wanderlan_core::topology::{self, tunnel_name};

use super::diff::{self, DiffLine};
use super::draft::{Draft, Running, TunnelRow, same_network};
use super::filter::Filter;
use super::form::{Form, FormEvent, FormKind};
use super::format;
use super::log::LogState;
use super::macs::MacState;
use super::ports::{PortPicker, PortTarget};
use super::traffic;
use super::ui::list;
use super::wizard::{self, Mode, Step, Wizard};
use crate::client::ClientError;
use crate::hostinfo;
use crate::service::Diagnosis;

/// Ticks (seconds) between connection attempts while the daemon is down.
pub const RETRY_TICKS: u32 = 2;
/// Ticks a message stays in the status line.
const MESSAGE_TICKS: u32 = 10;
/// Ticks between MAC table refreshes while the MAC view is open.
const MAC_REFRESH_TICKS: u64 = 2;
/// Ticks between refreshes of the traffic per VLAN while a peer or a VLAN
/// is shown (the daemon samples every 2 s).
const VLAN_REFRESH_TICKS: u64 = 2;
/// Ticks after which an unanswered status request is sent again.
const STATUS_TIMEOUT_TICKS: u64 = 5;
/// Ticks after which an unanswered request (config, MACs, interfaces,
/// VLAN traffic) may be sent again.
const REQUEST_TIMEOUT_TICKS: u64 = 5;
/// Seconds left at which the countdown banner turns red.
pub const URGENT_SECS: u64 = 10;

/// The views, reachable with the digit keys 1-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Sites and live peer status.
    Overview,
    /// Sites, network and local settings.
    Sites,
    /// Shared VLANs and mappings.
    Vlans,
    /// Learned MAC addresses.
    Macs,
    /// Event log.
    Log,
    /// Setup wizard.
    Setup,
    /// All settings of the network and of this host.
    Settings,
    /// The local internet breakouts of the sites, per VLAN.
    Breakout,
}

impl View {
    /// All views in tab order.
    pub const ALL: [Self; 8] = [
        Self::Overview,
        Self::Sites,
        Self::Vlans,
        Self::Macs,
        Self::Log,
        Self::Setup,
        Self::Breakout,
        Self::Settings,
    ];

    /// Tab title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Sites => "Sites",
            Self::Vlans => "VLANs",
            Self::Macs => "MACs",
            Self::Log => "Log",
            Self::Setup => "Setup",
            Self::Settings => "Settings",
            Self::Breakout => "Breakout",
        }
    }

    /// Position in [`View::ALL`].
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|v| *v == self)
            .unwrap_or_default()
    }

    fn offset(self, by: isize) -> Self {
        let n = Self::ALL.len().cast_signed();
        let i = (self.index().cast_signed() + by).rem_euclid(n);
        Self::ALL[i.cast_unsigned()]
    }
}

/// Text in front of the tabs.
pub const BRAND: &str = " wanderlan ";

/// One tab: its view, label, first and one-past-last column.
pub type Tab = (View, String, u16, u16);

/// The tabs of a tab bar `width` columns wide, and the brand in front of
/// them where it fits.
pub fn tab_layout(width: u16) -> (Option<&'static str>, Vec<Tab>) {
    let labels: Vec<(View, String)> = View::ALL
        .iter()
        .map(|&view| (view, format!(" {} {} ", view.index() + 1, view.title())))
        .collect();
    let total: usize = labels.iter().map(|(_, l)| l.len()).sum();
    let brand = (BRAND.len() + total <= usize::from(width)).then_some(BRAND);
    let mut x = u16::try_from(brand.map_or(0, str::len)).unwrap_or_default();
    let tabs = labels
        .into_iter()
        .map(|(view, label)| {
            let start = x;
            x += u16::try_from(label.len()).unwrap_or_default();
            (view, label, start, x)
        })
        .collect();
    (brand, tabs)
}

/// Connection to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    /// A connection attempt is running.
    Connecting,
    /// Connected.
    Connected,
    /// Not reachable; retrying.
    Down {
        /// Why (user-friendly message of the client).
        error: String,
        /// Ticks until the next attempt.
        retry_in: u32,
    },
}

/// Kind of a status line message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// Neutral.
    Info,
    /// Something worked.
    Success,
    /// Something failed.
    Error,
}

/// How many messages [`App::messages`] keeps.
pub const MESSAGE_HISTORY: usize = 20;

/// A message in the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Kind.
    pub kind: MessageKind,
    /// Text.
    pub text: String,
    ttl: u32,
}

/// A request that is running and blocks repeating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// `apply` was sent.
    Applying,
    /// `confirm` was sent.
    Confirming,
    /// `rollback` was sent.
    RollingBack,
    /// `reset` was sent.
    Resetting,
}

/// What a confirmation question does when answered with yes.
#[derive(Debug, Clone, PartialEq)]
pub enum AskAction {
    /// Quit the TUI.
    Quit,
    /// Throw the draft away.
    Discard,
    /// Remove a site from the draft (by name: the draft may change while
    /// the question is open).
    DeleteSite(String),
    /// Remove a VLAN mapping from the draft (by its content).
    DeleteMap(wanderlan_core::config::VlanMap),
    /// Remove the breakout of a VLAN of a site from the draft.
    DeleteBreakout(String, u16),
    /// Re-enable a port blocked by the loop guard.
    Unblock(String),
    /// Remove everything wanderlan set up on this site.
    Reset,
    /// Roll the pending change back now.
    Rollback,
    /// Delete an open invitation (then back to the list).
    DeleteInvite {
        /// The code.
        code: String,
        /// Selected row of the list.
        selected: usize,
    },
    /// Close a form and lose its input (no: back to the form).
    DiscardInput(Box<Form>),
    /// Close the trunk port window and lose the choice (no: back to it).
    DiscardPorts(Box<PortPicker>),
}

impl AskAction {
    /// True if Enter means yes. Where a wrong yes loses something that
    /// cannot be had back easily, Enter means no and only `y` is yes.
    pub fn enter_means_yes(&self) -> bool {
        matches!(
            self,
            Self::DeleteSite(_) | Self::DeleteMap(_) | Self::Unblock(_)
        )
    }
}

/// A yes/no question.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// Title of the popup.
    pub title: String,
    /// The question.
    pub text: String,
    /// What "yes" does.
    pub action: AskAction,
}

/// Where a reviewed configuration comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The draft.
    Draft,
    /// The setup wizard.
    Wizard,
}

/// Progress of a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewState {
    /// Showing the diff.
    Ready,
    /// `apply` is running.
    Applying,
    /// The daemon refused the change.
    Failed(ApiError),
    /// Applied; waiting for confirmation.
    Applied(Applied),
}

/// The diff/apply screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    /// Source of the configuration.
    pub origin: Origin,
    /// `network.toml` to apply.
    pub network: String,
    /// `local.toml` to apply.
    pub local: String,
    /// Revision the change is based on.
    pub base_revision: u64,
    /// Time to confirm after applying.
    pub commit_timeout: Duration,
    /// Diff against the running configuration.
    pub lines: Vec<DiffLine>,
    /// Validation problems.
    pub problems: Vec<Diagnostic>,
    /// First visible diff line.
    pub scroll: u16,
    /// Progress.
    pub state: ReviewState,
}

impl Review {
    /// True if the configuration has no errors.
    pub fn is_valid(&self) -> bool {
        !has_errors(&self.problems)
    }

    fn scroll_key(&mut self, code: KeyCode) -> bool {
        // What is shown: after applying the changes made, before it the diff.
        let shown = match &self.state {
            ReviewState::Applied(a) => a.changes.len().max(1) + 2 + a.warnings.len(),
            _ => self.lines.len(),
        };
        let max = u16::try_from(shown.saturating_sub(1)).unwrap_or(u16::MAX);
        self.scroll = match code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll.saturating_add(10),
            KeyCode::Home => 0,
            KeyCode::End => max,
            _ => return false,
        }
        .min(max);
        true
    }
}

/// A popup on top of the current view.
#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    /// The help screen.
    Help {
        /// First visible line.
        scroll: u16,
    },
    /// A form editing the draft.
    Form(Form),
    /// Diff and apply.
    Review(Review),
    /// A yes/no question.
    Ask(Ask),
    /// The open invitations of this site.
    Invites {
        /// Selected row.
        selected: usize,
    },
    /// The last messages of the status line.
    Messages {
        /// First visible line.
        scroll: u16,
    },
    /// Everything about one site: role, VLANs, tunnels, mappings, rights.
    Site {
        /// The site.
        name: String,
        /// First visible line (PgUp/PgDn).
        scroll: u16,
    },
    /// Traffic and details of one peer, with its VLANs.
    Peer {
        /// The peer.
        name: String,
        /// The chosen VLAN of its list (`None`: the first row).
        vlan: Option<u16>,
    },
    /// The live traffic of one VLAN on every port of this site.
    Vlan {
        /// The VLAN (shared ID).
        vid: u16,
        /// The peer whose details Esc goes back to.
        back: Option<String>,
    },
    /// Choosing trunk ports from the interfaces of this host.
    Ports(PortPicker),
}

impl Modal {
    /// True for a window that only shows something: closing it loses no
    /// input.
    pub fn loses_nothing(&self) -> bool {
        !matches!(
            self,
            Self::Form(_) | Self::Ask(_) | Self::Review(_) | Self::Ports(_)
        )
    }
}

/// Samples kept per peer for the traffic graphs (one per status answer).
pub const HISTORY_LEN: usize = 120;

/// One sample of a peer's traffic.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    /// Receive rate in bit/s.
    pub rx_bps: u64,
    /// Transmit rate in bit/s.
    pub tx_bps: u64,
    /// Round-trip time in milliseconds.
    pub rtt_ms: Option<f64>,
}

/// A typed answer of the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Answer to `status`.
    Status(Box<Status>),
    /// Answer to `subscribe`: the history.
    History(Vec<Event>),
    /// Answer to `get_config`.
    Config(ConfigTexts),
    /// Answer to `apply`.
    Applied(Applied),
    /// Answer to `confirm`.
    Confirmed(Confirmed),
    /// Answer to `interfaces`.
    Interfaces(Vec<Interface>),
    /// Answer to `mac_table`.
    Macs(Vec<MacEntry>),
    /// Answer to `vlan_traffic`.
    VlanTraffic(VlanTraffic),
    /// Answer to `invite`.
    Invitation(Invitation),
    /// Answer to `invites`.
    Invites(Vec<Invitation>),
    /// Answer to `join`.
    Joined(Joined),
    /// Answer to `reset`.
    Reset(ResetReport),
    /// An empty answer.
    Done,
}

/// Input of [`App::update`].
#[derive(Debug)]
pub enum Msg {
    /// A key press.
    Key(KeyEvent),
    /// A mouse event (only with `--mouse`).
    Mouse(MouseEvent),
    /// One second passed.
    Tick,
    /// The connection to the daemon is established.
    Connected,
    /// Connecting failed.
    ConnectFailed(ClientError),
    /// The connection was lost.
    Disconnected(ClientError),
    /// The daemon answered a request.
    Response {
        /// The request.
        request: Request,
        /// The answer.
        result: Result<Reply, ClientError>,
    },
    /// A live event from the daemon.
    Event(Event),
    /// What systemd knows about the daemon (while it is not reachable).
    Diagnosis(Diagnosis),
}

/// Output of [`App::update`], executed by the event loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Connect to the daemon.
    Connect,
    /// Drop the connection (it broke).
    Disconnect,
    /// Send a request.
    Request(Request),
    /// Quit the TUI.
    Quit,
    /// Ask systemd about the daemon (state and last log lines).
    Diagnose,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct InFlight {
    status: Option<u64>,
    // The tick a request was sent at; after REQUEST_TIMEOUT_TICKS without
    // an answer it may be sent again.
    config: Option<u64>,
    macs: Option<u64>,
    interfaces: Option<u64>,
    vlans: Option<u64>,
}

/// Scroll positions of the lists (see [`list::Scroll`]).
#[derive(Debug, Clone, Default)]
pub struct Scrolls {
    /// Overview: direct peers.
    pub peers: list::Scroll,
    /// Overview: the sites of the network.
    pub network: list::Scroll,
    /// Sites view.
    pub sites: list::Scroll,
    /// VLAN mappings.
    pub maps: list::Scroll,
    /// Tunnels in the VLAN view.
    pub tunnels: list::Scroll,
    /// MAC table.
    pub macs: list::Scroll,
    /// Settings view.
    pub settings: list::Scroll,
    /// Open invitations.
    pub invites: list::Scroll,
    /// Interfaces in the trunk port window.
    pub ports: list::Scroll,
    /// The VLANs in the peer details.
    pub peer_vlans: list::Scroll,
    /// The ports in the VLAN window.
    pub vlan_ports: list::Scroll,
    /// The breakouts.
    pub breakouts: list::Scroll,
    /// Largest scroll offset of the help screen at its last size.
    pub help_max: std::cell::Cell<u16>,
    /// Largest scroll offset of the site details at their last size.
    pub site_max: std::cell::Cell<u16>,
    /// Keys of the help bar as drawn: first and last column, label.
    pub bar: std::cell::RefCell<Vec<(u16, u16, &'static str)>>,
    /// Screen row of the help bar.
    pub bar_row: std::cell::Cell<u16>,
    /// Width of the tab bar as drawn.
    pub tabs_width: std::cell::Cell<u16>,
}

/// A list the mouse can point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListId {
    Peers,
    Network,
    Sites,
    Maps,
    Tunnels,
    Macs,
    Invites,
    Ports,
    PeerVlans,
    Breakouts,
}

/// The key a label of the help bar stands for; `None` for labels that
/// name several keys (Up/Down, 1-8, ...).
fn bar_key(label: &str) -> Option<KeyCode> {
    let first = label.split('/').next().unwrap_or(label);
    match first {
        "Enter" => Some(KeyCode::Enter),
        "Space" => Some(KeyCode::Char(' ')),
        "Esc" => Some(KeyCode::Esc),
        "Tab" => Some(KeyCode::Tab),
        "End" => Some(KeyCode::End),
        "Home" => Some(KeyCode::Home),
        _ if first.chars().count() == 1 => first.chars().next().map(KeyCode::Char),
        _ => None,
    }
}

/// Moves a selection in a list of `count` rows: Up/Down (j/k) one row,
/// PgUp/PgDn a `page`, Home/End to the ends. `None` if `code` does not
/// move.
pub fn nav(code: KeyCode, selected: usize, count: usize, page: usize) -> Option<usize> {
    let last = count.saturating_sub(1);
    Some(match code {
        KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
        KeyCode::PageUp => selected.saturating_sub(page.max(1)),
        KeyCode::PageDown => (selected + page.max(1)).min(last),
        KeyCode::Home => 0,
        KeyCode::End => last,
        _ => return None,
    })
}

/// The whole state of the TUI.
#[derive(Debug, Clone)]
pub struct App {
    /// Socket path, for messages.
    pub socket: String,
    /// Mouse support enabled.
    pub mouse: bool,
    /// Current view.
    pub view: View,
    /// Connection state.
    pub conn: Conn,
    /// Failed connection attempts in a row.
    pub attempts: u32,
    /// Last status.
    pub status: Option<Status>,
    /// Running configuration.
    pub running: Option<Running>,
    /// The draft (none for an unconfigured site).
    pub draft: Option<Draft>,
    /// Interfaces of the host (wizard, local settings).
    pub interfaces: Vec<Interface>,
    /// True once the interface list arrived.
    pub interfaces_loaded: bool,
    /// Error listing the interfaces.
    pub interfaces_error: Option<String>,
    /// What systemd says about the daemon while it is not reachable.
    pub diagnosis: Option<Diagnosis>,
    /// Open the invitation form once the configuration of a new network
    /// arrived.
    invite_after_setup: bool,
    /// The event log.
    pub log: LogState,
    /// The MAC table.
    pub macs: MacState,
    /// Selected peer in the overview.
    pub overview_selected: usize,
    /// Selected site in the sites view.
    pub sites_selected: usize,
    /// Selected mapping in the VLAN view.
    pub maps_selected: usize,
    /// Selected row in the breakout view.
    pub breakouts_selected: usize,
    /// The VLAN view shows the tunnels instead of the mappings (`t`).
    pub vlans_tunnels: bool,
    /// Search in the Sites view.
    pub sites_filter: Filter,
    /// Search in the VLAN mappings.
    pub maps_filter: Filter,
    /// Search in the tunnels.
    pub tunnels_filter: Filter,
    /// Selected tunnel in the VLAN view.
    pub tunnels_selected: usize,
    /// The setup wizard.
    pub wizard: Wizard,
    /// The invitation created last (shown in the overview while open).
    pub invitation: Option<Invitation>,
    /// The open invitations (list popup).
    pub invitations: Vec<Invitation>,
    /// The code to select once the list arrived (a new invitation).
    select_invite: Option<String>,
    /// The invitation form while its request runs (back if refused).
    invite_pending: Option<Form>,
    /// Traffic history per peer, oldest first.
    pub history: BTreeMap<String, VecDeque<Sample>>,
    /// The traffic per VLAN and port, as read last.
    pub vlan_traffic: Option<VlanTraffic>,
    /// Why it could not be read.
    pub vlan_traffic_error: Option<String>,
    /// Traffic history per VLAN (all ports), oldest first.
    pub vlan_history: BTreeMap<u16, VecDeque<Sample>>,
    /// Selected row in the settings view.
    pub settings_selected: usize,
    /// Popup, if any.
    pub modal: Option<Modal>,
    /// Scroll positions of the lists.
    pub scrolls: Scrolls,
    /// Status line message.
    pub message: Option<Message>,
    /// The last messages, newest first (`M`).
    pub messages: VecDeque<Message>,
    /// A running confirm/rollback/apply.
    pub busy: Option<Busy>,
    /// The terminal is smaller than the views need: only q and Ctrl-C act.
    pub too_small: bool,
    /// The draft that was applied last, until the change is confirmed:
    /// back into the draft if it is rolled back.
    applied_draft: Option<Draft>,
    /// The pending change ended without a confirm from here: once the
    /// configuration arrived, the applied draft comes back if it is not in
    /// effect (with this message).
    restore_draft: Option<String>,
    /// The review waits for a change from a peer or a join to settle,
    /// then applies.
    apply_when_settled: bool,
    /// A join to this site ended; the next configuration tells how.
    join_ended: Option<String>,
    wizard_offered: bool,
    in_flight: InFlight,
    seen: Option<(u64, String, bool, SiteState)>,
    ticks: u64,
}

impl App {
    /// Creates the app; call [`App::start`] next.
    pub fn new(socket: impl Into<String>, mouse: bool) -> Self {
        Self {
            socket: socket.into(),
            mouse,
            view: View::Overview,
            conn: Conn::Connecting,
            attempts: 0,
            status: None,
            running: None,
            draft: None,
            interfaces: Vec::new(),
            interfaces_loaded: false,
            interfaces_error: None,
            diagnosis: None,
            invite_after_setup: false,
            log: LogState::default(),
            macs: MacState::default(),
            overview_selected: 0,
            sites_selected: 0,
            maps_selected: 0,
            breakouts_selected: 0,
            vlans_tunnels: false,
            sites_filter: Filter::default(),
            maps_filter: Filter::default(),
            tunnels_filter: Filter::default(),
            tunnels_selected: 0,
            wizard: Wizard::new(),
            invitation: None,
            invitations: Vec::new(),
            select_invite: None,
            invite_pending: None,
            history: BTreeMap::new(),
            vlan_traffic: None,
            vlan_traffic_error: None,
            vlan_history: BTreeMap::new(),
            settings_selected: 0,
            modal: None,
            scrolls: Scrolls::default(),
            message: None,
            messages: VecDeque::new(),
            busy: None,
            too_small: false,
            applied_draft: None,
            restore_draft: None,
            apply_when_settled: false,
            join_ended: None,
            wizard_offered: false,
            in_flight: InFlight::default(),
            seen: None,
            ticks: 0,
        }
    }

    /// The first effects: connect.
    pub fn start(&mut self) -> Vec<Effect> {
        self.conn = Conn::Connecting;
        vec![Effect::Connect]
    }

    /// Processes one message.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        let mut fx = Vec::new();
        match msg {
            Msg::Key(key) => self.on_key(&key, &mut fx),
            Msg::Mouse(mouse) => self.on_mouse(mouse, &mut fx),
            Msg::Tick => self.on_tick(&mut fx),
            Msg::Connected => self.on_connected(&mut fx),
            Msg::ConnectFailed(e) | Msg::Disconnected(e) => self.on_down(&e, &mut fx),
            Msg::Diagnosis(d) => self.diagnosis = Some(d),
            Msg::Response { request, result } => self.on_response(request, result, &mut fx),
            Msg::Event(event) => self.log.push(event),
        }
        fx
    }

    /// True while connected.
    pub fn is_connected(&self) -> bool {
        self.conn == Conn::Connected
    }

    /// True if the draft differs from the running configuration.
    pub fn draft_dirty(&self) -> bool {
        match (&self.draft, &self.running) {
            (Some(d), Some(r)) => d.differs_from(r),
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// True if the running configuration moved on since the draft was made.
    pub fn draft_stale(&self) -> bool {
        match (&self.draft, &self.running) {
            (Some(d), Some(r)) => d.base_revision != r.revision,
            _ => false,
        }
    }

    /// The change waiting for confirmation.
    pub fn pending(&self) -> Option<&PendingChange> {
        self.status.as_ref().and_then(|s| s.pending.as_ref())
    }

    /// Live data of the tunnel to a site.
    pub fn peer(&self, site: &str) -> Option<&PeerStatus> {
        self.status
            .as_ref()
            .and_then(|s| s.peers.iter().find(|p| p.name == site))
    }

    /// The peer selected in the overview.
    pub fn selected_peer(&self) -> Option<&PeerStatus> {
        self.status
            .as_ref()
            .and_then(|s| s.peers.get(self.overview_selected))
    }

    pub(crate) fn say(&mut self, kind: MessageKind, text: impl Into<String>) {
        let message = Message {
            kind,
            text: text.into(),
            ttl: MESSAGE_TICKS,
        };
        if self.messages.front().map(|m| &m.text) != Some(&message.text) {
            self.messages.push_front(message.clone());
            self.messages.truncate(MESSAGE_HISTORY);
        }
        self.message = Some(message);
    }

    // ----- connection -------------------------------------------------

    fn on_connected(&mut self, fx: &mut Vec<Effect>) {
        self.conn = Conn::Connected;
        self.attempts = 0;
        self.diagnosis = None;
        self.in_flight = InFlight::default();
        self.seen = None;
        self.log.resubscribe();
        self.request_status(fx);
        self.request_config(fx);
        fx.push(Effect::Request(Request::Subscribe));
        if self.view == View::Setup || matches!(self.modal, Some(Modal::Ports(_))) {
            self.request_interfaces(fx);
        }
        // Nothing sent before is answered on the new connection.
        if self.busy.take().is_some()
            && let Some(Modal::Review(review)) = &mut self.modal
            && review.state == ReviewState::Applying
        {
            review.state = ReviewState::Failed(ApiError::new(
                "the connection to the daemon was lost; check the overview",
            ));
        }
    }

    fn on_down(&mut self, error: &ClientError, fx: &mut Vec<Effect>) {
        self.attempts += 1;
        // Its answer never comes on this connection.
        if let Some(mut form) = self.invite_pending.take() {
            form.set_note_error(
                "the connection to the daemon was lost: look at the invitations (I) once connected before creating it again"
                    .into(),
            );
            if self.modal.as_ref().is_none_or(Modal::loses_nothing) {
                self.modal = Some(Modal::Form(form));
            } else {
                self.invite_pending = Some(form);
            }
        }
        if self.attempts == 1 || self.attempts.is_multiple_of(5) {
            fx.push(Effect::Diagnose);
        }
        self.conn = Conn::Down {
            error: error.to_string(),
            retry_in: RETRY_TICKS,
        };
        self.in_flight = InFlight::default();
        if self.busy.take().is_some()
            && let Some(Modal::Review(review)) = &mut self.modal
            && review.state == ReviewState::Applying
        {
            review.state = ReviewState::Failed(ApiError::new(
                "the connection to the daemon was lost; check the overview after reconnecting",
            ));
        }
        if self.wizard.step == Step::Joining {
            self.wizard.step = Step::Summary;
            self.wizard.error = Some(format!(
                "the connection to the daemon was lost while joining ({error}); check the overview, then try again"
            ));
        }
    }

    fn on_tick(&mut self, fx: &mut Vec<Effect>) {
        self.ticks += 1;
        if let Some(m) = &mut self.message {
            m.ttl = m.ttl.saturating_sub(1);
            if m.ttl == 0 {
                self.message = None;
            }
        }
        match &mut self.conn {
            Conn::Down { retry_in, .. } => {
                *retry_in = retry_in.saturating_sub(1);
                if *retry_in == 0 {
                    self.conn = Conn::Connecting;
                    fx.push(Effect::Connect);
                }
            }
            Conn::Connected => {
                let stale = self
                    .in_flight
                    .status
                    .is_none_or(|sent| self.ticks.saturating_sub(sent) >= STATUS_TIMEOUT_TICKS);
                if stale {
                    self.request_status(fx);
                }
                let vlan_open = matches!(self.modal, Some(Modal::Vlan { .. }));
                if (self.view == View::Macs || vlan_open)
                    && self.ticks.is_multiple_of(MAC_REFRESH_TICKS)
                {
                    self.request_macs(fx);
                }
                if matches!(self.modal, Some(Modal::Peer { .. } | Modal::Vlan { .. }))
                    && self.ticks.is_multiple_of(VLAN_REFRESH_TICKS)
                {
                    self.request_vlan_traffic(fx);
                }
            }
            Conn::Connecting => {}
        }
    }

    fn request_status(&mut self, fx: &mut Vec<Effect>) {
        self.in_flight.status = Some(self.ticks);
        fx.push(Effect::Request(Request::Status));
    }

    fn request_config(&mut self, fx: &mut Vec<Effect>) {
        if self.may_send(self.in_flight.config) {
            self.in_flight.config = Some(self.ticks);
            fx.push(Effect::Request(Request::GetConfig));
        }
    }

    fn request_macs(&mut self, fx: &mut Vec<Effect>) {
        if self.is_connected() && self.may_send(self.in_flight.macs) {
            self.in_flight.macs = Some(self.ticks);
            fx.push(Effect::Request(Request::MacTable));
        }
    }

    /// True if a request sent at `sent` (if at all) may be sent again.
    fn may_send(&self, sent: Option<u64>) -> bool {
        sent.is_none_or(|t| self.ticks.saturating_sub(t) >= REQUEST_TIMEOUT_TICKS)
    }

    fn request_vlan_traffic(&mut self, fx: &mut Vec<Effect>) {
        if self.is_connected() && self.may_send(self.in_flight.vlans) {
            self.in_flight.vlans = Some(self.ticks);
            fx.push(Effect::Request(Request::VlanTraffic));
        }
    }

    fn request_interfaces(&mut self, fx: &mut Vec<Effect>) {
        if self.is_connected() && self.may_send(self.in_flight.interfaces) {
            self.in_flight.interfaces = Some(self.ticks);
            fx.push(Effect::Request(Request::Interfaces));
        }
    }

    // ----- daemon answers ---------------------------------------------

    fn on_response(
        &mut self,
        request: Request,
        result: Result<Reply, ClientError>,
        fx: &mut Vec<Effect>,
    ) {
        if let Err(ClientError::Closed) = result {
            self.on_down(&ClientError::Closed, fx);
            fx.push(Effect::Disconnect);
            return;
        }
        match (request, result) {
            (_, Ok(Reply::Status(status))) => self.on_status(*status, fx),
            (_, Ok(Reply::Config(texts))) => self.on_config(&texts, fx),
            (_, Ok(Reply::History(events))) => self.log.set_history(events),
            (_, Ok(Reply::Interfaces(list))) => self.on_interfaces(list),
            (_, Ok(Reply::Macs(entries))) => {
                self.in_flight.macs = None;
                self.macs.set_entries(entries);
            }
            (_, Ok(Reply::VlanTraffic(traffic))) => self.on_vlan_traffic(traffic),
            (Request::Apply(_), Ok(Reply::Applied(applied))) => self.on_applied(applied, fx),
            (Request::Apply(_), Err(e)) => self.on_apply_failed(e, fx),
            (Request::Confirm, Ok(Reply::Confirmed(c))) => self.on_confirmed(&c, fx),
            (Request::Rollback, Ok(_)) => self.on_rolled_back(fx),
            (Request::Unblock(p), Ok(_)) => {
                self.say(
                    MessageKind::Success,
                    format!("{} is enabled again.", p.port),
                );
                self.request_status(fx);
            }
            (Request::ProbeNow, Ok(_)) => self.say(
                MessageKind::Info,
                "Measuring the path MTU to all peers; the overview updates shortly.",
            ),
            (Request::Invite(_), Ok(Reply::Invitation(inv))) => {
                self.invite_pending = None;
                self.on_invitation(inv, fx);
            }
            (_, Ok(Reply::Invites(list))) => {
                self.invitations = list;
                // A used or deleted code is no longer shown anywhere; an
                // open one with its command as it is now (another port).
                self.invitation = self
                    .invitation
                    .take()
                    .and_then(|i| self.invitations.iter().find(|o| o.code == i.code).cloned());
                let wanted = self
                    .select_invite
                    .take()
                    .and_then(|code| self.invitations.iter().position(|i| i.code == code));
                if let Some(Modal::Invites { selected }) = &mut self.modal {
                    *selected = wanted
                        .unwrap_or(*selected)
                        .min(self.invitations.len().saturating_sub(1));
                }
            }
            (Request::DeleteInvite(p), Ok(_)) => {
                if self.invitation.as_ref().is_some_and(|i| i.code == p.code) {
                    self.invitation = None;
                }
                self.say(
                    MessageKind::Success,
                    format!("Invitation {} deleted.", p.code),
                );
                fx.push(Effect::Request(Request::Invites));
                self.request_status(fx);
            }
            (Request::Join(_), Ok(Reply::Joined(joined))) => self.on_joined(&joined, fx),
            (Request::Reset, Ok(Reply::Reset(report))) => self.on_reset(&report, fx),
            (Request::Reset, Err(e)) => {
                // Whatever happened, show the real state of the site.
                self.busy = None;
                self.say(MessageKind::Error, format!("reset failed: {e}"));
                if self.is_connected() {
                    self.request_status(fx);
                    self.request_config(fx);
                    self.request_interfaces(fx);
                }
            }
            (request, Err(e)) => {
                let ended = matches!(request, Request::Confirm | Request::Rollback);
                self.on_request_failed(&request, &e);
                if ended {
                    self.request_status(fx);
                    self.request_config(fx);
                }
            }
            (_, Ok(_)) => {}
        }
    }

    fn on_request_failed(&mut self, request: &Request, error: &ClientError) {
        let what = match request {
            Request::Status => {
                self.in_flight.status = None;
                "status"
            }
            Request::GetConfig => {
                self.in_flight.config = None;
                "reading the configuration"
            }
            Request::MacTable => {
                self.in_flight.macs = None;
                self.macs.error = Some(error.to_string());
                return;
            }
            Request::VlanTraffic => {
                self.in_flight.vlans = None;
                self.vlan_traffic_error = Some(error.to_string());
                return;
            }
            Request::Interfaces => {
                self.in_flight.interfaces = None;
                self.interfaces_error = Some(error.to_string());
                return;
            }
            // The change may have ended meanwhile (the countdown ran out):
            // what runs now tells.
            Request::Confirm | Request::Rollback => {
                self.busy = None;
                if self.applied_draft.is_some() {
                    self.restore_draft = Some(
                        "Your change was rolled back (not confirmed in time). It is kept in the draft: a applies it again, x discards it."
                            .into(),
                    );
                }
                self.in_flight.config = None;
                if matches!(request, Request::Confirm) {
                    "confirm"
                } else {
                    "rollback"
                }
            }
            Request::Subscribe => "log subscription",
            Request::Unblock(_) => "unblock",
            Request::ProbeNow => "MTU probe",
            Request::Validate(_) | Request::Apply(_) | Request::ApplyFiles(_) => "apply",
            Request::Invite(_) => {
                if let Some(mut form) = self.invite_pending.take() {
                    form.set_note_error(match error {
                        ClientError::Api(e) => e.message.clone(),
                        other => other.to_string(),
                    });
                    // Over a window that loses nothing; else kept for i.
                    if self.modal.as_ref().is_none_or(Modal::loses_nothing) {
                        self.modal = Some(Modal::Form(form));
                        return;
                    }
                    self.invite_pending = Some(form);
                    self.say(
                        MessageKind::Error,
                        format!("The invitation was refused: {error}. i shows the form again."),
                    );
                    return;
                }
                "invitation"
            }
            Request::Invites | Request::DeleteInvite(_) => "invitation",
            Request::Join(_) => {
                let text = match error {
                    ClientError::Api(e) => e.message.clone(),
                    other => other.to_string(),
                };
                // Back to the step that fixes it.
                self.wizard.step = if text.contains("another name")
                    || text.contains("already site")
                    || text.contains("site name")
                {
                    Step::Name
                } else {
                    Step::Summary
                };
                self.wizard.error = Some(text);
                return;
            }
            Request::Reset => {
                self.busy = None;
                "reset"
            }
        };
        self.say(MessageKind::Error, format!("{what} failed: {error}"));
    }

    fn on_status(&mut self, status: Status, fx: &mut Vec<Effect>) {
        self.in_flight.status = None;
        // Any change of what runs, also of local.toml alone (by the CLI).
        let seen = (
            status.revision,
            format!("{}{}", status.fingerprint, status.local_fingerprint),
            status.pending.is_some(),
            status.state,
        );
        if self.seen.as_ref() != Some(&seen) {
            if self.seen.is_some() {
                self.request_config(fx);
            }
            self.seen = Some(seen);
        }
        let had_pending = self.pending().is_some();
        if had_pending && status.pending.is_none() {
            // A join ended: whether the site is there the configuration
            // tells (requested with the status change).
            if let Some(ChangeSource::Join { site }) = self.pending().map(|p| p.source.clone()) {
                self.join_ended = Some(site);
            }
            if self.busy.is_none() {
                self.on_pending_ended(status.state, fx);
            }
        }
        if status.invites == 0 {
            self.invitation = None;
        }
        // A code was used or deleted elsewhere: which one the list tells.
        let fewer = self
            .status
            .as_ref()
            .is_some_and(|old| status.invites < old.invites);
        if fewer && self.invitation.is_some() {
            fx.push(Effect::Request(Request::Invites));
        }
        self.record_history(&status);
        if !self.wizard_offered {
            self.wizard_offered = true;
            if status.state == SiteState::Unconfigured {
                self.view = View::Setup;
                self.wizard
                    .prefill(&hostinfo::hostname(), hostinfo::default_address());
                self.request_interfaces(fx);
            }
        }
        self.overview_selected = self
            .overview_selected
            .min((status.peers.len() + status.sites.len()).saturating_sub(1));
        self.status = Some(status);
    }

    /// The pending change disappeared without a confirm/rollback from here:
    /// the timer ran out or someone else decided.
    fn on_pending_ended(&mut self, state: SiteState, fx: &mut Vec<Effect>) {
        if let Some(Modal::Review(review)) = &self.modal
            && matches!(review.state, ReviewState::Applied(_))
        {
            self.modal = None;
        }
        if self.wizard.step == Step::Applied {
            if state == SiteState::Unconfigured {
                self.wizard.step = Step::Summary;
                self.say(
                    MessageKind::Error,
                    "The setup was rolled back because it was not confirmed in time.",
                );
            } else {
                self.setup_done(fx);
            }
            return;
        }
        if self.applied_draft.is_some() {
            // Ours: the configuration (requested with the status) tells
            // whether it is still in effect.
            self.restore_draft = Some(
                "Your change was rolled back (not confirmed in time). It is kept in the draft: a applies it again, x discards it."
                    .into(),
            );
            self.request_config(fx);
            return;
        }
        self.say(
            MessageKind::Info,
            "The pending change was confirmed or rolled back (see the log).",
        );
    }

    /// Our own confirm/rollback ended the pending change.
    fn clear_pending(&mut self) {
        if let Some(status) = &mut self.status {
            status.pending = None;
        }
    }

    fn on_config(&mut self, texts: &ConfigTexts, fx: &mut Vec<Effect>) {
        self.in_flight.config = None;
        let running = match Running::parse(texts) {
            Ok(r) => r,
            Err(e) => {
                self.say(MessageKind::Error, e);
                return;
            }
        };
        let keep = match (&self.draft, &self.running) {
            (Some(d), Some(old)) => d.differs_from(old) && d.differs_from(&running),
            _ => false,
        };
        // The selected rows, to find them again in the new draft.
        let selection = self.selection();
        let before = self.draft.clone();
        let restored = match (self.restore_draft.take(), self.applied_draft.take()) {
            // Not in effect: rolled back; the edits come back, with what
            // was edited during the countdown (the draft builds on it).
            (Some(message), Some(applied)) if applied.differs_from(&running) => {
                let edited_since = self.draft.as_ref().is_some_and(|d| {
                    !same_network(&d.network, &applied.network) || d.local != applied.local
                });
                let mut back = match &self.draft {
                    Some(d) if edited_since => d.clone(),
                    _ => applied,
                };
                back.rebase(&running);
                self.say(MessageKind::Info, message);
                Some(back)
            }
            (Some(_), Some(_)) => {
                self.say(MessageKind::Success, "Your change was confirmed.");
                None
            }
            (None, applied) => {
                self.applied_draft = applied;
                None
            }
            (Some(_), None) => None,
        };
        // Changed in content (not only a new revision, e.g. by a confirm);
        // local.toml alone counts too (`wanderlan settings`).
        let moved_on = self.running.as_ref().is_some_and(|old| {
            old.local != running.local
                || match (&old.network, &running.network) {
                    (Some(a), Some(b)) => !same_network(a, b),
                    (a, b) => a.is_some() != b.is_some(),
                }
        });
        if let Some(draft) = restored {
            self.draft = Some(draft);
        } else if !keep {
            self.draft = Draft::from_running(&running);
        } else if !moved_on && let Some(draft) = &mut self.draft {
            // Only a new revision: the draft builds on it.
            draft.rebase(&running);
        } else if let Some(draft) = &mut self.draft {
            // Changed elsewhere while edited here: the edits move onto it,
            // so the review shows only them and the apply fits.
            let both = draft.rebase_onto(&running);
            let text = if both.is_empty() {
                format!(
                    "The configuration changed (revision {}) while you edited; your draft now builds on it.",
                    running.revision
                )
            } else {
                format!(
                    "The configuration changed (revision {}) while you edited; your draft now builds on it, your edits of {} replace the newer ones.",
                    running.revision,
                    both.join(", ")
                )
            };
            self.say(MessageKind::Info, text);
        }
        let configured = running.network.is_some();
        self.running = Some(running);
        self.restore_selection(&selection);
        self.clamp_selection();
        if self.draft != before {
            self.refresh_review();
        }
        self.apply_if_settled(fx);
        if let Some(site) = self.join_ended.take() {
            let joined = self
                .running
                .as_ref()
                .and_then(|r| r.network.as_ref())
                .and_then(|n| n.site(&site))
                .map(|s| s.online);
            match joined {
                Some(false) => self.say(
                    MessageKind::Success,
                    format!(
                        "{site} joined. It starts offline: o on it in the Overview takes it online (then Enter and c)."
                    ),
                ),
                Some(true) => self.say(MessageKind::Success, format!("{site} joined.")),
                None => self.say(
                    MessageKind::Info,
                    format!("The join of {site} did not complete (it did not answer in time); its code works again."),
                ),
            }
        }
        if configured && std::mem::take(&mut self.invite_after_setup) {
            self.modal = None;
            self.invite();
        }
    }

    /// The selected site, mapping and breakout, by name and content.
    fn selection(&self) -> (Option<String>, Option<VlanMap>, Option<(String, u16)>) {
        let Some(d) = &self.draft else {
            return (None, None, None);
        };
        (
            d.network
                .sites
                .get(self.sites_selected)
                .map(|s| s.name.clone()),
            d.network.maps.get(self.maps_selected).cloned(),
            self.breakout_rows().get(self.breakouts_selected).cloned(),
        )
    }

    /// Finds the selected rows again after the draft was replaced (a sync
    /// may have removed or added rows above them).
    fn restore_selection(
        &mut self,
        (site, map, breakout): &(Option<String>, Option<VlanMap>, Option<(String, u16)>),
    ) {
        let Some(d) = &self.draft else { return };
        if let Some(i) = site
            .as_ref()
            .and_then(|n| d.network.sites.iter().position(|s| s.name == *n))
        {
            self.sites_selected = i;
        }
        if let Some(i) = map
            .as_ref()
            .and_then(|m| d.network.maps.iter().position(|x| x == m))
        {
            self.maps_selected = i;
        }
        if let Some(i) = breakout
            .as_ref()
            .and_then(|b| self.breakout_rows().iter().position(|x| x == b))
        {
            self.breakouts_selected = i;
        }
    }

    /// The review waited for a change of the daemon's own to settle: it is
    /// done, so the draft (rebuilt on what runs now) is applied.
    fn apply_if_settled(&mut self, fx: &mut Vec<Effect>) {
        if !self.apply_when_settled || self.pending().is_some() || self.busy.is_some() {
            return;
        }
        self.apply_when_settled = false;
        if let Some(Modal::Review(mut review)) = self.modal.take() {
            if review.state == ReviewState::Ready {
                self.start_apply(&mut review, fx);
            }
            self.modal = Some(Modal::Review(review));
        }
    }

    /// An open review of the draft that has not been applied yet shows the
    /// draft as it is now (it was rebased or restored).
    fn refresh_review(&mut self) {
        if let Some(Modal::Review(r)) = &self.modal
            && r.origin == Origin::Draft
            && matches!(r.state, ReviewState::Ready | ReviewState::Failed(_))
        {
            self.modal = None;
            self.open_review();
        }
    }

    fn on_interfaces(&mut self, list: Vec<Interface>) {
        self.in_flight.interfaces = None;
        self.interfaces = list;
        self.interfaces_loaded = true;
        self.interfaces_error = None;
        // The list may be shorter now: the cursor stays on it ("other").
        if let (Some(Modal::Ports(picker)), Some(draft)) = (&mut self.modal, &self.draft) {
            let rows = picker.rows(draft, &self.interfaces).len();
            picker.cursor = picker.cursor.min(rows);
        }
    }

    fn on_applied(&mut self, applied: Applied, fx: &mut Vec<Effect>) {
        self.busy = None;
        let secs = applied.confirm_within_secs;
        let mut origin = Origin::Draft;
        if let Some(Modal::Review(review)) = &mut self.modal {
            origin = review.origin;
            review.state = ReviewState::Applied(applied);
            review.scroll = 0;
        }
        if origin == Origin::Wizard {
            self.wizard.step = Step::Applied;
        }
        self.say(
            MessageKind::Success,
            format!("Applied. Press c within {secs} s to keep it, otherwise it is rolled back."),
        );
        self.request_status(fx);
        self.request_config(fx);
    }

    fn on_apply_failed(&mut self, error: ClientError, fx: &mut Vec<Effect>) {
        self.busy = None;
        self.applied_draft = None;
        // Refused because the configuration moved on: the new one, so that
        // the draft builds on it.
        self.request_config(fx);
        let error = match error {
            ClientError::Api(e) => e,
            other => ApiError::new(other.to_string()),
        };
        match &mut self.modal {
            Some(Modal::Review(review)) => review.state = ReviewState::Failed(error),
            _ => self.say(MessageKind::Error, format!("apply failed: {error}")),
        }
    }

    fn on_confirmed(&mut self, confirmed: &Confirmed, fx: &mut Vec<Effect>) {
        self.busy = None;
        self.clear_pending();
        self.applied_draft = None;
        self.restore_draft = None;
        if self.wizard.step == Step::Applied {
            self.setup_done(fx);
        } else {
            self.say(
                MessageKind::Success,
                format!(
                    "Confirmed: network revision {} is in effect.",
                    confirmed.revision
                ),
            );
        }
        self.request_status(fx);
        self.request_config(fx);
    }

    /// The new network is confirmed: the invitation form for the next site
    /// opens as soon as the running configuration arrived (it needs it).
    fn setup_done(&mut self, fx: &mut Vec<Effect>) {
        self.wizard = Wizard::new();
        self.view = View::Sites;
        self.invite_after_setup = true;
        self.say(
            MessageKind::Success,
            "Setup complete. Invite the next site: its rules, and who may change them (Esc: later, i).",
        );
        self.request_config(fx);
    }

    fn on_invitation(&mut self, invitation: Invitation, fx: &mut Vec<Effect>) {
        self.say(
            MessageKind::Success,
            format!(
                "Invitation {} created; the list shows the command for the new site.",
                invitation.code
            ),
        );
        // The list with the new code selected: the command stays readable.
        if self.modal.is_none() {
            self.modal = Some(Modal::Invites { selected: 0 });
            self.select_invite = Some(invitation.code.clone());
            fx.push(Effect::Request(Request::Invites));
        }
        self.invitation = Some(invitation);
        self.request_status(fx);
    }

    /// Opens the form for a new invitation.
    fn invite(&mut self) {
        if !self.is_connected() {
            self.say(MessageKind::Error, "Not connected to the daemon.");
            return;
        }
        if self.refuse_while_busy() {
            return;
        }
        // A form that was refused (or cut off) comes back as it was.
        if let Some(form) = self.invite_pending.take() {
            self.modal = Some(Modal::Form(form));
            return;
        }
        // The rules are checked against what runs (not the draft): that is
        // what the new site joins.
        match self.running.as_ref().and_then(Draft::from_running) {
            Some(running) => self.modal = Some(Modal::Form(Form::invite(&running))),
            None => self.not_configured(),
        }
    }

    /// Opens the list of open invitations.
    fn show_invites(&mut self, fx: &mut Vec<Effect>) {
        if !self.is_connected() {
            self.say(MessageKind::Error, "Not connected to the daemon.");
            return;
        }
        self.modal = Some(Modal::Invites { selected: 0 });
        fx.push(Effect::Request(Request::Invites));
    }

    /// Stores the traffic per VLAN and records one sample per VLAN.
    fn on_vlan_traffic(&mut self, traffic: VlanTraffic) {
        self.in_flight.vlans = None;
        self.vlan_traffic_error = None;
        let totals = traffic::vlan_totals(&traffic);
        for (&vid, &(rx_bps, tx_bps)) in &totals {
            let samples = self.vlan_history.entry(vid).or_default();
            samples.push_back(Sample {
                rx_bps,
                tx_bps,
                rtt_ms: None,
            });
            while samples.len() > HISTORY_LEN {
                samples.pop_front();
            }
        }
        self.vlan_history.retain(|vid, _| totals.contains_key(vid));
        self.vlan_traffic = Some(traffic);
    }

    /// Opens the details of a peer and reads its VLANs.
    fn open_peer(&mut self, name: String, fx: &mut Vec<Effect>) {
        self.modal = Some(Modal::Peer { name, vlan: None });
        self.request_vlan_traffic(fx);
    }

    /// Records one traffic sample per peer from a status answer.
    fn record_history(&mut self, status: &Status) {
        for peer in &status.peers {
            let samples = self.history.entry(peer.name.clone()).or_default();
            samples.push_back(Sample {
                rx_bps: peer.rx_bps,
                tx_bps: peer.tx_bps,
                rtt_ms: peer.rtt_ms,
            });
            while samples.len() > HISTORY_LEN {
                samples.pop_front();
            }
        }
        self.history
            .retain(|name, _| status.peers.iter().any(|p| &p.name == name));
    }

    /// The site that manages this one, if this site is restricted: then it
    /// may not change VLAN rules (see [`policy`]).
    pub fn restricted_by(&self) -> Option<String> {
        let running = self.running.as_ref()?;
        let me = running.local.self_name.as_deref()?;
        policy::restricted_by(running.network.as_ref()?, me).map(str::to_string)
    }

    /// True if this site may change the rules of `site` (VLANs, mappings,
    /// online); otherwise says who may.
    /// True if this site may change `site` (as it runs: a site renamed in
    /// the draft by its old name).
    pub fn may_change_quietly(&self, site: &str) -> bool {
        let Some(running) = self.running.as_ref().and_then(|r| r.network.as_ref()) else {
            return true;
        };
        let me = self
            .running
            .as_ref()
            .and_then(|r| r.local.self_name.clone())
            .unwrap_or_default();
        match self.running_entry(site) {
            Some(entry) => policy::may_change(running, entry, &me),
            // New in the draft.
            None => true,
        }
    }

    /// The entry of `site` as it runs, also when the draft renamed it.
    fn running_entry(&self, site: &str) -> Option<&wanderlan_core::config::Site> {
        let running = self.running.as_ref()?.network.as_ref()?;
        if let Some(entry) = running.site(site) {
            return Some(entry);
        }
        let draft = self.draft.as_ref()?;
        let old = wanderlan_core::rename::renames(running, &draft.network)
            .into_iter()
            .find(|(_, new)| new == site)?
            .0;
        running.site(&old)
    }

    fn may_change(&mut self, site: &str) -> bool {
        let Some(running) = self.running.as_ref().and_then(|r| r.network.as_ref()) else {
            return true;
        };
        let Some(entry) = self.running_entry(site) else {
            return true;
        };
        let me = self
            .running
            .as_ref()
            .and_then(|r| r.local.self_name.clone())
            .unwrap_or_default();
        if policy::may_change(running, entry, &me) {
            return true;
        }
        let text = policy::why_not(running, entry, &me, "change");
        self.say(MessageKind::Error, text);
        false
    }

    /// True if this site may change the VLANs of the tunnel between `a` and
    /// `b`; otherwise says why not.
    fn may_change_tunnel(&mut self, a: &str, b: &str) -> bool {
        let Some(running) = self.running.as_ref() else {
            return true;
        };
        let (Some(network), Some(me)) =
            (running.network.as_ref(), running.local.self_name.as_deref())
        else {
            return true;
        };
        if policy::may_change_tunnel(network, a, b, me) {
            return true;
        }
        let text = policy::why_not_tunnel(network, a, b, me);
        self.say(MessageKind::Error, text);
        false
    }

    /// True if this site may change the VLANs of the network; otherwise
    /// says why not.
    fn may_change_network_vlans(&mut self) -> bool {
        let Some(running) = self.running.as_ref() else {
            return true;
        };
        let (Some(network), Some(me)) =
            (running.network.as_ref(), running.local.self_name.as_deref())
        else {
            return true;
        };
        if policy::may_change_network_vlans(network, me) {
            return true;
        }
        let text = policy::why_not_network(network, me);
        self.say(MessageKind::Error, text);
        false
    }

    /// Toggles online/offline of the selected site in the draft.
    fn toggle_online(&mut self) {
        let Some(name) = self
            .draft
            .as_ref()
            .and_then(|d| d.network.sites.get(self.sites_selected))
            .map(|s| s.name.clone())
        else {
            return;
        };
        if !self.may_change(&name) {
            return;
        }
        let Some(draft) = &mut self.draft else { return };
        let Some(site) = draft.network.sites.iter_mut().find(|s| s.name == name) else {
            return;
        };
        site.online = !site.online;
        let state = if site.online { "online" } else { "offline" };
        draft.revalidate();
        self.say(
            MessageKind::Info,
            format!("{name} goes {state} when the draft is applied (a)."),
        );
    }

    /// True if this site may rename `site`; otherwise says why not.
    fn may_rename(&mut self, site: &str) -> bool {
        let Some(running) = self.running.as_ref().and_then(|r| r.network.as_ref()) else {
            return true;
        };
        if running.site(site).is_none() {
            return true;
        }
        let me = self
            .running
            .as_ref()
            .and_then(|r| r.local.self_name.clone())
            .unwrap_or_default();
        if policy::may_rename(running, site, &me) {
            return true;
        }
        let text = policy::why_not_rename(running, site, &me);
        self.say(MessageKind::Error, text);
        false
    }

    fn start_join(&mut self, fx: &mut Vec<Effect>) {
        match self.wizard.join_params(&self.interfaces) {
            Ok(params) => fx.push(Effect::Request(Request::Join(params))),
            Err(e) => {
                self.wizard.step = Step::Summary;
                self.wizard.error = Some(e);
            }
        }
    }

    fn on_joined(&mut self, joined: &Joined, fx: &mut Vec<Effect>) {
        self.wizard = Wizard::new();
        self.view = View::Overview;
        self.say(
            MessageKind::Success,
            format!(
                "Joined via {via} as {site} ({} sites); confirmed automatically as soon as {via} answers. {site} starts offline: its tunnel stays down until {via} takes it online (there: o on {site} in the Overview, then Enter and c).",
                joined.sites,
                via = joined.via,
                site = joined.site,
            ),
        );
        self.request_status(fx);
        self.request_config(fx);
    }

    fn ask_reset(&mut self) {
        if !self.is_connected() {
            self.say(MessageKind::Error, "Not connected to the daemon.");
            return;
        }
        if self.refuse_while_busy() {
            return;
        }
        self.modal = Some(Modal::Ask(Ask {
            title: "Reset this site".into(),
            text: "Remove the bridge, all tunnels and rules, the configuration and the invitations of this site? It becomes unconfigured, like after the installation. Other sites keep listing it until it is removed there.".into(),
            action: AskAction::Reset,
        }));
    }

    fn on_reset(&mut self, report: &ResetReport, fx: &mut Vec<Effect>) {
        self.busy = None;
        // Nothing of the old network is left to edit or to apply.
        self.draft = None;
        self.applied_draft = None;
        self.restore_draft = None;
        self.modal = None;
        self.wizard = Wizard::new();
        self.wizard
            .prefill(&hostinfo::hostname(), hostinfo::default_address());
        self.invitation = None;
        self.view = View::Setup;
        if report.problems.is_empty() {
            self.say(
                MessageKind::Success,
                "Reset done: this site is unconfigured again.",
            );
        } else {
            self.say(
                MessageKind::Error,
                format!(
                    "Reset incomplete: {}. Press R again; the log (5) has details.",
                    report.problems.join("; ")
                ),
            );
        }
        self.request_status(fx);
        self.request_config(fx);
        self.request_interfaces(fx);
    }

    fn on_setup_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) {
        match key.code {
            KeyCode::Char('i') => self.invite(),
            KeyCode::Char('I') => self.show_invites(fx),
            KeyCode::Char('R') => self.ask_reset(),
            _ => {}
        }
    }

    fn on_rolled_back(&mut self, fx: &mut Vec<Effect>) {
        self.busy = None;
        self.clear_pending();
        if self.wizard.step == Step::Applied {
            self.wizard.step = Step::Summary;
        }
        if self.applied_draft.is_some() {
            self.restore_draft = Some(
                "Rolled back. Your change is kept in the draft: a applies it again, x discards it."
                    .into(),
            );
        }
        self.say(
            MessageKind::Success,
            "Rolled back to the previous configuration.",
        );
        self.request_status(fx);
        self.request_config(fx);
    }

    fn clamp_selection(&mut self) {
        if let Some(d) = &self.draft {
            self.sites_selected = self
                .sites_selected
                .min(d.network.sites.len().saturating_sub(1));
            self.maps_selected = self
                .maps_selected
                .min(d.network.maps.len().saturating_sub(1));
        }
        // The selection stays on a row that is shown.
        let sites = self.visible_sites();
        if !sites.is_empty() && !sites.iter().any(|(i, _)| *i == self.sites_selected) {
            self.sites_selected = sites[0].0;
        }
        let maps = self.visible_maps();
        if !maps.is_empty() && !maps.contains(&self.maps_selected) {
            self.maps_selected = maps[0];
        }
        self.tunnels_selected = self
            .tunnels_selected
            .min(self.visible_tunnels().len().saturating_sub(1));
        self.breakouts_selected = self
            .breakouts_selected
            .min(self.breakout_rows().len().saturating_sub(1));
        let settings = super::ui::settings::settings(self.draft.as_ref()).len();
        self.settings_selected = self.settings_selected.min(settings.saturating_sub(1));
    }

    // ----- keys -------------------------------------------------------

    fn on_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            fx.push(Effect::Quit);
            return;
        }
        // Too small: the screen says so; only q (and Ctrl-C) act.
        if self.too_small {
            if key.modifiers.is_empty()
                && key.code == KeyCode::Char('q')
                && self.quit_losses().is_empty()
            {
                fx.push(Effect::Quit);
            }
            return;
        }
        // The banner offers c and r: they work in every window that takes
        // no text and loses nothing when closed (forms: Esc first).
        if self.modal.is_some()
            && !matches!(self.modal, Some(Modal::Review(ref r)) if matches!(r.state, ReviewState::Applied(_)))
            && self.confirm_keys_live()
            && key.modifiers.is_empty()
            && self.pending().is_some()
        {
            match key.code {
                KeyCode::Char('c') => {
                    self.modal = None;
                    self.confirm(fx);
                    return;
                }
                KeyCode::Char('r') => {
                    self.modal = self.ask_rollback();
                    return;
                }
                _ => {}
            }
        }
        if let Some(modal) = self.modal.take() {
            self.modal = self.on_modal_key(modal, key, fx);
            return;
        }
        let unconfigured = self
            .status
            .as_ref()
            .is_some_and(|s| s.state == SiteState::Unconfigured);
        if self.view == View::Setup
            && self.is_connected()
            && self.status.is_some()
            && (unconfigured || self.wizard.step != Step::Welcome)
            && let Some(action) = self.wizard.handle_key(key, &self.interfaces)
        {
            self.on_wizard_action(action, fx);
            return;
        }
        if self.view == View::Macs && self.macs.editing {
            self.macs.handle_key(key);
            return;
        }
        if let Some(filter) = self.typing_filter() {
            filter.handle_key(key);
            self.log.scroll = 0;
            self.clamp_selection();
            return;
        }
        if self.on_global_key(key, fx) {
            return;
        }
        match self.view {
            View::Overview => self.on_overview_key(key, fx),
            View::Sites => self.on_sites_key(key, fx),
            View::Vlans => self.on_vlans_key(key),
            View::Macs => {
                self.macs.handle_key(key);
            }
            View::Log => self.on_log_key(key),
            View::Setup => self.on_setup_key(key, fx),
            View::Settings => self.on_settings_key(key, fx),
            View::Breakout => self.on_breakout_key(key),
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent, fx: &mut Vec<Effect>) {
        if !self.mouse || self.too_small {
            return;
        }
        let (column, row) = (mouse.column, mouse.row);
        let press = |app: &mut Self, code: KeyCode, fx: &mut Vec<Effect>| {
            app.on_key(&KeyEvent::new(code, KeyModifiers::NONE), fx);
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if row == 0 && self.modal.is_none() => {
                if let Some((view, ..)) = tab_layout(self.scrolls.tabs_width.get())
                    .1
                    .into_iter()
                    .find(|(_, _, start, end)| (*start..*end).contains(&column))
                {
                    self.set_view(view, fx);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if row == self.scrolls.bar_row.get() => {
                let label = self
                    .scrolls
                    .bar
                    .borrow()
                    .iter()
                    .find(|(start, end, _)| (*start..*end).contains(&column))
                    .map(|(_, _, label)| *label);
                if let Some(code) = label.and_then(bar_key) {
                    press(self, code, fx);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((list, index)) = self.list_at(column, row) {
                    self.click(list, index, fx);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                let code = if up { KeyCode::Up } else { KeyCode::Down };
                match self.list_at_or_near(column, row) {
                    // The overview's network list has its own part of the cursor.
                    Some(ListId::Network) => {
                        let peers = self.status.as_ref().map_or(0, |s| s.peers.len());
                        let sites = self.overview_sites().len();
                        let at = self.overview_selected.max(peers) - peers;
                        let next = if up {
                            at.saturating_sub(1)
                        } else {
                            (at + 1).min(sites.saturating_sub(1))
                        };
                        self.overview_selected = peers + next;
                    }
                    Some(ListId::Peers) => {
                        let peers = self.status.as_ref().map_or(0, |s| s.peers.len());
                        let at = self.overview_selected.min(peers.saturating_sub(1));
                        self.overview_selected = if up {
                            at.saturating_sub(1)
                        } else {
                            (at + 1).min(peers.saturating_sub(1))
                        };
                    }
                    Some(_) => press(self, code, fx),
                    // Windows and views that scroll as a whole; never the
                    // focus of a form or a setup step.
                    None if matches!(
                        self.modal,
                        Some(
                            Modal::Help { .. }
                                | Modal::Messages { .. }
                                | Modal::Review(_)
                                | Modal::Peer { .. }
                                | Modal::Vlan { .. }
                                | Modal::Site { .. }
                        )
                    ) || (self.modal.is_none() && self.view == View::Log) =>
                    {
                        press(self, code, fx);
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    /// The lists on the screen now.
    fn lists(&self) -> Vec<(ListId, &list::Scroll)> {
        match (&self.modal, self.view) {
            (Some(Modal::Invites { .. }), _) => vec![(ListId::Invites, &self.scrolls.invites)],
            (Some(Modal::Ports(_)), _) => vec![(ListId::Ports, &self.scrolls.ports)],
            (Some(Modal::Peer { .. }), _) => vec![(ListId::PeerVlans, &self.scrolls.peer_vlans)],
            (Some(_), _) => Vec::new(),
            (None, View::Overview) => vec![
                (ListId::Peers, &self.scrolls.peers),
                (ListId::Network, &self.scrolls.network),
            ],
            (None, View::Sites) => vec![(ListId::Sites, &self.scrolls.sites)],
            (None, View::Vlans) if self.vlans_tunnels => {
                vec![(ListId::Tunnels, &self.scrolls.tunnels)]
            }
            (None, View::Vlans) => vec![(ListId::Maps, &self.scrolls.maps)],
            (None, View::Macs) => vec![(ListId::Macs, &self.scrolls.macs)],
            (None, View::Breakout) => vec![(ListId::Breakouts, &self.scrolls.breakouts)],
            (None, _) => Vec::new(),
        }
    }

    /// The list and row under the mouse.
    fn list_at(&self, column: u16, row: u16) -> Option<(ListId, usize)> {
        self.lists()
            .into_iter()
            .find_map(|(id, scroll)| scroll.row_at(column, row).map(|r| (id, r)))
    }

    /// The list under the mouse, also below its last row (the wheel).
    fn list_at_or_near(&self, column: u16, row: u16) -> Option<ListId> {
        self.list_at(column, row).map(|(id, _)| id).or_else(|| {
            self.lists()
                .into_iter()
                .find(|(_, scroll)| scroll.contains(column, row.saturating_sub(1)))
                .map(|(id, _)| id)
        })
    }

    /// A click on a row: selects it, or opens it if it is selected.
    fn click(&mut self, list: ListId, index: usize, fx: &mut Vec<Effect>) {
        let peers = self.status.as_ref().map_or(0, |s| s.peers.len());
        let open = match list {
            ListId::Peers | ListId::Network => {
                let target = if list == ListId::Peers {
                    index
                } else {
                    peers + index
                };
                let again = self.overview_selected == target;
                self.overview_selected = target;
                again
            }
            ListId::Sites => {
                let Some(&(site, _)) = self.visible_sites().get(index) else {
                    return;
                };
                let again = self.sites_selected == site;
                self.sites_selected = site;
                again
            }
            ListId::Maps => {
                let Some(&map) = self.visible_maps().get(index) else {
                    return;
                };
                let again = self.maps_selected == map;
                self.maps_selected = map;
                again
            }
            ListId::Tunnels => {
                let again = self.tunnels_selected == index;
                self.tunnels_selected = index;
                again
            }
            ListId::Breakouts => {
                let again = self.breakouts_selected == index;
                self.breakouts_selected = index;
                again
            }
            ListId::Macs => {
                self.macs.selected = index;
                false
            }
            ListId::Invites => {
                self.modal = Some(Modal::Invites { selected: index });
                false
            }
            // A click on a VLAN chooses it, a second one opens it.
            ListId::PeerVlans => {
                let Some(Modal::Peer { name, vlan }) = &mut self.modal else {
                    return;
                };
                let vid = self
                    .vlan_traffic
                    .as_ref()
                    .and_then(|t| traffic::peer_vlans(t, name).get(index).map(|p| p.vid));
                let again = vid.is_some() && (*vlan == vid || (vlan.is_none() && index == 0));
                if vid.is_some() {
                    *vlan = vid;
                }
                again
            }
            // A click on the row under the cursor ticks it.
            ListId::Ports => {
                let Some(Modal::Ports(picker)) = &mut self.modal else {
                    return;
                };
                let again = picker.cursor == index;
                picker.cursor = index;
                if again {
                    self.on_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), fx);
                }
                false
            }
        };
        if open {
            self.on_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), fx);
        }
    }

    fn on_global_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Tab => self.set_view(self.view.offset(1), fx),
            KeyCode::BackTab => self.set_view(self.view.offset(-1), fx),
            KeyCode::Char(c @ '1'..='8') => {
                let index = c.to_digit(10).map_or(0, |d| d as usize - 1);
                if let Some(&view) = View::ALL.get(index) {
                    self.set_view(view, fx);
                }
            }
            // At the section of this view; Up and Home lead to the rest.
            KeyCode::Char('?') => {
                self.modal = Some(Modal::Help {
                    scroll: super::help::section_line(self.view),
                });
            }
            KeyCode::Char('M') => self.modal = Some(Modal::Messages { scroll: 0 }),
            KeyCode::Char('q') => self.request_quit(fx),
            KeyCode::Char('a') => self.open_review(),
            KeyCode::Char('x') => self.ask_discard(),
            KeyCode::Char('c') => self.confirm(fx),
            KeyCode::Char('r') => {
                if let Some(ask) = self.ask_rollback() {
                    self.modal = Some(ask);
                }
            }
            _ => return false,
        }
        true
    }

    /// Switches the view.
    pub fn set_view(&mut self, view: View, fx: &mut Vec<Effect>) {
        self.view = view;
        match view {
            View::Macs => self.request_macs(fx),
            View::Setup => self.request_interfaces(fx),
            _ => {}
        }
    }

    /// What would be lost by quitting now, in sentences (empty: nothing).
    fn quit_losses(&self) -> Vec<String> {
        let mut lost = Vec::new();
        if self.busy == Some(Busy::Applying) {
            lost.push("An apply is still running.".to_string());
        }
        if self.draft_dirty() {
            lost.push("The draft has changes that are not applied; they are lost.".to_string());
        }
        // A change from a peer or a join is confirmed by the daemon itself:
        // nothing is lost by quitting.
        if let Some(p) = self.pending().filter(|p| p.source == ChangeSource::Local) {
            lost.push(if self.is_connected() {
                format!(
                    "The applied change is rolled back in {} s unless confirmed (c).",
                    p.remaining_secs
                )
            } else {
                "A change was waiting for confirmation; the daemon is not reachable, so it may be rolled back by itself.".to_string()
            });
        }
        lost
    }

    fn request_quit(&mut self, fx: &mut Vec<Effect>) {
        let lost = self.quit_losses();
        if lost.is_empty() {
            fx.push(Effect::Quit);
            return;
        }
        self.modal = Some(Modal::Ask(Ask {
            title: "Quit".into(),
            text: format!("{} Quit anyway?", lost.join(" ")),
            action: AskAction::Quit,
        }));
    }

    /// True (with a message) while a reset runs: nothing else is sent.
    fn refuse_while_busy(&mut self) -> bool {
        if self.busy == Some(Busy::Resetting) {
            self.say(MessageKind::Info, "The reset is still running.");
            return true;
        }
        false
    }

    /// True if c and r reach the pending change in the window that is
    /// open: no text is typed there and nothing would be lost by closing it.
    pub fn confirm_keys_live(&self) -> bool {
        let typing = [
            &self.sites_filter,
            &self.maps_filter,
            &self.tunnels_filter,
            &self.log.text,
        ]
        .iter()
        .any(|f| f.editing);
        let wizard_text = self.view == View::Setup
            && self.is_connected()
            && self.status.is_some()
            && self.wizard.captures_text(&self.interfaces);
        match &self.modal {
            None => !typing && !wizard_text && !(self.view == View::Macs && self.macs.editing),
            Some(Modal::Form(_) | Modal::Ask(_)) => false,
            Some(Modal::Review(r)) => r.state != ReviewState::Applying,
            Some(Modal::Ports(p)) => {
                !p.changed()
                    && self
                        .draft
                        .as_ref()
                        .is_some_and(|d| p.cursor < p.rows(d, &self.interfaces).len())
            }
            Some(_) => true,
        }
    }

    fn ask_discard(&mut self) {
        if !self.draft_dirty() {
            self.say(MessageKind::Info, "The draft has no changes.");
            return;
        }
        self.modal = Some(Modal::Ask(Ask {
            title: "Discard draft".into(),
            text: "Throw away all changes in the draft and start again from the running configuration?"
                .into(),
            action: AskAction::Discard,
        }));
    }

    fn blocked_by_pending(&mut self) -> bool {
        if let Some(p) = self.pending()
            && p.source != ChangeSource::Local
        {
            let what = match &p.source {
                ChangeSource::Sync { peer } => format!("A change from \"{peer}\""),
                ChangeSource::Join { site } => format!("The join of \"{site}\""),
                ChangeSource::Local => String::new(),
            };
            let text = format!(
                "{what} is confirmed by itself as soon as the other side answers ({} s at most); the apply follows then.",
                p.remaining_secs
            );
            self.say(MessageKind::Info, text);
            return true;
        }
        if let Some(p) = self.pending() {
            let keys = if self.confirm_keys_live() {
                "confirm it (c) or roll it back (r) first"
            } else {
                "close this window (Esc), then confirm it (c) or roll it back (r)"
            };
            let text = format!(
                "A change is waiting for confirmation ({} s left): {keys}.",
                p.remaining_secs
            );
            self.say(MessageKind::Error, text);
            return true;
        }
        false
    }

    fn open_review(&mut self) {
        if !self.is_connected() {
            self.say(MessageKind::Error, "Not connected to the daemon.");
            return;
        }
        if self.refuse_while_busy() || self.blocked_by_pending() {
            return;
        }
        let Some(draft) = &self.draft else {
            self.say(
                MessageKind::Info,
                "This site is not configured yet; run the setup (6) first.",
            );
            return;
        };
        if !self.draft_dirty() {
            self.say(
                MessageKind::Info,
                "The draft has no changes. Edit sites (2) or VLANs (3) first.",
            );
            return;
        }
        if let Some(running) = &self.running
            && let Some(old) = &running.network
            && let Some(me) = &running.local.self_name
            && let Err(e) = policy::check_edit(old, &draft.network, me)
        {
            self.say(MessageKind::Error, e);
            return;
        }
        let Some(draft) = &self.draft else { return };
        self.modal = Some(Modal::Review(Review {
            origin: Origin::Draft,
            network: draft.network_text(),
            local: draft.local_text(),
            base_revision: draft.base_revision,
            commit_timeout: draft
                .local
                .commit_timeout
                .unwrap_or(draft.network.commit_timeout),
            lines: draft.diff(self.running.as_ref()),
            problems: draft.diagnostics.clone(),
            scroll: 0,
            state: ReviewState::Ready,
        }));
    }

    /// True (with a message) while the daemon is not reachable.
    fn refuse_offline(&mut self) -> bool {
        if self.is_connected() {
            return false;
        }
        self.say(
            MessageKind::Error,
            "Not connected to the daemon; the pending change cannot be confirmed or rolled back now.",
        );
        true
    }

    fn confirm(&mut self, fx: &mut Vec<Effect>) {
        if self.refuse_offline() {
            return;
        }
        if self.pending().is_none() {
            self.say(MessageKind::Info, "No change is waiting for confirmation.");
            return;
        }
        if self.busy.is_none() {
            self.busy = Some(Busy::Confirming);
            fx.push(Effect::Request(Request::Confirm));
        }
    }

    /// The question before rolling the pending change back, or a message
    /// that there is none.
    fn ask_rollback(&mut self) -> Option<Modal> {
        if self.refuse_offline() {
            return None;
        }
        if self.pending().is_none() {
            self.say(MessageKind::Info, "No change is waiting for confirmation.");
            return None;
        }
        Some(Modal::Ask(Ask {
            title: "Roll back".into(),
            text: "Roll the pending change back now? The configuration before it applies again."
                .into(),
            action: AskAction::Rollback,
        }))
    }

    fn rollback(&mut self, fx: &mut Vec<Effect>) {
        if self.pending().is_none() {
            self.say(MessageKind::Info, "No change is waiting for confirmation.");
            return;
        }
        if self.busy.is_none() {
            self.busy = Some(Busy::RollingBack);
            fx.push(Effect::Request(Request::Rollback));
        }
    }

    fn on_modal_key(
        &mut self,
        modal: Modal,
        key: &KeyEvent,
        fx: &mut Vec<Effect>,
    ) -> Option<Modal> {
        match modal {
            Modal::Help { scroll } => {
                let scroll = match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') => return None,
                    KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
                    KeyCode::PageUp => scroll.saturating_sub(10),
                    KeyCode::PageDown => scroll.saturating_add(10),
                    KeyCode::Home => 0,
                    KeyCode::End => u16::MAX,
                    _ => scroll,
                };
                // Known once the help was drawn at the current size.
                let max = match self.scrolls.help_max.get() {
                    0 => super::help::max_scroll(),
                    m => m,
                };
                Some(Modal::Help {
                    scroll: scroll.min(max),
                })
            }
            Modal::Ask(ask) => {
                let yes = match key.code {
                    KeyCode::Char('y' | 'Y') => true,
                    KeyCode::Enter => ask.action.enter_means_yes(),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => false,
                    _ => return Some(Modal::Ask(ask)),
                };
                match (yes, ask.action) {
                    // No: back where the question came from.
                    (false, AskAction::DiscardInput(form)) => Some(Modal::Form(*form)),
                    (false, AskAction::DiscardPorts(picker)) => Some(Modal::Ports(*picker)),
                    (false, AskAction::DeleteInvite { selected, .. }) => {
                        Some(Modal::Invites { selected })
                    }
                    (false, _) => None,
                    (true, AskAction::DeleteInvite { code, selected }) => {
                        fx.push(Effect::Request(Request::DeleteInvite(InviteParams {
                            code,
                        })));
                        Some(Modal::Invites { selected })
                    }
                    (true, action) => {
                        self.on_answer(action, fx);
                        None
                    }
                }
            }
            Modal::Form(form) => self.on_form_key(form, key, fx),
            Modal::Invites { selected } => self.on_invites_key(selected, key),
            Modal::Messages { scroll } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'M') => None,
                KeyCode::Up | KeyCode::Char('k') => Some(Modal::Messages {
                    scroll: scroll.saturating_sub(1),
                }),
                KeyCode::Down | KeyCode::Char('j') => Some(Modal::Messages {
                    scroll: scroll.saturating_add(1).min(
                        u16::try_from(self.messages.len().saturating_sub(1)).unwrap_or(u16::MAX),
                    ),
                }),
                _ => Some(Modal::Messages { scroll }),
            },
            Modal::Peer { name, vlan } => self.on_peer_key(name, vlan, key, fx),
            Modal::Vlan { vid, back } => self.on_vlan_key(vid, back, key, fx),
            Modal::Site { name, scroll } => self.on_site_key(name, scroll, key),
            Modal::Review(review) => self.on_review_key(review, key, fx),
            Modal::Ports(picker) => self.on_ports_key(picker, key),
        }
    }

    /// Peer details: Left/Right the previous or next peer, Up/Down a VLAN
    /// of its tunnel, Enter opens it.
    fn on_peer_key(
        &mut self,
        name: String,
        vlan: Option<u16>,
        key: &KeyEvent,
        fx: &mut Vec<Effect>,
    ) -> Option<Modal> {
        let peers: Vec<String> = self
            .status
            .as_ref()
            .map(|s| s.peers.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default();
        let vids: Vec<u16> = self
            .vlan_traffic
            .as_ref()
            .filter(|t| t.available)
            .map(|t| {
                traffic::peer_vlans(t, &name)
                    .iter()
                    .map(|p| p.vid)
                    .collect()
            })
            .unwrap_or_default();
        let at = vlan
            .and_then(|v| vids.iter().position(|x| *x == v))
            .unwrap_or_default();
        if let Some(n) = nav(key.code, at, vids.len(), self.scrolls.peer_vlans.page()) {
            return Some(Modal::Peer {
                name,
                vlan: vids.get(n).copied(),
            });
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => None,
            KeyCode::Enter => match vids.get(at) {
                Some(&vid) => {
                    self.request_vlan_traffic(fx);
                    self.request_macs(fx);
                    Some(Modal::Vlan {
                        vid,
                        back: Some(name),
                    })
                }
                None => Some(Modal::Peer { name, vlan }),
            },
            KeyCode::Left | KeyCode::Right => {
                let here = peers.iter().position(|p| *p == name).unwrap_or_default();
                let next = if key.code == KeyCode::Left {
                    here.saturating_sub(1)
                } else {
                    (here + 1).min(peers.len().saturating_sub(1))
                };
                match peers.get(next) {
                    Some(other) if *other != name => Some(Modal::Peer {
                        name: other.clone(),
                        vlan: None,
                    }),
                    _ => Some(Modal::Peer { name, vlan }),
                }
            }
            _ => Some(Modal::Peer { name, vlan }),
        }
    }

    /// The VLAN window: Up/Down the previous or next VLAN, m its MAC
    /// addresses, Esc back to the peer.
    fn on_vlan_key(
        &mut self,
        vid: u16,
        back: Option<String>,
        key: &KeyEvent,
        fx: &mut Vec<Effect>,
    ) -> Option<Modal> {
        let step = match key.code {
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Esc => {
                return back.map(|name| Modal::Peer {
                    name,
                    vlan: Some(vid),
                });
            }
            KeyCode::Char('q') => return None,
            KeyCode::Char('m') => {
                self.macs.filter.set(format!("vlan:{vid}"));
                self.macs.selected = 0;
                self.set_view(View::Macs, fx);
                return None;
            }
            _ => return Some(Modal::Vlan { vid, back }),
        };
        let vid = self
            .vlan_traffic
            .as_ref()
            .map_or(vid, |t| traffic::next_vlan(t, vid, step));
        Some(Modal::Vlan { vid, back })
    }

    fn on_invites_key(&mut self, selected: usize, key: &KeyEvent) -> Option<Modal> {
        let count = self.invitations.len();
        if let Some(n) = nav(key.code, selected, count, self.scrolls.invites.page()) {
            return Some(Modal::Invites { selected: n });
        }
        let selected = match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return None,
            KeyCode::Char('n' | 'i') => {
                self.invite();
                return self.modal.take();
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(inv) = self.invitations.get(selected) {
                    return Some(Modal::Ask(Ask {
                        title: "Delete invitation".into(),
                        text: format!(
                            "Delete the invitation {}? Its code no longer works.",
                            inv.code
                        ),
                        action: AskAction::DeleteInvite {
                            code: inv.code.clone(),
                            selected,
                        },
                    }));
                }
                selected
            }
            _ => selected,
        };
        Some(Modal::Invites { selected })
    }

    /// Opens the window to choose trunk ports (the interfaces come from
    /// the daemon, with the tunnels that run over each).
    fn open_ports(&mut self, target: PortTarget, fx: &mut Vec<Effect>) {
        let Some(draft) = &self.draft else {
            self.not_configured();
            return;
        };
        self.modal = Some(Modal::Ports(PortPicker::new(target, draft)));
        self.request_interfaces(fx);
    }

    fn on_ports_key(&mut self, mut picker: PortPicker, key: &KeyEvent) -> Option<Modal> {
        let Some(draft) = &self.draft else {
            return None;
        };
        if !self.interfaces_loaded && self.interfaces_error.is_none() {
            // Still loading: only Esc.
            return (key.code != KeyCode::Esc).then_some(Modal::Ports(picker));
        }
        let rows = picker.rows(draft, &self.interfaces);
        // The rows and "other" below them.
        let count = rows.len() + 1;
        let typing = picker.cursor >= rows.len();
        let moves = if typing {
            matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
            )
        } else {
            true
        };
        if moves && let Some(n) = nav(key.code, picker.cursor, count, self.scrolls.ports.page()) {
            picker.cursor = n;
            return Some(Modal::Ports(picker));
        }
        match key.code {
            KeyCode::Esc if picker.changed() => {
                return Some(Modal::Ask(Ask {
                    title: "Discard the choice".into(),
                    text: "Close the window and lose the changed trunk ports? n goes back to it."
                        .into(),
                    action: AskAction::DiscardPorts(Box::new(picker)),
                }));
            }
            KeyCode::Esc => return None,
            // Added above "other", which keeps the cursor for the next name.
            KeyCode::Enter if typing && !picker.manual.value().trim().is_empty() => {
                picker.add_manual(&self.interfaces);
                picker.cursor = picker.rows(draft, &self.interfaces).len();
            }
            KeyCode::Enter => match picker.apply(draft) {
                Ok(new) => {
                    self.draft = Some(new);
                    self.clamp_selection();
                    let text = if self.draft.as_ref().is_some_and(|d| d.errors() > 0) {
                        "Saved in the draft. The draft still has errors (marked red)."
                    } else {
                        "Saved in the draft. Press a to review and apply the changes."
                    };
                    self.say(MessageKind::Info, text);
                    return None;
                }
                Err(e) => picker.error = Some(e),
            },
            _ if typing => picker.type_key(key),
            KeyCode::Char(' ') => {
                if let Some(row) = rows.get(picker.cursor) {
                    picker.toggle(row);
                }
                // A port not on this host leaves the list when taken out.
                let count = picker.rows(draft, &self.interfaces).len();
                picker.cursor = picker.cursor.min(count);
            }
            KeyCode::Char('m') => {
                if let Some(row) = rows.get(picker.cursor) {
                    picker.make_main(&row.name);
                }
            }
            _ => {}
        }
        Some(Modal::Ports(picker))
    }

    fn on_form_key(
        &mut self,
        mut form: Form,
        key: &KeyEvent,
        fx: &mut Vec<Effect>,
    ) -> Option<Modal> {
        // An invitation is for the network that runs, not the draft.
        let running = (form.kind == FormKind::Invite)
            .then(|| self.running.as_ref().and_then(Draft::from_running))
            .flatten();
        let draft = running.as_ref().or(self.draft.as_ref())?;
        match form.handle_key(key) {
            FormEvent::Nothing => {}
            FormEvent::Changed => form.revalidate(draft),
            FormEvent::Cancel if form.is_touched() => {
                return Some(Modal::Ask(Ask {
                    title: "Discard input".into(),
                    text: "Close the form and lose what you entered? n goes back to it.".into(),
                    action: AskAction::DiscardInput(Box::new(form)),
                }));
            }
            FormEvent::Cancel => return None,
            FormEvent::Submit if form.kind == FormKind::Invite => {
                if form.submit(draft).is_some() {
                    if form.notes.iter().any(Diagnostic::is_error) {
                        self.say(
                            MessageKind::Error,
                            "The rules do not fit the network (see the notes in the form).",
                        );
                    } else {
                        fx.push(Effect::Request(Request::Invite(form.invite_rules())));
                        // Kept until the daemon answers: a refusal brings it back.
                        self.invite_pending = Some(form);
                        return None;
                    }
                }
            }
            FormEvent::Submit => {
                if let Some(new) = form.submit(draft) {
                    match form.kind {
                        FormKind::Site(None) => {
                            self.sites_selected = new.network.sites.len().saturating_sub(1);
                        }
                        FormKind::Map(None) => {
                            self.maps_selected = new.network.maps.len().saturating_sub(1);
                        }

                        _ => {}
                    }
                    self.draft = Some(new);
                    self.clamp_selection();
                    let text = if self.draft.as_ref().is_some_and(|d| d.errors() > 0) {
                        "Saved in the draft. The draft still has errors (marked red)."
                    } else {
                        "Saved in the draft. Press a to review and apply the changes."
                    };
                    self.say(MessageKind::Info, text);
                    return None;
                }
            }
        }
        Some(Modal::Form(form))
    }

    fn on_review_key(
        &mut self,
        mut review: Review,
        key: &KeyEvent,
        fx: &mut Vec<Effect>,
    ) -> Option<Modal> {
        match &review.state {
            ReviewState::Applying => {}
            ReviewState::Applied(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => return None,
                KeyCode::Char('c') => {
                    self.confirm(fx);
                    return None;
                }
                KeyCode::Char('r') => return self.ask_rollback(),
                _ => {}
            },
            ReviewState::Ready | ReviewState::Failed(_) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => return None,
                KeyCode::Enter | KeyCode::Char('y') => self.start_apply(&mut review, fx),
                _ => {}
            },
        }
        review.scroll_key(key.code);
        Some(Modal::Review(review))
    }

    /// Sends the reviewed configuration to the daemon, unless it is invalid
    /// or another change is still waiting for confirmation.
    fn start_apply(&mut self, review: &mut Review, fx: &mut Vec<Effect>) {
        if !review.is_valid() {
            return;
        }
        if self.blocked_by_pending() {
            // A change of the daemon's own: applied once it is done.
            self.apply_when_settled = self
                .pending()
                .is_some_and(|p| p.source != ChangeSource::Local);
            return;
        }
        self.apply_when_settled = false;
        review.state = ReviewState::Applying;
        self.busy = Some(Busy::Applying);
        if review.origin == Origin::Draft {
            // What comes back if the change is rolled back (also when the
            // answer to this request never arrives).
            self.applied_draft = self.draft.clone();
        }
        fx.push(Effect::Request(Request::Apply(ApplyParams {
            config: ConfigInput {
                network: review.network.clone(),
                local: review.local.clone(),
            },
            base_revision: Some(review.base_revision),
            timeout_secs: None,
        })));
    }

    fn on_answer(&mut self, action: AskAction, fx: &mut Vec<Effect>) {
        match action {
            AskAction::Quit => fx.push(Effect::Quit),
            AskAction::Discard => {
                self.draft = self.running.as_ref().and_then(Draft::from_running);
                self.clamp_selection();
                self.say(MessageKind::Info, "Draft discarded.");
            }
            AskAction::DeleteSite(name) => {
                let index = self
                    .draft
                    .as_ref()
                    .and_then(|d| d.network.sites.iter().position(|s| s.name == name));
                if let Some(site) = index.and_then(|i| self.draft.as_mut()?.remove_site(i)) {
                    self.say(
                        MessageKind::Info,
                        format!("Site {} removed from the draft.", site.name),
                    );
                }
                self.clamp_selection();
            }
            AskAction::DeleteMap(map) => {
                let index = self
                    .draft
                    .as_ref()
                    .and_then(|d| d.network.maps.iter().position(|m| *m == map));
                if let Some(index) = index
                    && self.draft.as_mut().is_some_and(|d| d.remove_map(index))
                {
                    self.say(
                        MessageKind::Info,
                        format!(
                            "The mapping of {} (source VLAN {} = mapped VLAN {}) is removed from the draft.",
                            map.site, map.shared, map.local
                        ),
                    );
                }
                self.clamp_selection();
            }
            AskAction::DeleteBreakout(site, vid) => {
                if let Some(draft) = &mut self.draft
                    && let Some(entry) = draft.network.sites.iter_mut().find(|s| s.name == site)
                    && entry.breakout.remove(&vid).is_some()
                {
                    draft.revalidate();
                    self.say(
                        MessageKind::Info,
                        format!(
                            "{site}: VLAN {vid} goes through the tunnel again (in the draft; a applies)."
                        ),
                    );
                }
                self.clamp_selection();
            }
            AskAction::Unblock(port) => {
                fx.push(Effect::Request(Request::Unblock(UnblockParams { port })));
            }
            AskAction::Reset if self.busy.is_none() => {
                self.busy = Some(Busy::Resetting);
                fx.push(Effect::Request(Request::Reset));
            }
            AskAction::Reset => {}
            AskAction::Rollback => self.rollback(fx),
            AskAction::DeleteInvite { code, .. } => {
                fx.push(Effect::Request(Request::DeleteInvite(InviteParams {
                    code,
                })));
            }
            AskAction::DiscardInput(_) | AskAction::DiscardPorts(_) => {}
        }
    }

    fn on_wizard_action(&mut self, action: wizard::Action, fx: &mut Vec<Effect>) {
        match action {
            wizard::Action::None => {}
            wizard::Action::Leave => {
                self.view = View::Overview;
                self.say(MessageKind::Info, "Setup left. Press 6 to return to it.");
            }
            wizard::Action::LoadInterfaces => {
                self.wizard.transport = match self.wizard.mode {
                    Mode::Join => self
                        .wizard
                        .target_address()
                        .and_then(hostinfo::route_interface),
                    Mode::New => hostinfo::default_interface(),
                };
                self.request_interfaces(fx);
            }
            wizard::Action::Review => self.open_wizard_review(),
            wizard::Action::Join => self.start_join(fx),
        }
    }

    fn open_wizard_review(&mut self) {
        if self.blocked_by_pending() {
            return;
        }
        let (network, local) = match self.wizard.build(&self.interfaces) {
            Ok(built) => built,
            Err(e) => {
                self.wizard.error = Some(e);
                return;
            }
        };
        let (network_text, local_text) = (render_network(&network), render_local(&local));
        let (old_network, old_local) = self
            .running
            .as_ref()
            .map(|r| (r.network_text(), r.local_text()))
            .unwrap_or_default();
        let base_revision = self
            .running
            .as_ref()
            .map(|r| r.revision)
            .or_else(|| self.status.as_ref().map(|s| s.revision))
            .unwrap_or_default();
        self.modal = Some(Modal::Review(Review {
            origin: Origin::Wizard,
            lines: diff::config_diff(
                (&old_network, &old_local),
                (&network_text, &local_text),
                "running",
                "setup",
            ),
            network: network_text,
            local: local_text,
            base_revision,
            commit_timeout: network.commit_timeout,
            problems: wanderlan_core::config::validate(&network, Some(&local)),
            scroll: 0,
            state: ReviewState::Ready,
        }));
    }

    /// The rows of the Sites view: (index in the network, depth in the
    /// tree), in tree order, passing the search.
    pub fn visible_sites(&self) -> Vec<(usize, usize)> {
        let Some(d) = &self.draft else {
            return Vec::new();
        };
        topology::tree_order(&d.network)
            .into_iter()
            .filter(|(i, _)| {
                d.network.sites.get(*i).is_some_and(|s| {
                    let vlans = d.network.vlans_of(s).to_string();
                    let address = s.address.to_string();
                    let role = format::role(s.uplink.as_deref());
                    let managed = policy::managed_label(&d.network, s);
                    self.sites_filter
                        .matches(&[&s.name, &address, &vlans, &role, &managed])
                })
            })
            .collect()
    }

    /// Indices of the VLAN mappings passing the search.
    pub fn visible_maps(&self) -> Vec<usize> {
        let Some(d) = &self.draft else {
            return Vec::new();
        };
        d.network
            .maps
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                self.maps_filter
                    .matches(&[&m.site, &m.local.to_string(), &m.shared.to_string()])
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The rows of the tunnel table passing the search.
    pub fn visible_tunnels(&self) -> Vec<TunnelRow> {
        let Some(d) = &self.draft else {
            return Vec::new();
        };
        let text = |v: &Option<wanderlan_core::config::VlanSet>| {
            v.as_ref().map(ToString::to_string).unwrap_or_default()
        };
        d.tunnel_rows()
            .into_iter()
            .filter(|r| {
                let kind = match r.servers {
                    Some(true) => "servers",
                    Some(false) => "client",
                    None => "no tunnel",
                };
                self.tunnels_filter.matches(&[
                    &r.sites[0],
                    &r.sites[1],
                    kind,
                    &text(&r.carries),
                    &text(&r.limit),
                ])
            })
            .collect()
    }

    /// The search of the current view while it is typed.
    fn typing_filter(&mut self) -> Option<&mut Filter> {
        let filter = match self.view {
            View::Sites => &mut self.sites_filter,
            View::Vlans if self.vlans_tunnels => &mut self.tunnels_filter,
            View::Vlans => &mut self.maps_filter,
            View::Log => &mut self.log.text,
            _ => return None,
        };
        filter.editing.then_some(filter)
    }

    /// The sites of the overview's network list, in its order (servers
    /// with their clients below).
    pub fn overview_sites(&self) -> Vec<&SiteSummary> {
        let Some(status) = &self.status else {
            return Vec::new();
        };
        let pairs: Vec<(&str, Option<&str>)> = status
            .sites
            .iter()
            .map(|s| (s.name.as_str(), s.uplink.as_deref()))
            .collect();
        topology::tree_order_by(&pairs)
            .into_iter()
            .filter_map(|(i, _)| status.sites.get(i))
            .collect()
    }

    fn on_overview_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) {
        // One cursor over both lists: the peers, then the sites.
        let peers = self.status.as_ref().map_or(0, |s| s.peers.len());
        let count = peers + self.overview_sites().len();
        let page = if self.overview_selected < peers {
            self.scrolls.peers.page()
        } else {
            self.scrolls.network.page()
        };
        if let Some(n) = nav(key.code, self.overview_selected, count, page) {
            self.overview_selected = n;
            return;
        }
        match key.code {
            KeyCode::Char('u') => {
                let port = self.selected_peer().map(|p| (p.port.clone(), p.port_state));
                self.ask_unblock(port);
            }
            // A trunk port the loop guard shut (no peer row to select).
            KeyCode::Char('U') => {
                let port = self.status.as_ref().and_then(|s| {
                    s.blocked
                        .iter()
                        .find(|b| !s.peers.iter().any(|p| p.port == **b))
                        .cloned()
                });
                match port {
                    Some(port) => self.ask_unblock(Some((port, PortState::Blocked))),
                    None => self.say(MessageKind::Info, "No trunk port is blocked."),
                }
            }
            // A site of the list online or offline: straight to the review.
            KeyCode::Char('o') => {
                let name = self
                    .overview_sites()
                    .get(self.overview_selected.saturating_sub(peers))
                    .filter(|_| self.overview_selected >= peers)
                    .map(|s| s.name.clone());
                let index = name.as_ref().and_then(|n| {
                    self.draft
                        .as_ref()?
                        .network
                        .sites
                        .iter()
                        .position(|s| s.name == *n)
                });
                match index {
                    Some(i) => {
                        let before = self.draft.clone();
                        self.sites_selected = i;
                        self.toggle_online();
                        if self.draft != before {
                            self.open_review();
                        }
                    }
                    None => self.say(
                        MessageKind::Info,
                        "Select a site of the network list (below the peers).",
                    ),
                }
            }
            KeyCode::Char('p') if self.is_connected() => {
                fx.push(Effect::Request(Request::ProbeNow));
            }
            KeyCode::Enter => {
                if let Some(peer) = self.selected_peer() {
                    let name = peer.name.clone();
                    self.open_peer(name, fx);
                } else if let Some(name) = self
                    .overview_sites()
                    .get(self.overview_selected.saturating_sub(peers))
                    .map(|s| s.name.clone())
                {
                    self.modal = Some(Modal::Site { name, scroll: 0 });
                }
            }
            _ => {}
        }
    }

    /// Keys of the site details: Up/Down the previous or next site (as in
    /// Sites), `e` edits it.
    fn on_site_key(&mut self, name: String, scroll: u16, key: &KeyEvent) -> Option<Modal> {
        let names: Vec<String> = self
            .visible_sites()
            .into_iter()
            .filter_map(|(i, _)| self.draft.as_ref()?.network.sites.get(i))
            .map(|s| s.name.clone())
            .collect();
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
            // A long site scrolls; Up/Down stay the next site.
            KeyCode::PageDown => Some(Modal::Site {
                name,
                scroll: scroll.saturating_add(5).min(self.scrolls.site_max.get()),
            }),
            KeyCode::PageUp => Some(Modal::Site {
                name,
                scroll: scroll.saturating_sub(5),
            }),
            KeyCode::Char('e') => {
                let index = self
                    .draft
                    .as_ref()
                    .and_then(|d| d.network.sites.iter().position(|s| s.name == name));
                match index {
                    Some(i) if self.may_change(&name) => {
                        self.sites_selected = i;
                        let draft = self.draft.as_ref()?;
                        Some(Modal::Form(Form::site(draft, Some(i))))
                    }
                    _ => Some(Modal::Site { name, scroll }),
                }
            }
            KeyCode::Char('N') => {
                let index = self
                    .draft
                    .as_ref()
                    .and_then(|d| d.network.sites.iter().position(|s| s.name == name));
                match index {
                    Some(i) if self.may_rename(&name) => {
                        self.sites_selected = i;
                        let draft = self.draft.as_ref()?;
                        Some(Modal::Form(Form::rename(draft, i)))
                    }
                    _ => Some(Modal::Site { name, scroll }),
                }
            }
            code => {
                let at = names.iter().position(|n| *n == name).unwrap_or_default();
                let next = nav(code, at, names.len(), 1)
                    .and_then(|i| names.get(i).cloned())
                    .unwrap_or_else(|| name.clone());
                if let Some(i) = self
                    .draft
                    .as_ref()
                    .and_then(|d| d.network.sites.iter().position(|s| s.name == next))
                {
                    self.sites_selected = i;
                }
                // Another site starts at the top; any other key keeps it.
                let scroll = if next == name { scroll } else { 0 };
                Some(Modal::Site { name: next, scroll })
            }
        }
    }

    fn on_settings_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) {
        use super::ui::settings::{ROWS, Scope, Setting, settings};
        let all = settings(self.draft.as_ref());
        if let Some(n) = nav(
            key.code,
            self.settings_selected,
            all.len(),
            self.scrolls.settings.page(),
        ) {
            self.settings_selected = n;
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('e') => {
                if self.draft.is_none() {
                    self.not_configured();
                    return;
                }
                match all.get(self.settings_selected).copied() {
                    Some(Setting::Fixed(i)) => {
                        let Some(row) = ROWS.get(i) else { return };
                        if row.key == "trunk" {
                            self.open_ports(PortTarget::Default, fx);
                            return;
                        }
                        let Some(draft) = &self.draft else { return };
                        // This host only: local.toml or its own ports.
                        let mut form = match row.scope {
                            Scope::Local => Form::local(draft, row.key),
                            Scope::OwnSite => Form::own_ports(draft),
                        };
                        form.focus_on(row.key);
                        self.modal = Some(Modal::Form(form));
                    }
                    Some(Setting::TunnelTrunk(site)) => {
                        let Some(name) = self
                            .draft
                            .as_ref()
                            .and_then(|d| d.network.sites.get(site))
                            .map(|s| s.name.clone())
                        else {
                            return;
                        };
                        self.open_ports(PortTarget::Tunnel(name), fx);
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn ask_unblock(&mut self, port: Option<(String, PortState)>) {
        match port {
            Some((port, PortState::Blocked)) => {
                self.modal = Some(Modal::Ask(Ask {
                    title: "Unblock port".into(),
                    text: format!(
                        "The loop guard shut {port} down because frames came back in a loop. Enable it again? Do this only after the loop is fixed."
                    ),
                    action: AskAction::Unblock(port),
                }));
            }
            Some((port, _)) => self.say(MessageKind::Info, format!("{port} is not blocked.")),
            None => self.say(MessageKind::Info, "No tunnel port selected."),
        }
    }

    fn not_configured(&mut self) {
        self.say(
            MessageKind::Info,
            "This site is not configured yet; run the setup (6) first.",
        );
    }

    fn on_sites_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) {
        // The keys for the selected row act only on a row that is shown.
        let row_key = matches!(
            key.code,
            KeyCode::Enter | KeyCode::Char('e' | 'N' | 'o' | 'd' | 'u') | KeyCode::Delete
        );
        if row_key
            && !self
                .visible_sites()
                .iter()
                .any(|(i, _)| *i == self.sites_selected)
        {
            self.say(
                MessageKind::Info,
                "Nothing selected: the search matches nothing (/ changes it).",
            );
            return;
        }
        let selected_site = self
            .draft
            .as_ref()
            .and_then(|d| d.network.sites.get(self.sites_selected))
            .map(|s| s.name.clone());
        let edits = matches!(key.code, KeyCode::Char('e' | 'd') | KeyCode::Delete);
        if edits
            && let Some(name) = &selected_site
            && !self.may_change(name)
        {
            return;
        }
        if key.code == KeyCode::Char('N')
            && let Some(name) = &selected_site
            && !self.may_rename(name)
        {
            return;
        }
        let Some(draft) = &self.draft else {
            self.not_configured();
            return;
        };
        let selected = self.sites_selected;
        let count = draft.network.sites.len();
        // Up/Down follow the rows shown (tree order, search).
        let order: Vec<usize> = self.visible_sites().into_iter().map(|(i, _)| i).collect();
        let row = order
            .iter()
            .position(|i| *i == selected)
            .unwrap_or_default();
        if let Some(n) = nav(key.code, row, order.len(), self.scrolls.sites.page()) {
            self.sites_selected = order.get(n).copied().unwrap_or(0);
            return;
        }
        match key.code {
            KeyCode::Char('/') => self.sites_filter.start(),
            KeyCode::Char('n') => self.modal = Some(Modal::Form(Form::site(draft, None))),
            KeyCode::Enter => {
                if let Some(name) = selected_site {
                    self.modal = Some(Modal::Site { name, scroll: 0 });
                }
            }
            KeyCode::Char('e') if selected < count => {
                self.modal = Some(Modal::Form(Form::site(draft, Some(selected))));
            }
            KeyCode::Char('N') if selected < count => {
                self.modal = Some(Modal::Form(Form::rename(draft, selected)));
            }
            // The VLANs of the network (each site sets its own settings).
            KeyCode::Char('m') => {
                if self.may_change_network_vlans()
                    && let Some(draft) = &self.draft
                {
                    self.modal = Some(Modal::Form(Form::vlans(draft)));
                }
            }
            KeyCode::Char('i') => self.invite(),
            KeyCode::Char('I') => self.show_invites(fx),
            KeyCode::Char('o') if selected < count => self.toggle_online(),
            KeyCode::Char('l') => self.open_ports(PortTarget::Default, fx),
            KeyCode::Char('d') | KeyCode::Delete if selected < count => {
                let Some(site) = draft.network.sites.get(selected) else {
                    return;
                };
                if draft.local.self_name.as_deref() == Some(site.name.as_str()) {
                    let text = format!(
                        "{} is this site and cannot be removed (change \"This site\" in the settings, 8, first).",
                        site.name
                    );
                    self.say(MessageKind::Error, text);
                    return;
                }
                // Its clients would hang in the air.
                let clients: Vec<&str> = draft
                    .network
                    .sites
                    .iter()
                    .filter(|c| c.uplink.as_deref() == Some(site.name.as_str()))
                    .map(|c| c.name.as_str())
                    .collect();
                if !clients.is_empty() {
                    let text = format!(
                        "{} still has clients ({}): delete them first.",
                        site.name,
                        clients.join(", ")
                    );
                    self.say(MessageKind::Error, text);
                    return;
                }
                let maps = draft.network.maps_of(&site.name).count();
                let extra = if maps > 0 {
                    format!(" Its {maps} VLAN mapping(s) are removed too.")
                } else {
                    String::new()
                };
                self.modal = Some(Modal::Ask(Ask {
                    title: "Delete site".into(),
                    text: format!("Remove site {} from the draft?{extra}", site.name),
                    action: AskAction::DeleteSite(site.name.clone()),
                }));
            }
            KeyCode::Char('u') if selected < count => {
                let port = draft
                    .network
                    .sites
                    .get(selected)
                    .map(|s| tunnel_name(&s.name))
                    .unwrap_or_default();
                let state = self
                    .status
                    .as_ref()
                    .and_then(|s| s.peers.iter().find(|peer| peer.port == port))
                    .map(|peer| peer.port_state);
                match state {
                    Some(state) => self.ask_unblock(Some((port, state))),
                    None => self.say(MessageKind::Info, "There is no tunnel to this site."),
                }
            }
            _ => {}
        }
    }

    fn on_vlans_key(&mut self, key: &KeyEvent) {
        if key.code == KeyCode::Char('t') {
            self.vlans_tunnels = !self.vlans_tunnels;
            self.clamp_selection();
            return;
        }
        if self.vlans_tunnels && !matches!(key.code, KeyCode::Char('v' | 'o')) {
            self.on_tunnels_key(key);
            return;
        }
        let selected_map_site = self
            .draft
            .as_ref()
            .and_then(|d| d.network.maps.get(self.maps_selected))
            .map(|m| m.site.clone());
        let edits = matches!(
            key.code,
            KeyCode::Enter | KeyCode::Char('e' | 'd') | KeyCode::Delete
        );
        if edits && !self.visible_maps().contains(&self.maps_selected) {
            self.say(
                MessageKind::Info,
                "Nothing selected: no mapping is shown (n adds one, / changes the search).",
            );
            return;
        }
        if edits
            && let Some(site) = &selected_map_site
            && !self.may_change(site)
        {
            return;
        }
        let Some(draft) = &self.draft else {
            self.not_configured();
            return;
        };
        let selected = self.maps_selected;
        let count = draft.network.maps.len();
        let shown = self.visible_maps();
        let row = shown
            .iter()
            .position(|i| *i == selected)
            .unwrap_or_default();
        if let Some(n) = nav(key.code, row, shown.len(), self.scrolls.maps.page()) {
            self.maps_selected = shown.get(n).copied().unwrap_or(selected);
            return;
        }
        let Some(draft) = &self.draft else {
            return;
        };
        match key.code {
            KeyCode::Char('/') => self.maps_filter.start(),
            KeyCode::Char('v') => {
                if self.may_change_network_vlans()
                    && let Some(draft) = &self.draft
                {
                    self.modal = Some(Modal::Form(Form::vlans(draft)));
                }
            }
            KeyCode::Char('o') => {
                // Its own VLANs: always this site's own business.
                let me = draft.local.self_name.as_deref();
                match draft
                    .network
                    .sites
                    .iter()
                    .position(|s| Some(s.name.as_str()) == me)
                {
                    Some(i) => self.modal = Some(Modal::Form(Form::own_vlans(draft, i))),
                    None => self.not_configured(),
                }
            }
            KeyCode::Char('n') => {
                let mut form = Form::map(draft, None, None);
                // Only sites this site may change.
                let draft = draft.clone();
                if form.limit_sites("site", |s| self.may_change_quietly(s), &draft) {
                    self.modal = Some(Modal::Form(form));
                } else {
                    self.say(
                        MessageKind::Error,
                        "There is no site whose mappings this site may change.",
                    );
                }
            }
            KeyCode::Enter | KeyCode::Char('e') if selected < count => {
                self.modal = Some(Modal::Form(Form::map(draft, Some(selected), None)));
            }
            KeyCode::Char('d') | KeyCode::Delete if selected < count => {
                if let Some(map) = draft.network.maps.get(selected) {
                    self.modal = Some(Modal::Ask(Ask {
                        title: "Delete VLAN mapping".into(),
                        text: format!(
                            "Remove the mapping of {}: source VLAN {} = mapped VLAN {} on its trunk?",
                            map.site, map.shared, map.local
                        ),
                        action: AskAction::DeleteMap(map.clone()),
                    }));
                }
            }
            _ => {}
        }
    }

    /// The rows of the breakout view: every VLAN of every site with a
    /// breakout, in the order of the sites.
    pub fn breakout_rows(&self) -> Vec<(String, u16)> {
        let Some(d) = &self.draft else {
            return Vec::new();
        };
        d.network
            .sites
            .iter()
            .flat_map(|s| s.breakout.keys().map(|vid| (s.name.clone(), *vid)))
            .collect()
    }

    /// Keys of the breakout view.
    fn on_breakout_key(&mut self, key: &KeyEvent) {
        let rows = self.breakout_rows();
        if let Some(n) = nav(
            key.code,
            self.breakouts_selected,
            rows.len(),
            self.scrolls.breakouts.page(),
        ) {
            self.breakouts_selected = n;
            return;
        }
        let selected = rows.get(self.breakouts_selected).cloned();
        let edits = matches!(
            key.code,
            KeyCode::Enter | KeyCode::Char('e' | 'd') | KeyCode::Delete
        );
        if edits && selected.is_none() {
            self.say(MessageKind::Info, "No VLAN breaks out yet: n chooses one.");
            return;
        }
        if edits
            && let Some((site, _)) = &selected
            && !self.may_change(site)
        {
            return;
        }
        let Some(draft) = &self.draft else {
            self.not_configured();
            return;
        };
        match key.code {
            KeyCode::Char('n') => {
                let site = selected
                    .as_ref()
                    .map(|(s, _)| s.clone())
                    .or_else(|| draft.local.self_name.clone());
                let uplinks = self.uplink_hint();
                if let Some(draft) = self.draft.clone() {
                    let mut form = Form::breakout(&draft, None, site.as_deref(), &uplinks);
                    // Only sites this site may change.
                    if form.limit_sites("site", |s| self.may_change_quietly(s), &draft) {
                        self.modal = Some(Modal::Form(form));
                    } else {
                        self.say(
                            MessageKind::Error,
                            "There is no site whose breakouts this site may change.",
                        );
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let uplinks = self.uplink_hint();
                if let (Some(draft), Some(selected)) = (&self.draft, selected) {
                    self.modal = Some(Modal::Form(Form::breakout(
                        draft,
                        Some(selected),
                        None,
                        &uplinks,
                    )));
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some((site, vid)) = selected {
                    self.modal = Some(Modal::Ask(Ask {
                        title: "Remove breakout".into(),
                        text: format!(
                            "VLAN {vid} of {site} goes through the tunnel again, also to the internet?"
                        ),
                        action: AskAction::DeleteBreakout(site, vid),
                    }));
                }
            }
            _ => {}
        }
    }

    /// The uplinks of this box, for the hint of the uplink field.
    fn uplink_hint(&self) -> String {
        let Some(status) = &self.status else {
            return String::new();
        };
        let mut names: Vec<String> = status
            .uplinks
            .iter()
            .filter(|u| u.gateway4.is_some() || u.gateway6.is_some())
            .map(|u| {
                if u.default4 || u.default6 {
                    format!("{} (default)", u.name)
                } else {
                    u.name.clone()
                }
            })
            .collect();
        if names.is_empty() {
            names = self
                .interfaces
                .iter()
                .filter(|i| i.is_listed() && i.master.is_none() && i.kind != "bridge")
                .map(|i| i.name.clone())
                .collect();
        }
        names.join(", ")
    }

    /// Keys of the tunnel table in the VLAN view.
    fn on_tunnels_key(&mut self, key: &KeyEvent) {
        let Some(draft) = &self.draft else {
            self.not_configured();
            return;
        };
        let _ = draft;
        // Also when the search matches nothing.
        if key.code == KeyCode::Char('/') {
            self.tunnels_filter.start();
            return;
        }
        let rows = self.visible_tunnels();
        let count = rows.len();
        let selected = self.tunnels_selected.min(count.saturating_sub(1));
        let Some(row) = rows.into_iter().nth(selected) else {
            return;
        };
        let [a, b] = row.sites.clone();
        if let Some(n) = nav(key.code, selected, count, self.scrolls.tunnels.page()) {
            self.tunnels_selected = n;
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('e') => {
                if row.servers.is_none() {
                    self.say(
                        MessageKind::Info,
                        format!("{a} and {b} have no tunnel with each other; d removes the entry."),
                    );
                } else if self.may_change_tunnel(&a, &b)
                    && let Some(draft) = &self.draft
                {
                    self.modal = Some(Modal::Form(Form::tunnel_vlans(draft, &a, &b)));
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if row.limit.is_none() {
                    self.say(
                        MessageKind::Info,
                        format!("The tunnel {a} – {b} has no limit of its own."),
                    );
                } else if self.may_change_tunnel(&a, &b)
                    && let Some(draft) = &mut self.draft
                {
                    draft.network.set_tunnel_limit(&a, &b, None);
                    draft.revalidate();
                    self.clamp_selection();
                    self.say(
                        MessageKind::Info,
                        format!(
                            "The tunnel {a} – {b} carries all VLANs both sites have again (in the draft)."
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    fn on_log_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Char('/') => self.log.text.start(),
            KeyCode::Char('t') => {
                if self.log.zone.is_none() {
                    self.say(
                        MessageKind::Info,
                        "No local time zone found (TZ, /etc/localtime): the log shows UTC.",
                    );
                } else {
                    self.log.utc = !self.log.utc;
                }
            }
            KeyCode::Char('l') => self.log.cycle_level(),
            KeyCode::Char('p') => {
                let peers: Vec<String> = self
                    .status
                    .as_ref()
                    .map(|s| s.peers.iter().map(|p| p.name.clone()).collect())
                    .unwrap_or_default();
                self.log.cycle_peer(&peers);
            }
            _ => {
                self.log.handle_key(key);
            }
        }
    }
}

/// Text of the countdown banner, if a change waits for confirmation.
pub fn banner_text(pending: &PendingChange, needs_esc: bool) -> String {
    let what = match &pending.source {
        ChangeSource::Local => "UNCONFIRMED CHANGE".to_string(),
        // Confirmed by the daemon itself once the other side answers.
        ChangeSource::Sync { peer } => {
            return format!(
                " CHANGE FROM {peer}: confirmed by itself once {peer} answers (at most {} s) ",
                pending.remaining_secs
            );
        }
        ChangeSource::Join { site } => {
            return format!(
                " SITE {site} JOINING: confirmed by itself once {site} answers (at most {} s) ",
                pending.remaining_secs
            );
        }
    };
    let keys = if needs_esc {
        "Esc, then c confirm / r roll back"
    } else {
        "c confirm  |  r roll back now"
    };
    format!(
        " {what}: rollback in {} s  |  {keys} ",
        pending.remaining_secs
    )
}

#[cfg(test)]
mod tests;
