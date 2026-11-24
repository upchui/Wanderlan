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
use wanderlan_core::api::{
    ApiError, Applied, ApplyParams, ChangeSource, ConfigInput, ConfigTexts, Confirmed, Event,
    Interface, Invitation, Joined, MacEntry, PeerStatus, PendingChange, PortState, Request,
    SiteState, Status, UnblockParams,
};
use wanderlan_core::config::{Diagnostic, has_errors, render_local, render_mesh};
use wanderlan_core::topology::gretap_name;

use super::diff::{self, DiffLine};
use super::draft::{Draft, Running};
use super::form::{Form, FormEvent, FormKind, trunk_hint};
use super::log::LogState;
use super::macs::MacState;
use super::wizard::{self, Mode, Step, Wizard};
use crate::client::ClientError;
use crate::hostinfo;

/// Ticks (seconds) between connection attempts while the daemon is down.
pub const RETRY_TICKS: u32 = 2;
/// Ticks a message stays in the status line.
const MESSAGE_TICKS: u32 = 10;
/// Ticks between MAC table refreshes while the MAC view is open.
const MAC_REFRESH_TICKS: u64 = 2;
/// Ticks after which an unanswered status request is sent again.
const STATUS_TIMEOUT_TICKS: u64 = 5;
/// Seconds left at which the countdown banner turns red.
pub const URGENT_SECS: u64 = 10;

/// The views, reachable with the digit keys 1-6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Sites and live peer status.
    Overview,
    /// Sites, mesh and local settings.
    Sites,
    /// Shared VLANs and mappings.
    Vlans,
    /// Learned MAC addresses.
    Macs,
    /// Event log.
    Log,
    /// Setup wizard.
    Setup,
}

impl View {
    /// All views in tab order.
    pub const ALL: [Self; 6] = [
        Self::Overview,
        Self::Sites,
        Self::Vlans,
        Self::Macs,
        Self::Log,
        Self::Setup,
    ];

    /// Tab title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Sites => "Sites",
            Self::Vlans => "VLANs",
            Self::Macs => "MAC table",
            Self::Log => "Log",
            Self::Setup => "Setup",
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

/// The tab labels with their first and one-past-last column in the tab bar.
pub fn tab_layout() -> Vec<(View, String, u16, u16)> {
    let mut x = u16::try_from(BRAND.len()).unwrap_or_default();
    View::ALL
        .iter()
        .map(|&view| {
            let label = format!(" {} {} ", view.index() + 1, view.title());
            let start = x;
            x += u16::try_from(label.len()).unwrap_or_default();
            (view, label, start, x)
        })
        .collect()
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
}

/// What a confirmation question does when answered with yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskAction {
    /// Quit the TUI.
    Quit,
    /// Throw the draft away.
    Discard,
    /// Remove a site from the draft.
    DeleteSite(usize),
    /// Remove a VLAN mapping from the draft.
    DeleteMap(usize),
    /// Re-enable a port blocked by the loop guard.
    Unblock(String),
    /// Remove everything wanderlan set up on this site.
    Reset,
}

