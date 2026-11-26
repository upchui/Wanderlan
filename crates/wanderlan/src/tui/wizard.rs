//! The setup wizard for a site without configuration.
//!
//! Two ways to start: a new network, with this site as its first site, or
//! joining an existing network with an invitation from any of its sites
//! (`wanderlan invite` there). Joining only needs the address of the
//! inviting site, the code and the local trunk; everything else comes from
//! the network.

use std::net::Ipv4Addr;
use std::str::FromStr as _;

use crossterm::event::{KeyCode, KeyEvent};
use wanderlan_core::api::{Interface, JoinParams};
use wanderlan_core::config::{
    DEFAULT_CONTROL_PORT, Local, Network, Site, VlanSet, has_errors, ifname_error, site_name_error,
    validate,
};
use wanderlan_core::join;
use wanderlan_core::{BRIDGE_NAME, topology::is_tunnel_name};

use super::form::trunk_candidates;
use super::input::{Outcome, TextInput};

/// Default VLANs offered for a new network: empty, i.e. all VLANs and the
/// untagged traffic, like a trunk between two switches.
pub const DEFAULT_VLANS: &str = "";

/// What the wizard sets up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A new network; this site is its first site.
    New,
    /// Join an existing network with an invitation.
    Join,
}

/// Steps of the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Explanation and choice of the mode.
    Welcome,
    /// Address of the site that created the invitation (join).
    Target,
    /// The invitation code (join).
    Code,
    /// Trunk interface.
    Trunk,
    /// Name of this site.
    Name,
    /// Transport address of this site (new network).
    Address,
    /// VLANs of the network (new network).
    Vlans,
    /// Summary before applying or joining.
    Summary,
    /// Applied, waiting for confirmation (new network).
    Applied,
    /// Waiting for the inviting site (join).
    Joining,
}

impl Step {
    /// Number of numbered steps (both modes have five).
    pub const COUNT: usize = 5;

    /// Heading of the step.
    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Target => "Site that invited you",
            Self::Code => "Invitation code",
            Self::Trunk => "Trunk interface",
            Self::Name => "Name of this site",
            Self::Address => "Address in the transport network",
            Self::Vlans => "VLANs",
            Self::Summary => "Summary",
            Self::Applied => "Waiting for confirmation",
            Self::Joining => "Joining",
        }
    }
}

/// What the app has to do after a key in the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Leave the wizard.
    Leave,
    /// Show the configuration of the new network and apply it.
    Review,
    /// The trunk step needs the interface list.
    LoadInterfaces,
    /// Ask the inviting site to let this site join.
    Join,
}

/// State of the wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wizard {
    /// New network or join.
    pub mode: Mode,
    /// Current step.
    pub step: Step,
    /// Name of this site.
    pub name: TextInput,
    /// The suggested name (from the host name); a name equal to it may get a
    /// suffix when joining if it is taken.
    pub default_name: String,
    /// Selected entry of the interface list; one past the last entry means
    /// "type a name".
    pub trunk_selected: usize,
    /// The user moved the selection (otherwise the suggestion is used).
    pub trunk_touched: bool,
    /// Interface that carries the transport traffic; not suggested as trunk.
    pub transport: Option<String>,
    /// Manually entered trunk name.
    pub trunk_manual: TextInput,
    /// Transport address of this site.
    pub address: TextInput,
    /// VLANs of the network.
    pub vlans: TextInput,
    /// Address of the inviting site.
    pub target: TextInput,
    /// Invitation code.
    pub code: TextInput,
    /// Problem with the current step.
    pub error: Option<String>,
}

impl Default for Wizard {
    fn default() -> Self {
        Self::new()
    }
}

impl Wizard {
    /// A wizard at the welcome step.
    pub fn new() -> Self {
        Self {
            mode: Mode::New,
            step: Step::Welcome,
            name: TextInput::default(),
            default_name: String::new(),
            trunk_selected: 0,
            trunk_touched: false,
            transport: None,
            trunk_manual: TextInput::default(),
            address: TextInput::default(),
            vlans: TextInput::new(DEFAULT_VLANS),
            target: TextInput::default(),
            code: TextInput::default(),
            error: None,
        }
    }

    /// Fills in suggestions: the name from the host name, the address of
    /// the default route. Fields the user already filled are kept.
    pub fn prefill(&mut self, hostname: &str, address: Option<Ipv4Addr>) {
        if self.name.value().is_empty() && !hostname.is_empty() {
            self.default_name = join::site_name_from_hostname(hostname);
            self.name = TextInput::new(self.default_name.clone());
        }
        if self.address.value().is_empty()
            && let Some(a) = address
        {
            self.address = TextInput::new(a.to_string());
        }
    }

