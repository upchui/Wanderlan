//! The setup wizard for a site without configuration.
//!
//! It asks for the minimum a site needs to join a mesh: its own name, role,
//! trunk interface, transport address, the shared VLANs and one peer. The
//! result is a mesh with two sites and a `local.toml`; the rest of the mesh
//! arrives through the configuration sync once the peer lists this site.

use std::net::Ipv4Addr;
use std::str::FromStr as _;

use crossterm::event::{KeyCode, KeyEvent};
use wanderlan_core::api::Interface;
use wanderlan_core::config::{
    Local, Mesh, Role, Site, VlanSet, has_errors, ifname_error, site_name_error, validate,
};
use wanderlan_core::{BRIDGE_NAME, topology::GRETAP_PREFIX};

use super::form::trunk_candidates;
use super::input::{Outcome, TextInput};

/// Default shared VLANs offered by the wizard.
pub const DEFAULT_VLANS: &str = "10-50";

/// Steps of the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Explanation.
    Welcome,
    /// Name of this site.
    Name,
    /// Hub or spoke.
    Role,
    /// Trunk interface.
    Trunk,
    /// Transport address of this site.
    Address,
    /// Shared VLANs.
    Vlans,
    /// The first peer.
    Peer,
    /// Summary before applying.
    Summary,
    /// Applied, waiting for confirmation.
    Applied,
}

impl Step {
    /// Number of numbered steps.
    pub const COUNT: usize = 7;

    /// Position in "step n of 7", if numbered.
    pub fn number(self) -> Option<usize> {
        match self {
            Self::Welcome | Self::Applied => None,
            Self::Name => Some(1),
            Self::Role => Some(2),
            Self::Trunk => Some(3),
            Self::Address => Some(4),
            Self::Vlans => Some(5),
            Self::Peer => Some(6),
            Self::Summary => Some(7),
        }
    }

    /// Heading of the step.
    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Name => "Name of this site",
            Self::Role => "Role of this site",
            Self::Trunk => "Trunk interface",
            Self::Address => "Transport address",
            Self::Vlans => "Shared VLANs",
            Self::Peer => "First peer",
            Self::Summary => "Summary",
            Self::Applied => "Waiting for confirmation",
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
    /// Show the diff of the built configuration and apply it.
    Review,
    /// The trunk step needs the interface list.
    LoadInterfaces,
}