/// A yes/no question.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// `mesh.toml` to apply.
    pub mesh: String,
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
        let max = u16::try_from(self.lines.len().saturating_sub(1)).unwrap_or(u16::MAX);
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
    /// Answer to `invite`.
    Invitation(Invitation),
    /// Answer to `join`.
    Joined(Joined),
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct InFlight {
    status: Option<u64>,
    config: bool,
    macs: bool,
    interfaces: bool,
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
    /// The setup wizard.
    pub wizard: Wizard,
    /// The invitation created last (shown in the overview while open).
    pub invitation: Option<Invitation>,
    /// Popup, if any.
    pub modal: Option<Modal>,
    /// Status line message.
    pub message: Option<Message>,
    /// A running confirm/rollback/apply.
    pub busy: Option<Busy>,
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
            log: LogState::default(),
            macs: MacState::default(),
            overview_selected: 0,
            sites_selected: 0,
            maps_selected: 0,
            wizard: Wizard::new(),
            invitation: None,
            modal: None,
            message: None,
            busy: None,
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
            Msg::ConnectFailed(e) | Msg::Disconnected(e) => self.on_down(&e),
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

    fn say(&mut self, kind: MessageKind, text: impl Into<String>) {
        self.message = Some(Message {
            kind,
            text: text.into(),
            ttl: MESSAGE_TICKS,
        });
    }

    // ----- connection -------------------------------------------------

    fn on_connected(&mut self, fx: &mut Vec<Effect>) {
        self.conn = Conn::Connected;
        self.attempts = 0;
        self.in_flight = InFlight::default();
        self.seen = None;
        self.log.resubscribe();
        self.request_status(fx);
        self.request_config(fx);
        fx.push(Effect::Request(Request::Subscribe));
        if self.view == View::Setup {
            self.request_interfaces(fx);
        }
    }

    fn on_down(&mut self, error: &ClientError) {
        self.attempts += 1;
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
                if self.view == View::Macs && self.ticks.is_multiple_of(MAC_REFRESH_TICKS) {
                    self.request_macs(fx);
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
        if !self.in_flight.config {
            self.in_flight.config = true;
            fx.push(Effect::Request(Request::GetConfig));
        }
    }

    fn request_macs(&mut self, fx: &mut Vec<Effect>) {
        if self.is_connected() && !self.in_flight.macs {
            self.in_flight.macs = true;
            fx.push(Effect::Request(Request::MacTable));
        }
    }

    fn request_interfaces(&mut self, fx: &mut Vec<Effect>) {
        if self.is_connected() && !self.in_flight.interfaces && !self.interfaces_loaded {
            self.in_flight.interfaces = true;
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
            self.on_down(&ClientError::Closed);
            fx.push(Effect::Disconnect);
            return;
        }
        match (request, result) {
            (_, Ok(Reply::Status(status))) => self.on_status(*status, fx),
            (_, Ok(Reply::Config(texts))) => self.on_config(&texts),
            (_, Ok(Reply::History(events))) => self.log.set_history(events),
            (_, Ok(Reply::Interfaces(list))) => self.on_interfaces(list),
            (_, Ok(Reply::Macs(entries))) => {
                self.in_flight.macs = false;
                self.macs.set_entries(entries);
            }
            (Request::Apply(_), Ok(Reply::Applied(applied))) => self.on_applied(applied, fx),
            (Request::Apply(_), Err(e)) => self.on_apply_failed(e),
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
            (Request::Invite, Ok(Reply::Invitation(inv))) => self.on_invitation(inv, fx),
            (Request::Join(_), Ok(Reply::Joined(joined))) => self.on_joined(&joined, fx),
            (Request::Reset, Ok(_)) => self.on_reset(fx),
            (request, Err(e)) => self.on_request_failed(&request, &e),
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
                self.in_flight.config = false;
                "reading the configuration"
            }
            Request::MacTable => {
                self.in_flight.macs = false;
                self.macs.error = Some(error.to_string());
                return;
            }
            Request::Interfaces => {
                self.in_flight.interfaces = false;
                self.interfaces_error = Some(error.to_string());
                return;
            }
            Request::Confirm => {
                self.busy = None;
                "confirm"
            }
            Request::Rollback => {
                self.busy = None;
                "rollback"
            }
            Request::Subscribe => "log subscription",
            Request::Unblock(_) => "unblock",
            Request::ProbeNow => "MTU probe",
            Request::Validate(_) | Request::Apply(_) | Request::ApplyFiles(_) => "apply",
            Request::Invite | Request::Invites | Request::DeleteInvite(_) => "invitation",
            Request::Join(_) => {
                self.wizard.step = Step::Summary;
                self.wizard.error = Some(match error {
                    ClientError::Api(e) => e.message.clone(),
                    other => other.to_string(),
                });
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
        let seen = (
            status.revision,
            status.fingerprint.clone(),
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
        if had_pending && status.pending.is_none() && self.busy.is_none() {
            self.on_pending_ended(status.state, fx);
        }
        if status.invites == 0 {
            self.invitation = None;
        }
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
            .min(status.peers.len().saturating_sub(1));
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

    fn on_config(&mut self, texts: &ConfigTexts) {
        self.in_flight.config = false;
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
        if !keep {
            self.draft = Draft::from_running(&running);
        }
        self.running = Some(running);
        self.clamp_selection();
    }

    fn on_interfaces(&mut self, list: Vec<Interface>) {
        self.in_flight.interfaces = false;
        self.interfaces = list;
        self.interfaces_loaded = true;
        self.interfaces_error = None;
        if let Some(Modal::Form(form)) = &mut self.modal
            && form.kind == FormKind::Local
        {
            form.set_hint("trunk", trunk_hint(&self.interfaces));
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

    fn on_apply_failed(&mut self, error: ClientError) {
        self.busy = None;
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
        if self.wizard.step == Step::Applied {
            self.setup_done(fx);
        } else {
            self.say(
                MessageKind::Success,
                format!(
                    "Confirmed: mesh revision {} is in effect.",
                    confirmed.revision
                ),
            );
        }
        self.request_status(fx);
        self.request_config(fx);
    }

    /// The new network is confirmed: invite the next site right away.
    fn setup_done(&mut self, fx: &mut Vec<Effect>) {
        self.wizard = Wizard::new();
        self.view = View::Overview;
        self.say(
            MessageKind::Success,
            "Setup complete. Creating an invitation for the next site ...",
        );
        fx.push(Effect::Request(Request::Invite));
    }

    fn on_invitation(&mut self, invitation: Invitation, fx: &mut Vec<Effect>) {
        self.say(
            MessageKind::Success,
            format!("On the new site run: {}", invitation.command),
        );
        self.invitation = Some(invitation);
        self.request_status(fx);
    }

    fn invite(&mut self, fx: &mut Vec<Effect>) {
        if !self.is_connected() {
            self.say(MessageKind::Error, "Not connected to the daemon.");
            return;
        }
        fx.push(Effect::Request(Request::Invite));
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
                "Joined via {} as {} ({} sites); confirmed automatically as soon as {} answers.",
                joined.via, joined.site, joined.sites, joined.via
            ),
        );
        self.request_status(fx);
        self.request_config(fx);
    }

    fn ask_reset(&mut self) {
        self.modal = Some(Modal::Ask(Ask {
            title: "Reset this site".into(),
            text: "Remove the bridge, all tunnels and rules, the configuration and the invitations of this site? It becomes unconfigured, like after the installation. Other sites keep listing it until it is removed there.".into(),
            action: AskAction::Reset,
        }));
    }

    fn on_reset(&mut self, fx: &mut Vec<Effect>) {
        self.busy = None;
        self.wizard = Wizard::new();
        self.wizard
            .prefill(&hostinfo::hostname(), hostinfo::default_address());
        self.invitation = None;
        self.view = View::Setup;
        self.say(
            MessageKind::Success,
            "Reset done: this site is unconfigured again.",
        );
        self.request_status(fx);
        self.request_config(fx);
        self.request_interfaces(fx);
    }

    fn on_setup_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) {
        match key.code {
            KeyCode::Char('i') => self.invite(fx),
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
                .min(d.mesh.sites.len().saturating_sub(1));
            self.maps_selected = self.maps_selected.min(d.mesh.maps.len().saturating_sub(1));
        }
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
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent, fx: &mut Vec<Effect>) {
        if !self.mouse {
            return;
        }
        let code = match mouse.kind {
            MouseEventKind::ScrollUp => KeyCode::Up,
            MouseEventKind::ScrollDown => KeyCode::Down,
            MouseEventKind::Down(MouseButton::Left) if mouse.row == 0 && self.modal.is_none() => {
                if let Some((view, ..)) = tab_layout()
                    .into_iter()
                    .find(|(_, _, start, end)| (*start..*end).contains(&mouse.column))
                {
                    self.set_view(view, fx);
                }
                return;
            }
            _ => return,
        };
        self.on_key(&KeyEvent::new(code, KeyModifiers::NONE), fx);
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
            KeyCode::Char(c @ '1'..='6') => {
                let index = c.to_digit(10).map_or(0, |d| d as usize - 1);
                if let Some(&view) = View::ALL.get(index) {
                    self.set_view(view, fx);
                }
            }
            KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char('q') => self.request_quit(fx),
            KeyCode::Char('a') => self.open_review(),
            KeyCode::Char('x') => self.ask_discard(),
            KeyCode::Char('c') => self.confirm(fx),
            KeyCode::Char('r') => self.rollback(fx),
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

    fn request_quit(&mut self, fx: &mut Vec<Effect>) {
        let text = if self.draft_dirty() {
            "The draft has changes that are not applied. Quit and discard them?".to_string()
        } else if let Some(p) = self.pending() {
            format!(
                "A change is waiting for confirmation and is rolled back in {} s unless confirmed. Quit anyway?",
                p.remaining_secs
            )
        } else {
            fx.push(Effect::Quit);
            return;
        };
        self.modal = Some(Modal::Ask(Ask {
            title: "Quit".into(),
            text,
            action: AskAction::Quit,
        }));
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
        if let Some(p) = self.pending() {
            let text = format!(
                "A change is waiting for confirmation ({} s left): confirm it (c) or roll it back (r) first.",
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
        if self.blocked_by_pending() {
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
        self.modal = Some(Modal::Review(Review {
            origin: Origin::Draft,
            mesh: draft.mesh_text(),
            local: draft.local_text(),
            base_revision: draft.base_revision,
            commit_timeout: draft.mesh.commit_timeout,
            lines: draft.diff(self.running.as_ref()),
            problems: draft.diagnostics.clone(),
            scroll: 0,
            state: ReviewState::Ready,
        }));
    }

    fn confirm(&mut self, fx: &mut Vec<Effect>) {
        if self.pending().is_none() {
            self.say(MessageKind::Info, "No change is waiting for confirmation.");
            return;
        }
        if self.busy.is_none() {
            self.busy = Some(Busy::Confirming);
            fx.push(Effect::Request(Request::Confirm));
        }
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
                    _ => scroll,
                };
                Some(Modal::Help {
                    scroll: scroll.min(super::help::max_scroll()),
                })
            }
            Modal::Ask(ask) => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    self.on_answer(ask.action, fx);
                    None
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => None,
                _ => Some(Modal::Ask(ask)),
            },
            Modal::Form(form) => self.on_form_key(form, key),
            Modal::Review(review) => self.on_review_key(review, key, fx),
        }
    }

    fn on_form_key(&mut self, mut form: Form, key: &KeyEvent) -> Option<Modal> {
        let Some(draft) = &self.draft else {
            return None;
        };
        match form.handle_key(key) {
            FormEvent::Nothing => {}
            FormEvent::Changed => form.revalidate(draft),
            FormEvent::Cancel => return None,
            FormEvent::Submit => {
                if let Some(new) = form.submit(draft) {
                    match form.kind {
                        FormKind::Site(None) => {
                            self.sites_selected = new.mesh.sites.len().saturating_sub(1);
                        }
                        FormKind::Map(None) => {
                            self.maps_selected = new.mesh.maps.len().saturating_sub(1);
                        }
                        _ => {}
                    }
                    self.draft = Some(new);
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
                KeyCode::Char('r') => {
                    self.rollback(fx);
                    return None;
                }
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
        if !review.is_valid() || self.blocked_by_pending() {
            return;
        }
        review.state = ReviewState::Applying;
        self.busy = Some(Busy::Applying);
        fx.push(Effect::Request(Request::Apply(ApplyParams {
            config: ConfigInput {
                mesh: review.mesh.clone(),
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
            AskAction::DeleteSite(index) => {
                if let Some(site) = self.draft.as_mut().and_then(|d| d.remove_site(index)) {
                    self.say(
                        MessageKind::Info,
                        format!("Site {} removed from the draft.", site.name),
                    );
                }
                self.clamp_selection();
            }
            AskAction::DeleteMap(index) => {
                if self.draft.as_mut().is_some_and(|d| d.remove_map(index)) {
                    self.say(
                        MessageKind::Info,
                        format!("VLAN mapping #{} removed from the draft.", index + 1),
                    );
                }
                self.clamp_selection();
            }
            AskAction::Unblock(port) => {
                fx.push(Effect::Request(Request::Unblock(UnblockParams { port })));
            }
            AskAction::Reset => fx.push(Effect::Request(Request::Reset)),
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
        let (mesh, local) = match self.wizard.build(&self.interfaces) {
            Ok(built) => built,
            Err(e) => {
                self.wizard.error = Some(e);
                return;
            }
        };
        let (mesh_text, local_text) = (render_mesh(&mesh), render_local(&local));
        let (old_mesh, old_local) = self
            .running
            .as_ref()
            .map(|r| (r.mesh_text(), r.local_text()))
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
                (&old_mesh, &old_local),
                (&mesh_text, &local_text),
                "running",
                "setup",
            ),
            mesh: mesh_text,
            local: local_text,
            base_revision,
            commit_timeout: mesh.commit_timeout,
            problems: wanderlan_core::config::validate(&mesh, Some(&local)),
            scroll: 0,
            state: ReviewState::Ready,
        }));
    }

    fn on_overview_key(&mut self, key: &KeyEvent, fx: &mut Vec<Effect>) {
        let peers = self.status.as_ref().map_or(0, |s| s.peers.len());
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.overview_selected = self.overview_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overview_selected = (self.overview_selected + 1).min(peers.saturating_sub(1));
            }
            KeyCode::Char('u') => {
                let port = self.selected_peer().map(|p| (p.port.clone(), p.port_state));
                self.ask_unblock(port);
            }
            KeyCode::Char('p') if self.is_connected() => {
                fx.push(Effect::Request(Request::ProbeNow));
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
        let Some(draft) = &self.draft else {
            self.not_configured();
            return;
        };
        let selected = self.sites_selected;
        let count = draft.mesh.sites.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.sites_selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.sites_selected = (selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('n') => self.modal = Some(Modal::Form(Form::site(draft, None))),
            KeyCode::Enter | KeyCode::Char('e') if selected < count => {
                self.modal = Some(Modal::Form(Form::site(draft, Some(selected))));
            }
            KeyCode::Char('m') => self.modal = Some(Modal::Form(Form::mesh(draft))),
            KeyCode::Char('i') => self.invite(fx),
            KeyCode::Char('l') => {
                self.modal = Some(Modal::Form(Form::local(draft, &self.interfaces)));
                self.request_interfaces(fx);
            }
            KeyCode::Char('d') | KeyCode::Delete if selected < count => {
                let Some(site) = draft.mesh.sites.get(selected) else {
                    return;
                };
                if draft.local.self_name.as_deref() == Some(site.name.as_str()) {
                    let text = format!(
                        "{} is this site and cannot be removed (change `self` with l first).",
                        site.name
                    );
                    self.say(MessageKind::Error, text);
                    return;
                }
                let maps = draft.mesh.maps_of(&site.name).count();
                let extra = if maps > 0 {
                    format!(" Its {maps} VLAN mapping(s) are removed too.")
                } else {
                    String::new()
                };
                self.modal = Some(Modal::Ask(Ask {
                    title: "Delete site".into(),
                    text: format!("Remove site {} from the draft?{extra}", site.name),
                    action: AskAction::DeleteSite(selected),
                }));
            }
            KeyCode::Char('u') if selected < count => {
                let port = draft
                    .mesh
                    .sites
                    .get(selected)
                    .map(|s| gretap_name(&s.name))
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
        let Some(draft) = &self.draft else {
            self.not_configured();
            return;
        };
        let selected = self.maps_selected;
        let count = draft.mesh.maps.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.maps_selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.maps_selected = (selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('v') => self.modal = Some(Modal::Form(Form::vlans(draft))),
            KeyCode::Char('n') => self.modal = Some(Modal::Form(Form::map(draft, None, None))),
            KeyCode::Enter | KeyCode::Char('e') if selected < count => {
                self.modal = Some(Modal::Form(Form::map(draft, Some(selected), None)));
            }
            KeyCode::Char('d') | KeyCode::Delete if selected < count => {
                self.modal = Some(Modal::Ask(Ask {
                    title: "Delete VLAN mapping".into(),
                    text: format!("Remove VLAN mapping #{} from the draft?", selected + 1),
                    action: AskAction::DeleteMap(selected),
                }));
            }
            _ => {}
        }
    }

    fn on_log_key(&mut self, key: &KeyEvent) {
        match key.code {
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
pub fn banner_text(pending: &PendingChange) -> String {
    let what = match &pending.source {
        ChangeSource::Local => "UNCONFIRMED CHANGE".to_string(),
        ChangeSource::Sync { peer } => format!("CHANGE FROM {peer}"),
        ChangeSource::Join { site } => format!("SITE {site} JOINING"),
    };
    format!(
        " {what}: rollback in {} s  |  c confirm  |  r roll back now ",
        pending.remaining_secs
    )
}

#[cfg(test)]
mod tests;