    /// Position in "step n of 5", if numbered.
    pub fn number(&self) -> Option<usize> {
        match (self.mode, self.step) {
            (_, Step::Welcome | Step::Applied | Step::Joining) => None,
            (Mode::New, Step::Name) | (Mode::Join, Step::Target) => Some(1),
            (Mode::New, Step::Trunk) | (Mode::Join, Step::Code) => Some(2),
            (Mode::New, Step::Address) | (Mode::Join, Step::Trunk) => Some(3),
            (Mode::New, Step::Vlans) | (Mode::Join, Step::Name) => Some(4),
            (_, Step::Summary) => Some(5),
            _ => None,
        }
    }

    /// The address of the inviting site, if it can be parsed.
    pub fn target_address(&self) -> Option<Ipv4Addr> {
        join::parse_target(self.target.value(), DEFAULT_CONTROL_PORT)
            .ok()
            .map(|(a, _)| a)
    }

    /// Index of the selected trunk entry: the user's choice, or the first
    /// interface that can be a trunk (physical, VLAN, bond, not in another
    /// bridge) and does not carry the transport traffic.
    pub fn trunk_index(&self, interfaces: &[Interface]) -> usize {
        if self.trunk_touched {
            return self.trunk_selected;
        }
        let list = trunk_candidates(interfaces);
        let usable = |i: &&Interface| Some(i.name.as_str()) != self.transport.as_deref();
        list.iter()
            .position(|i| usable(i) && i.is_trunk_candidate())
            .or_else(|| list.iter().position(usable))
            .unwrap_or(0)
    }

    /// True if the "type a name" entry of the trunk list is selected.
    pub fn trunk_is_manual(&self, interfaces: &[Interface]) -> bool {
        self.trunk_index(interfaces) >= trunk_candidates(interfaces).len()
    }

    /// True while printable keys go into a text field.
    pub fn captures_text(&self, interfaces: &[Interface]) -> bool {
        match self.step {
            Step::Name | Step::Address | Step::Vlans | Step::Target | Step::Code => true,
            Step::Trunk => self.trunk_is_manual(interfaces),
            Step::Welcome | Step::Summary | Step::Applied | Step::Joining => false,
        }
    }

    /// The chosen trunk interface.
    pub fn trunk(&self, interfaces: &[Interface]) -> String {
        trunk_candidates(interfaces)
            .get(self.trunk_index(interfaces))
            .map_or_else(
                || self.trunk_manual.value().trim().to_string(),
                |i| i.name.clone(),
            )
    }

    fn go(&mut self, step: Step) -> Action {
        self.step = step;
        self.error = None;
        if step == Step::Trunk {
            Action::LoadInterfaces
        } else {
            Action::None
        }
    }

    fn previous(&self) -> Option<Step> {
        match (self.mode, self.step) {
            (_, Step::Welcome | Step::Applied | Step::Joining) => None,
            (Mode::New, Step::Name) | (Mode::Join, Step::Target) => Some(Step::Welcome),
            (Mode::New, Step::Trunk) => Some(Step::Name),
            (Mode::New, Step::Address) => Some(Step::Trunk),
            (Mode::New, Step::Vlans) => Some(Step::Address),
            (Mode::New, Step::Summary) => Some(Step::Vlans),
            (Mode::Join, Step::Code) => Some(Step::Target),
            (Mode::Join, Step::Trunk) => Some(Step::Code),
            (Mode::Join, Step::Name) => Some(Step::Trunk),
            (Mode::Join, Step::Summary) => Some(Step::Name),
            _ => Some(Step::Welcome),
        }
    }

    fn next(&self) -> Step {
        match (self.mode, self.step) {
            (Mode::New, Step::Name) => Step::Trunk,
            (Mode::New, Step::Trunk) => Step::Address,
            (Mode::New, Step::Address) => Step::Vlans,
            (Mode::Join, Step::Target) => Step::Code,
            (Mode::Join, Step::Code) => Step::Trunk,
            (Mode::Join, Step::Trunk) => Step::Name,
            _ => Step::Summary,
        }
    }