/// State of the wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wizard {
    /// Current step.
    pub step: Step,
    /// Name of this site.
    pub name: TextInput,
    /// Role of this site.
    pub role: Role,
    /// Selected entry of the interface list; one past the last entry means
    /// "type a name".
    pub trunk_selected: usize,
    /// Manually entered trunk name.
    pub trunk_manual: TextInput,
    /// Transport address of this site.
    pub address: TextInput,
    /// Shared VLANs.
    pub vlans: TextInput,
    /// Name of the first peer.
    pub peer_name: TextInput,
    /// Address of the first peer.
    pub peer_address: TextInput,
    /// Focused peer field: 0 name, 1 address.
    pub peer_focus: usize,
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
            step: Step::Welcome,
            name: TextInput::default(),
            role: Role::Spoke,
            trunk_selected: 0,
            trunk_manual: TextInput::default(),
            address: TextInput::default(),
            vlans: TextInput::new(DEFAULT_VLANS),
            peer_name: TextInput::default(),
            peer_address: TextInput::default(),
            peer_focus: 0,
            error: None,
        }
    }

    /// Role of the first peer: a spoke's first peer is its hub, a hub's
    /// first peer is another hub.
    pub fn peer_role(&self) -> Role {
        Role::Hub
    }

    /// True if the "type a name" entry of the trunk list is selected.
    pub fn trunk_is_manual(&self, interfaces: &[Interface]) -> bool {
        self.trunk_selected >= trunk_candidates(interfaces).len()
    }

    /// True while printable keys go into a text field.
    pub fn captures_text(&self, interfaces: &[Interface]) -> bool {
        match self.step {
            Step::Name | Step::Address | Step::Vlans | Step::Peer => true,
            Step::Trunk => self.trunk_is_manual(interfaces),
            Step::Welcome | Step::Role | Step::Summary | Step::Applied => false,
        }
    }

    /// The chosen trunk interface.
    pub fn trunk(&self, interfaces: &[Interface]) -> String {
        trunk_candidates(interfaces)
            .get(self.trunk_selected)
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

    /// Handles a key. Returns `None` if the key is not for the wizard (so
    /// global keys such as view switching still work).
    pub fn handle_key(&mut self, key: &KeyEvent, interfaces: &[Interface]) -> Option<Action> {
        let back = |step| match step {
            Step::Welcome => None,
            Step::Name => Some(Step::Welcome),
            Step::Role => Some(Step::Name),
            Step::Trunk => Some(Step::Role),
            Step::Address => Some(Step::Trunk),
            Step::Vlans => Some(Step::Address),
            Step::Peer => Some(Step::Vlans),
            Step::Summary => Some(Step::Peer),
            Step::Applied => None,
        };
        if key.code == KeyCode::Esc {
            return match back(self.step) {
                Some(step) => Some(self.go(step)),
                None if self.step == Step::Welcome => Some(Action::Leave),
                None => None,
            };
        }
        match self.step {
            Step::Welcome => (key.code == KeyCode::Enter).then(|| self.go(Step::Name)),
            Step::Name => self.text_step(key, Step::Role),
            Step::Role => match key.code {
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char(' ') => {
                    self.role = match self.role {
                        Role::Hub => Role::Spoke,
                        Role::Spoke => Role::Hub,
                    };
                    Some(Action::None)
                }
                KeyCode::Char('h') => {
                    self.role = Role::Hub;
                    Some(Action::None)
                }
                KeyCode::Char('s') => {
                    self.role = Role::Spoke;
                    Some(Action::None)
                }
                KeyCode::Enter => Some(self.go(Step::Trunk)),
                _ => None,
            },
            Step::Trunk => self.trunk_key(key, interfaces),
            Step::Address => self.text_step(key, Step::Vlans),
            Step::Vlans => self.text_step(key, Step::Peer),
            Step::Peer => self.peer_key(key),
            Step::Summary => (key.code == KeyCode::Enter).then(|| match self.build(interfaces) {
                Ok(_) => Action::Review,
                Err(e) => {
                    self.error = Some(e);
                    Action::None
                }
            }),
            Step::Applied => None,
        }
    }

    fn text_step(&mut self, key: &KeyEvent, next: Step) -> Option<Action> {
        if key.code == KeyCode::Enter {
            return Some(match self.check_step() {
                Ok(()) => self.go(next),
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
            _ => &mut self.vlans,
        };
        if input.handle(key) == Outcome::Edited {
            self.error = None;
        }
        Some(Action::None)
    }

    fn trunk_key(&mut self, key: &KeyEvent, interfaces: &[Interface]) -> Option<Action> {
        let entries = trunk_candidates(interfaces).len() + 1;
        match key.code {
            KeyCode::Up => {
                self.trunk_selected = self.trunk_selected.min(entries - 1).saturating_sub(1);
                self.error = None;
                Some(Action::None)
            }
            KeyCode::Down => {
                self.trunk_selected = (self.trunk_selected + 1).min(entries - 1);
                self.error = None;
                Some(Action::None)
            }
            KeyCode::Enter => Some(match self.check_trunk(interfaces) {
                Ok(()) => self.go(Step::Address),
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

    fn peer_key(&mut self, key: &KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                self.peer_focus = 1 - self.peer_focus.min(1);
                Some(Action::None)
            }
            KeyCode::Enter => {
                if self.peer_focus == 0 {
                    match self.check_peer_name() {
                        Ok(()) => {
                            self.peer_focus = 1;
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e),
                    }
                    return Some(Action::None);
                }
                Some(match self.check_step() {
                    Ok(()) => self.go(Step::Summary),
                    Err(e) => {
                        self.error = Some(e);
                        Action::None
                    }
                })
            }
            _ => {
                let input = if self.peer_focus == 0 {
                    &mut self.peer_name
                } else {
                    &mut self.peer_address
                };
                if input.handle(key) == Outcome::Edited {
                    self.error = None;
                }
                Some(Action::None)
            }
        }
    }

    fn check_step(&self) -> Result<(), String> {
        match self.step {
            Step::Name => site_name_error(self.name.value().trim()).map_or(Ok(()), Err),
            Step::Address => parse_address(self.address.value()).map(|_| ()),
            Step::Vlans => VlanSet::from_str(self.vlans.value())
                .map(|_| ())
                .map_err(|e| format!("{e} (e.g. 10-50 or 10-50,60)")),
            Step::Peer => {
                self.check_peer_name()?;
                let own = parse_address(self.address.value())?;
                let peer = parse_address(self.peer_address.value())?;
                if own == peer {
                    return Err(format!("{peer} is the address of this site"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn check_peer_name(&self) -> Result<(), String> {
        let peer = self.peer_name.value().trim();
        if let Some(e) = site_name_error(peer) {
            return Err(e);
        }
        if peer == self.name.value().trim() {
            return Err(format!("\"{peer}\" is the name of this site"));
        }
        Ok(())
    }

    fn check_trunk(&self, interfaces: &[Interface]) -> Result<(), String> {
        let trunk = self.trunk(interfaces);
        if let Some(e) = ifname_error(&trunk) {
            return Err(e);
        }
        if trunk.starts_with(GRETAP_PREFIX) || trunk == BRIDGE_NAME {
            return Err(format!("\"{trunk}\" is an interface managed by wanderlan"));
        }
        Ok(())
    }

    /// Builds the minimal mesh (this site and the first peer) and the local
    /// configuration.
    pub fn build(&self, interfaces: &[Interface]) -> Result<(Mesh, Local), String> {
        let name = self.name.value().trim().to_string();
        let peer = self.peer_name.value().trim().to_string();
        let vlans = VlanSet::from_str(self.vlans.value()).map_err(|e| e.to_string())?;
        self.check_trunk(interfaces)?;
        let mut mesh = Mesh::new(vlans);
        mesh.sites.push(Site {
            name: name.clone(),
            role: self.role,
            hub: (self.role == Role::Spoke).then(|| peer.clone()),
            address: parse_address(self.address.value())?,
        });
        mesh.sites.push(Site {
            name: peer,
            role: self.peer_role(),
            hub: None,
            address: parse_address(self.peer_address.value())?,
        });
        let local = Local {
            self_name: Some(name),
            trunk: Some(self.trunk(interfaces)),
        };
        let diags = validate(&mesh, Some(&local));
        if has_errors(&diags) {
            let first = diags
                .iter()
                .find(|d| d.is_error())
                .map(|d| d.message.clone())
                .unwrap_or_default();
            return Err(first);
        }
        Ok((mesh, local))
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

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use wanderlan_core::config::{render_local, render_mesh};

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

    /// Runs through all steps as a spoke `haus4` of hub `heim`.
    fn completed() -> Wizard {
        let mut w = Wizard::new();
        press(&mut w, KeyCode::Enter);
        type_text(&mut w, "haus4");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Role);
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Trunk);
        press(&mut w, KeyCode::Down);
        press(&mut w, KeyCode::Down);
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Address);
        type_text(&mut w, "10.8.0.14");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Vlans);
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Peer);
        type_text(&mut w, "heim");
        press(&mut w, KeyCode::Enter);
        type_text(&mut w, "10.8.0.1");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Summary, "{:?}", w.error);
        w
    }

    #[test]
    fn builds_minimal_spoke_configuration() {
        let mut w = completed();
        assert_eq!(press(&mut w, KeyCode::Enter), Some(Action::Review));
        let (mesh, local) = w.build(&fixtures::interfaces()).unwrap();
        insta::assert_snapshot!("wizard_spoke_mesh", render_mesh(&mesh));
        assert_eq!(
            render_local(&local),
            "# wanderlan site-local configuration (never synchronised).\n\nself = \"haus4\"\ntrunk = \"ens19\"\n"
        );
    }

    #[test]
    fn hub_peers_with_another_hub() {
        let mut w = completed();
        w.role = Role::Hub;
        let (mesh, _) = w.build(&fixtures::interfaces()).unwrap();
        assert!(mesh.sites.iter().all(|s| s.role == Role::Hub));
        assert!(mesh.sites.iter().all(|s| s.hub.is_none()));
    }

    #[test]
    fn rejects_invalid_input_per_step() {
        let mut w = Wizard::new();
        press(&mut w, KeyCode::Enter);
        type_text(&mut w, "Bad Name");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Name);
        assert!(w.error.as_deref().unwrap().contains("may only contain"));
        // Editing clears the error.
        press(&mut w, KeyCode::Backspace);
        assert_eq!(w.error, None);

        let mut w = completed();
        w.step = Step::Peer;
        w.peer_focus = 0;
        w.peer_name.set("haus4");
        press(&mut w, KeyCode::Enter);
        assert!(w.error.as_deref().unwrap().contains("name of this site"));
        w.peer_name.set("heim");
        w.peer_address.set("10.8.0.14");
        w.peer_focus = 1;
        press(&mut w, KeyCode::Enter);
        assert!(w.error.as_deref().unwrap().contains("address of this site"));

        w.step = Step::Address;
        w.address.set("127.0.0.1");
        press(&mut w, KeyCode::Enter);
        assert!(w.error.as_deref().unwrap().contains("not a usable"));

        w.step = Step::Vlans;
        w.vlans.set("10-5000");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Vlans);
    }

    #[test]
    fn trunk_list_hides_loopback_and_managed_and_allows_manual_entry() {
        let interfaces = fixtures::interfaces();
        let names: Vec<&str> = trunk_candidates(&interfaces)
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(names, ["br-lan", "ens18", "ens19", "eth1"]);
        let mut w = Wizard::new();
        w.step = Step::Trunk;
        for _ in 0..10 {
            press(&mut w, KeyCode::Down);
        }
        assert!(w.trunk_is_manual(&interfaces));
        assert!(w.captures_text(&interfaces));
        type_text(&mut w, "gt-x");
        press(&mut w, KeyCode::Enter);
        assert!(w.error.as_deref().unwrap().contains("managed by wanderlan"));
        w.trunk_manual.set("bond0");
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Address);
        assert_eq!(w.trunk(&interfaces), "bond0");
    }

    #[test]
    fn escape_goes_back_and_leaves_on_the_first_step() {
        let mut w = Wizard::new();
        press(&mut w, KeyCode::Enter);
        assert_eq!(w.step, Step::Name);
        assert_eq!(press(&mut w, KeyCode::Esc), Some(Action::None));
        assert_eq!(w.step, Step::Welcome);
        assert_eq!(press(&mut w, KeyCode::Esc), Some(Action::Leave));
        // Digits on the welcome step are not for the wizard (view keys).
        assert_eq!(press(&mut w, KeyCode::Char('1')), None);
    }
}