    /// Handles a key. Returns `None` if the key is not for the wizard (so
    /// global keys such as view switching still work).
    pub fn handle_key(&mut self, key: &KeyEvent, interfaces: &[Interface]) -> Option<Action> {
        if key.code == KeyCode::Esc {
            return match self.previous() {
                Some(step) => Some(self.go(step)),
                None if self.step == Step::Welcome => Some(Action::Leave),
                None => None,
            };
        }
        match self.step {
            Step::Welcome => self.welcome_key(key),
            Step::Trunk => self.trunk_key(key, interfaces),
            Step::Name | Step::Address | Step::Vlans | Step::Target | Step::Code => {
                self.text_step(key)
            }
            Step::Summary => (key.code == KeyCode::Enter).then(|| match self.mode {
                Mode::New => match self.build(interfaces) {
                    Ok(_) => Action::Review,
                    Err(e) => {
                        self.error = Some(e);
                        Action::None
                    }
                },
                Mode::Join => match self.join_params(interfaces) {
                    Ok(_) => {
                        self.step = Step::Joining;
                        self.error = None;
                        Action::Join
                    }
                    Err(e) => {
                        self.error = Some(e);
                        Action::None
                    }
                },
            }),
            Step::Applied | Step::Joining => None,
        }
    }

    fn welcome_key(&mut self, key: &KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                self.mode = match self.mode {
                    Mode::New => Mode::Join,
                    Mode::Join => Mode::New,
                };
                Some(Action::None)
            }
            KeyCode::Char('n') => {
                self.mode = Mode::New;
                Some(Action::None)
            }
            KeyCode::Char('j') => {
                self.mode = Mode::Join;
                Some(Action::None)
            }
            KeyCode::Enter => Some(self.go(match self.mode {
                Mode::New => Step::Name,
                Mode::Join => Step::Target,
            })),
            _ => None,
        }
    }

    fn text_step(&mut self, key: &KeyEvent) -> Option<Action> {
        // The whole line the inviting site shows, pasted: address and code.
        if key.code == KeyCode::Enter
            && self.step == Step::Target
            && let Some((target, code)) = split_join_line(self.target.value())
        {
            self.target.set(target);
            self.code.set(code);
            if self.check_step().is_ok() {
                self.step = Step::Code;
                if self.check_step().is_ok() {
                    return Some(self.go(Step::Trunk));
                }
            }
        }
        if key.code == KeyCode::Enter {
            return Some(match self.check_step() {
                Ok(()) => self.go(self.next()),
                Err(e) => {
                    self.error = Some(e);
                    Action::None
                }
            });
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            return None;
        }
        let input = match self.step {
            Step::Name => &mut self.name,
            Step::Address => &mut self.address,
            Step::Target => &mut self.target,
            Step::Code => &mut self.code,
            _ => &mut self.vlans,
        };
        if input.handle(key) == Outcome::Edited {
            self.error = None;
        }
        Some(Action::None)
    }

    fn trunk_key(&mut self, key: &KeyEvent, interfaces: &[Interface]) -> Option<Action> {
        let entries = trunk_candidates(interfaces).len() + 1;
        let current = self.trunk_index(interfaces);
        match key.code {
            KeyCode::Up => {
                self.trunk_selected = current.min(entries - 1).saturating_sub(1);
                self.trunk_touched = true;
                self.error = None;
                Some(Action::None)
            }
            KeyCode::Down => {
                self.trunk_selected = (current + 1).min(entries - 1);
                self.trunk_touched = true;
                self.error = None;
                Some(Action::None)
            }
            KeyCode::Enter => Some(match self.check_trunk(interfaces) {
                Ok(()) => self.go(self.next()),
                Err(e) => {
                    self.error = Some(e);
                    Action::None
                }
            }),
            _ if self.trunk_is_manual(interfaces) => {
                if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                    return None;
                }
                if self.trunk_manual.handle(key) == Outcome::Edited {
                    self.error = None;
                }
                Some(Action::None)
            }
            _ => None,
        }
    }

    fn check_step(&self) -> Result<(), String> {
        match self.step {
            Step::Name => site_name_error(self.name.value().trim()).map_or(Ok(()), Err),
            Step::Address => parse_address(self.address.value()).map(|_| ()),
            Step::Vlans => self.parse_vlans().map(|_| ()),
            Step::Target => {
                join::parse_target(self.target.value(), DEFAULT_CONTROL_PORT).map(|_| ())
            }
            Step::Code => {
                let code = join::normalize_code(self.code.value());
                if code.len() == join::CODE_LEN {
                    Ok(())
                } else {
                    Err(format!(
                        "an invitation code has {} characters, e.g. K7F3Q-9QX2M",
                        join::CODE_LEN
                    ))
                }
            }
            _ => Ok(()),
        }
    }

    fn check_trunk(&self, interfaces: &[Interface]) -> Result<(), String> {
        let trunk = self.trunk(interfaces);
        if let Some(e) = ifname_error(&trunk) {
            return Err(e);
        }
        if is_tunnel_name(&trunk) || trunk == BRIDGE_NAME {
            return Err(format!("\"{trunk}\" is an interface managed by wanderlan"));
        }
        if self.transport.as_deref() == Some(trunk.as_str()) {
            return Err(format!(
                "{trunk} carries the traffic to the other sites; as trunk it would cut this host off. Choose the interface that carries the VLANs."
            ));
        }
        // What the port window refuses, the setup refuses too.
        if let Some(why) = interfaces
            .iter()
            .find(|i| i.name == trunk)
            .and_then(super::ports::blocked)
        {
            return Err(format!("{trunk} cannot be a trunk ({why})"));
        }
        Ok(())
    }

    /// Builds the configuration of a new network: this site alone.
    pub fn build(&self, interfaces: &[Interface]) -> Result<(Network, Local), String> {
        let name = self.name.value().trim().to_string();
        let vlans = self.parse_vlans()?;
        self.check_trunk(interfaces)?;
        let mut network = Network::new(vlans);
        network.sites.push(Site {
            name: name.clone(),
            address: parse_address(self.address.value())?,
            id: None,
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            uplink: None,
            breakout: Default::default(),
        });
        let local = Local {
            self_name: Some(name),
            trunk: vec![self.trunk(interfaces)],
            ..Default::default()
        };
        let diags = validate(&network, Some(&local));
        if has_errors(&diags) {
            let first = diags
                .iter()
                .find(|d| d.is_error())
                .map(|d| d.message.clone())
                .unwrap_or_default();
            return Err(first);
        }
        Ok((network, local))
    }

    /// The VLANs of the new network; empty means all VLANs and untagged.
    fn parse_vlans(&self) -> Result<VlanSet, String> {
        let text = self.vlans.value();
        if text.trim().is_empty() {
            return Ok(VlanSet::all());
        }
        VlanSet::from_str(text).map_err(|e| format!("{e} (e.g. native,10-50; empty for all)"))
    }

    /// The join request.
    pub fn join_params(&self, interfaces: &[Interface]) -> Result<JoinParams, String> {
        join::parse_target(self.target.value(), DEFAULT_CONTROL_PORT)?;
        self.check_trunk(interfaces)?;
        let name = self.name.value().trim().to_string();
        if let Some(e) = site_name_error(&name) {
            return Err(e);
        }
        Ok(JoinParams {
            target: self.target.value().trim().to_string(),
            code: self.code.value().trim().to_string(),
            name_is_default: !self.default_name.is_empty() && name == self.default_name,
            name,
            trunk: self.trunk(interfaces),
        })
    }
}

fn parse_address(text: &str) -> Result<Ipv4Addr, String> {
    let text = text.trim();
    let ip: Ipv4Addr = text
        .parse()
        .map_err(|_| format!("\"{text}\" is not an IPv4 address (e.g. 10.8.0.11)"))?;
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || ip.is_broadcast() {
        return Err(format!("{ip} is not a usable unicast address"));
    }
    Ok(ip)
}

/// `wanderlan join <address> <code>` (or just the two): address and code.
fn split_join_line(text: &str) -> Option<(String, String)> {
    let mut words: Vec<&str> = text.split_whitespace().collect();
    if words.first() == Some(&"wanderlan") {
        words.remove(0);
    }
    if words.first() == Some(&"join") {
        words.remove(0);
    }
    match words.as_slice() {
        [target, code] if join::normalize_code(code).len() == join::CODE_LEN => {
            Some(((*target).to_string(), (*code).to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use wanderlan_core::config::{render_local, render_network};

    use super::*;
    use crate::tui::fixtures;

    fn press(w: &mut Wizard, code: KeyCode) -> Option<Action> {
        w.handle_key(
            &KeyEvent::new(code, KeyModifiers::NONE),
            &fixtures::interfaces(),
        )
    }

    fn type_text(w: &mut Wizard, text: &str) {
        for c in text.chars() {
            press(w, KeyCode::Char(c));
        }
    }

    /// Runs through a new network `heim` (trunk ens19, ens18 carries the
    /// transport traffic).
    fn new_network() -> Wizard {
        let mut w = Wizard::new();
        w.prefill("heim.example.org", Some(Ipv4Addr::new(10, 8, 0, 1)));
        w.transport = Some("ens18".into());
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Name);
        assert_eq!(w.name.value(), "heim", "name from the host name");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Trunk);
        assert_eq!(
            w.trunk(&fixtures::interfaces()),
            "ens19",
            "ens18 is skipped"
        );
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Address);
        assert_eq!(w.address.value(), "10.8.0.1");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Vlans);
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Summary, "{:?}", w.error);
        w
    }

    #[test]
    fn a_new_network_has_this_site_only() {
        let mut w = new_network();
        assert_eq!(press(&mut w, KeyCode::Enter), Some(Action::Review));
        let (network, local) = w.build(&fixtures::interfaces()).unwrap();
        insta::assert_snapshot!("wizard_new_network_config", render_network(&network));
        assert_eq!(
            render_local(&local),
            "# wanderlan site-local configuration (never synchronised).\n\nself = \"heim\"\ntrunk = \"ens19\"\n"
        );
    }

    #[test]
    fn joining_needs_address_code_trunk_and_name() {
        let mut w = Wizard::new();
        w.prefill("wanderlan", None);
        press(&mut w, KeyCode::Down);
        assert_eq!(w.mode, Mode::Join);
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Target);
        assert_eq!(w.number(), Some(1));
        type_text(&mut w, "10.13.0.");
        press(&mut w, KeyCode::Enter);
        assert!(w.error.is_some(), "incomplete address");
        type_text(&mut w, "103");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Code);
        type_text(&mut w, "k7f3q");
        press(&mut w, KeyCode::Enter);
        assert!(
            w.error
                .as_deref()
                .is_some_and(|e| e.contains("10 characters"))
        );
        type_text(&mut w, "-9qx2m");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Trunk);
        w.transport = Some("ens18".into());
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Name);
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Summary);
        assert_eq!(press(&mut w, KeyCode::Enter), Some(Action::Join));
        assert_eq!(w.step, Step::Joining);
        let params = w.join_params(&fixtures::interfaces()).unwrap();
        assert_eq!(params.target, "10.13.0.103");
        assert_eq!(params.code, "k7f3q-9qx2m");
        assert_eq!(params.name, "wanderlan");
        assert!(params.name_is_default);
        assert_eq!(params.trunk, "ens19");

        // The whole line the inviting site shows, pasted at once.
        let mut w = Wizard::new();
        w.mode = Mode::Join;
        w.step = Step::Target;
        type_text(&mut w, "wanderlan join 10.13.0.103 K7F3Q-9QX2M");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Trunk, "{:?}", w.error);
        assert_eq!(
            (w.target.value(), w.code.value()),
            ("10.13.0.103", "K7F3Q-9QX2M")
        );
    }

    #[test]
    fn a_chosen_name_is_not_the_default() {
        let mut w = Wizard::new();
        w.prefill("wanderlan", None);
        w.mode = Mode::Join;
        w.step = Step::Name;
        type_text(&mut w, "2");
        w.target = TextInput::new("10.13.0.103");
        w.code = TextInput::new("K7F3Q-9QX2M");
        let params = w.join_params(&fixtures::interfaces()).unwrap();
        assert_eq!(params.name, "wanderlan2");
        assert!(!params.name_is_default);
    }

    #[test]
    fn the_transport_interface_is_not_accepted_as_trunk() {
        let mut w = new_network();
        w.step = Step::Trunk;
        w.trunk_touched = true;
        w.trunk_selected = trunk_candidates(&fixtures::interfaces())
            .iter()
            .position(|i| i.name == "ens18")
            .unwrap();
        assert_eq!(w.trunk(&fixtures::interfaces()), "ens18");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Trunk);
        assert!(
            w.error
                .as_deref()
                .is_some_and(|e| e.contains("cut this host off"))
        );
    }

    #[test]
    fn esc_goes_back_and_leaves_from_the_start() {
        let mut w = new_network();
        for step in [
            Step::Vlans,
            Step::Address,
            Step::Trunk,
            Step::Name,
            Step::Welcome,
        ] {
            press(&mut w, KeyCode::Esc);
            assert_eq!(w.step, step);
        }
        assert_eq!(press(&mut w, KeyCode::Esc), Some(Action::Leave));
    }

    #[test]
    fn manual_trunk_name_is_validated() {
        let mut w = new_network();
        w.step = Step::Trunk;
        let manual = trunk_candidates(&fixtures::interfaces()).len();
        for _ in 0..manual + 2 {
            press(&mut w, KeyCode::Down);
        }
        assert!(w.trunk_is_manual(&fixtures::interfaces()));
        assert!(w.captures_text(&fixtures::interfaces()));
        type_text(&mut w, "br0");
        press(&mut w, KeyCode::Enter);
        assert!(
            w.error
                .as_deref()
                .is_some_and(|e| e.contains("managed by wanderlan"))
        );
    }
}
