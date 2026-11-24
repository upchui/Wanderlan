//! Modal forms that edit the draft: sites, VLAN mappings, mesh settings and
//! the local settings.
//!
//! Every change re-runs the field parsers and then the full validation on a
//! candidate draft with the edit applied. Diagnostics whose [`FieldPath`]
//! points at one of the form's fields are shown next to that field; other
//! new problems the edit would cause are listed as notes.

use std::net::Ipv4Addr;
use std::str::FromStr as _;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use wanderlan_core::api::Interface;
use wanderlan_core::config::{
    Diagnostic, FieldPath, LocalVid, Role, Severity, Site, VID_MAX, VID_MIN, VlanMap, VlanSet,
    ifname_error, site_name_error,
};

use super::draft::{Draft, is_within};
use super::format;
use super::input::{Outcome, TextInput};

/// What a form edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    /// A site; `None` adds a new one.
    Site(Option<usize>),
    /// A VLAN mapping; `None` adds a new one.
    Map(Option<usize>),
    /// The `[mesh]` settings.
    Mesh,
    /// Only the shared VLANs.
    Vlans,
    /// `local.toml`.
    Local,
}

/// The editable value of a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Free text.
    Text(TextInput),
    /// One of a fixed list of values.
    Choice {
        /// The values.
        options: Vec<String>,
        /// Index of the chosen value.
        selected: usize,
    },
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Key in the configuration file (`name`, `hub`, `vlans`, ...).
    pub key: &'static str,
    /// Label shown in the form.
    pub label: &'static str,
    /// Help text shown below the field.
    pub hint: String,
    /// The value.
    pub input: Input,
    /// Disabled fields are skipped (e.g. `hub` of a hub).
    pub enabled: bool,
    /// The user changed the field.
    pub touched: bool,
    /// Error from parsing or validation.
    pub error: Option<String>,
    /// Warning from validation.
    pub warning: Option<String>,
}

impl Field {
    fn text(
        key: &'static str,
        label: &'static str,
        hint: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            key,
            label,
            hint: hint.into(),
            input: Input::Text(TextInput::new(value)),
            enabled: true,
            touched: false,
            error: None,
            warning: None,
        }
    }

    fn choice(
        key: &'static str,
        label: &'static str,
        hint: impl Into<String>,
        options: Vec<String>,
        value: &str,
    ) -> Self {
        let selected = options.iter().position(|o| o == value).unwrap_or(0);
        Self {
            input: Input::Choice { options, selected },
            ..Self::text(key, label, hint, "")
        }
    }

    /// The current value as text.
    pub fn value(&self) -> &str {
        match &self.input {
            Input::Text(t) => t.value(),
            Input::Choice { options, selected } => {
                options.get(*selected).map_or("", String::as_str)
            }
        }
    }

    /// The error to show. Errors of empty fields the user has not touched
    /// yet (a new form) wait for the first submit.
    pub fn visible_error(&self, submitted: bool) -> Option<&str> {
        let shown = self.touched || submitted || !self.value().is_empty();
        self.error.as_deref().filter(|_| self.enabled && shown)
    }
}

/// Result of a key press in a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormEvent {
    /// Nothing to do.
    Nothing,
    /// A value changed; the form must be revalidated.
    Changed,
    /// The user cancelled.
    Cancel,
    /// The user wants to save.
    Submit,
}

/// A modal form.
#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    /// What the form edits.
    pub kind: FormKind,
    /// Title of the popup.
    pub title: String,
    /// The fields.
    pub fields: Vec<Field>,
    /// Index of the focused field.
    pub focus: usize,
    /// Problems the edit causes that belong to no single field.
    pub notes: Vec<Diagnostic>,
    /// The user tried to save at least once.
    pub submitted: bool,
    candidate: Option<Draft>,
}

impl Form {
    fn new(kind: FormKind, title: impl Into<String>, fields: Vec<Field>, draft: &Draft) -> Self {
        let mut form = Self {
            kind,
            title: title.into(),
            fields,
            focus: 0,
            notes: Vec::new(),
            submitted: false,
            candidate: None,
        };
        form.update_enabled();
        form.revalidate(draft);
        form
    }

    /// Form for a new site (`None`) or an existing one.
    pub fn site(draft: &Draft, index: Option<usize>) -> Self {
        let existing = index.and_then(|i| draft.mesh.sites.get(i));
        let own_name = existing.map(|s| s.name.as_str());
        let mut hubs: Vec<String> = draft
            .mesh
            .hubs()
            .filter(|h| Some(h.name.as_str()) != own_name)
            .map(|h| h.name.clone())
            .collect();
        let (name, role, hub, address) = match existing {
            Some(s) => (
                s.name.clone(),
                s.role,
                s.hub.clone().unwrap_or_default(),
                s.address.to_string(),
            ),
            None => (
                String::new(),
                Role::Spoke,
                default_hub(draft),
                String::new(),
            ),
        };
        if !hub.is_empty() && !hubs.contains(&hub) {
            hubs.push(hub.clone());
        }
        let title = match existing {
            Some(s) => format!("Edit site {}", s.name),
            None => "New site".to_string(),
        };
        let fields = vec![
            Field::text(
                "name",
                "Name",
                "a-z, 0-9 and '-', at most 12 characters",
                name,
            ),
            Field::choice(
                "role",
                "Role",
                "hub: meshed with all hubs; spoke: one tunnel to its hub",
                vec!["hub".into(), "spoke".into()],
                role.as_str(),
            ),
            Field::choice("hub", "Hub", "the hub this spoke connects to", hubs, &hub),
            Field::text(
                "address",
                "Address",
                "IPv4 address in the transport network (e.g. the VPN)",
                address,
            ),
        ];
        Self::new(FormKind::Site(index), title, fields, draft)
    }

    /// Form for a new VLAN mapping (`None`) or an existing one.
    pub fn map(draft: &Draft, index: Option<usize>, site_hint: Option<&str>) -> Self {
        let existing = index.and_then(|i| draft.mesh.maps.get(i));
        let sites: Vec<String> = draft.mesh.sites.iter().map(|s| s.name.clone()).collect();
        let site = existing
            .map(|m| m.site.clone())
            .or_else(|| site_hint.map(str::to_string))
            .or_else(|| draft.local.self_name.clone())
            .unwrap_or_default();
        let (local, shared) = existing.map_or_else(
            || ("native".to_string(), String::new()),
            |m| (m.local.to_string(), m.shared.to_string()),
        );
        let title = match index {
            Some(i) => format!("Edit VLAN mapping #{}", i + 1),
            None => "New VLAN mapping".to_string(),
        };
        let fields = vec![
            Field::choice(
                "site",
                "Site",
                "the site whose trunk uses a different VLAN",
                sites,
                &site,
            ),
            Field::text(
                "local",
                "Local VLAN",
                "\"native\" (untagged on the trunk) or a local VLAN ID",
                local,
            ),
            Field::text(
                "shared",
                "Shared VLAN",
                "VLAN ID in the shared network (one of mesh.vlans)",
                shared,
            ),
        ];
        Self::new(FormKind::Map(index), title, fields, draft)
    }

    /// Form for the `[mesh]` settings.
    pub fn mesh(draft: &Draft) -> Self {
        let m = &draft.mesh;
        let fields = vec![
            vlans_field(&m.vlans),
            Field::text(
                "control_port",
                "Control port",
                "UDP/TCP port of the daemons on the site addresses",
                m.control_port.to_string(),
            ),
            Field::text(
                "mtu_probe_interval",
                "MTU probe every",
                "how often the path MTU is measured, e.g. 5m",
                format::duration(m.mtu_probe_interval),
            ),
            Field::text(
                "commit_timeout",
                "Commit timeout",
                "time to confirm a change before it is rolled back, e.g. 60s",
                format::duration(m.commit_timeout),
            ),
            Field::text(
                "mss",
                "Fixed MSS",
                "empty: derive from the measured path MTU; or 536-1460",
                m.mss.map(|v| v.to_string()).unwrap_or_default(),
            ),
        ];
        Self::new(FormKind::Mesh, "Mesh settings (all sites)", fields, draft)
    }

    /// Form for the shared VLANs only.
    pub fn vlans(draft: &Draft) -> Self {
        let fields = vec![vlans_field(&draft.mesh.vlans)];
        Self::new(FormKind::Vlans, "Shared VLANs", fields, draft)
    }

    /// Form for `local.toml`.
    pub fn local(draft: &Draft, interfaces: &[Interface]) -> Self {
        let sites: Vec<String> = draft.mesh.sites.iter().map(|s| s.name.clone()).collect();
        let own = draft.local.self_name.clone().unwrap_or_default();
        let fields = vec![
            Field::choice(
                "self",
                "This site",
                "which site of the mesh this host is",
                sites,
                &own,
            ),
            Field::text(
                "trunk",
                "Trunk",
                trunk_hint(interfaces),
                draft.local.trunk.clone().unwrap_or_default(),
            ),
        ];
        Self::new(FormKind::Local, "This site (local.toml)", fields, draft)
    }

    /// Looks up a field by key.
    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// Replaces the help text of a field.
    pub fn set_hint(&mut self, key: &str, hint: String) {
        if let Some(f) = self.field_mut(key) {
            f.hint = hint;
        }
    }

    fn field_mut(&mut self, key: &str) -> Option<&mut Field> {
        self.fields.iter_mut().find(|f| f.key == key)
    }

    fn value(&self, key: &str) -> String {
        self.field(key)
            .map(|f| f.value().trim().to_string())
            .unwrap_or_default()
    }

    fn set_error(&mut self, key: &str, message: impl Into<String>) {
        if let Some(f) = self.field_mut(key) {
            f.error.get_or_insert_with(|| message.into());
        }
    }

    /// Handles a key. The caller revalidates on [`FormEvent::Changed`].
    pub fn handle_key(&mut self, key: &KeyEvent) -> FormEvent {
        match key.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Enter => return FormEvent::Submit,
            KeyCode::Tab | KeyCode::Down => {
                self.move_focus(true);
                return FormEvent::Nothing;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_focus(false);
                return FormEvent::Nothing;
            }
            _ => {}
        }
        let Some(field) = self.fields.get_mut(self.focus) else {
            return FormEvent::Nothing;
        };
        let changed = match &mut field.input {
            Input::Choice { options, selected } if !options.is_empty() => match key.code {
                KeyCode::Left => {
                    *selected = (*selected + options.len() - 1) % options.len();
                    true
                }
                KeyCode::Right | KeyCode::Char(' ') => {
                    *selected = (*selected + 1) % options.len();
                    true
                }
                _ => false,
            },
            Input::Choice { .. } => false,
            Input::Text(text) => text.handle(key) == Outcome::Edited,
        };
        if changed {
            field.touched = true;
            FormEvent::Changed
        } else {
            FormEvent::Nothing
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let n = self.fields.len();
        for step in 1..=n {
            let i = if forward {
                (self.focus + step) % n
            } else {
                (self.focus + n - step) % n
            };
            if self.fields.get(i).is_some_and(|f| f.enabled) {
                self.focus = i;
                return;
            }
        }
    }

    fn update_enabled(&mut self) {
        if matches!(self.kind, FormKind::Site(_)) {
            let spoke = self.value("role") == Role::Spoke.as_str();
            if let Some(hub) = self.field_mut("hub") {
                hub.enabled = spoke;
            }
        }
    }

    /// Parses the fields and validates the draft with the edit applied.
    pub fn revalidate(&mut self, draft: &Draft) {
        self.update_enabled();
        for f in &mut self.fields {
            f.error = None;
            f.warning = None;
        }
        self.notes.clear();
        self.candidate = self.build(draft);
        let Some(candidate) = self.candidate.take() else {
            return;
        };
        let entity = self.entity_path(draft);
        for diag in &candidate.diagnostics {
            let key = self
                .fields
                .iter()
                .find(|f| f.enabled && self.path_of(f.key, draft) == diag.path)
                .map(|f| f.key);
            match key {
                Some(key) => {
                    if let Some(field) = self.field_mut(key) {
                        let slot = match diag.severity {
                            Severity::Error => &mut field.error,
                            Severity::Warning => &mut field.warning,
                        };
                        slot.get_or_insert_with(|| diag.message.clone());
                    }
                }
                None => {
                    let own = entity.as_ref().is_some_and(|e| is_within(&diag.path, e));
                    let new = !draft.diagnostics.iter().any(|d| d.message == diag.message);
                    if own || new {
                        self.notes.push(diag.clone());
                    }
                }
            }
        }
        self.candidate = Some(candidate);
    }

    /// True if all fields are valid.
    pub fn is_valid(&self) -> bool {
        self.candidate.is_some() && self.fields.iter().all(|f| !f.enabled || f.error.is_none())
    }

    /// Tries to save: returns the new draft if all fields are valid.
    pub fn submit(&mut self, draft: &Draft) -> Option<Draft> {
        self.submitted = true;
        self.revalidate(draft);
        if self.is_valid() {
            self.candidate.take()
        } else {
            if let Some(i) = self
                .fields
                .iter()
                .position(|f| f.enabled && f.error.is_some())
            {
                self.focus = i;
            }
            None
        }
    }

    /// Index of the edited entity in the candidate draft.
    fn index(&self, draft: &Draft) -> Option<usize> {
        match self.kind {
            FormKind::Site(i) => Some(i.unwrap_or(draft.mesh.sites.len())),
            FormKind::Map(i) => Some(i.unwrap_or(draft.mesh.maps.len())),
            FormKind::Mesh | FormKind::Vlans | FormKind::Local => None,
        }
    }

    fn entity_path(&self, draft: &Draft) -> Option<FieldPath> {
        let i = self.index(draft)?;
        match self.kind {
            FormKind::Site(_) => Some(FieldPath::site(i)),
            FormKind::Map(_) => Some(FieldPath::map(i)),
            FormKind::Mesh | FormKind::Vlans | FormKind::Local => None,
        }
    }

    /// Path of a field in the candidate draft.
    fn path_of(&self, key: &str, draft: &Draft) -> FieldPath {
        let i = self.index(draft).unwrap_or_default();
        match self.kind {
            FormKind::Site(_) => FieldPath::site_field(i, key),
            FormKind::Map(_) => FieldPath::map_field(i, key),
            FormKind::Mesh | FormKind::Vlans => FieldPath::mesh(key),
            FormKind::Local => FieldPath::local(key),
        }
    }

    /// Builds the candidate draft; records parse errors on the fields.
    fn build(&mut self, draft: &Draft) -> Option<Draft> {
        let mut candidate = draft.clone();
        match self.kind {
            FormKind::Site(index) => {
                let site = self.parse_site()?;
                match index.and_then(|i| candidate.mesh.sites.get_mut(i)) {
                    Some(slot) => {
                        let old = std::mem::replace(slot, site.clone());
                        if old.name != site.name {
                            candidate.rename_references(&old.name, &site.name);
                        }
                    }
                    None => candidate.mesh.sites.push(site),
                }
            }
            FormKind::Map(index) => {
                let map = self.parse_map()?;
                match index.and_then(|i| candidate.mesh.maps.get_mut(i)) {
                    Some(slot) => *slot = map,
                    None => candidate.mesh.maps.push(map),
                }
            }
            FormKind::Mesh => {
                let vlans = self.parse_vlans();
                let port = self.parse_port();
                let interval = self.parse_duration("mtu_probe_interval");
                let timeout = self.parse_duration("commit_timeout");
                let mss = self.parse_mss();
                let mesh = &mut candidate.mesh;
                mesh.vlans = vlans?;
                mesh.control_port = port?;
                mesh.mtu_probe_interval = interval?;
                mesh.commit_timeout = timeout?;
                mesh.mss = mss?;
            }
            FormKind::Vlans => candidate.mesh.vlans = self.parse_vlans()?,
            FormKind::Local => {
                let own = self.value("self");
                let trunk = self.value("trunk");
                if let Some(msg) = ifname_error(&trunk) {
                    self.set_error("trunk", msg);
                    return None;
                }
                candidate.local.self_name = (!own.is_empty()).then_some(own);
                candidate.local.trunk = Some(trunk);
            }
        }
        candidate.revalidate();
        Some(candidate)
    }

    fn parse_site(&mut self) -> Option<Site> {
        let name = self.value("name");
        let name_ok = match site_name_error(&name) {
            Some(msg) => {
                self.set_error("name", msg);
                false
            }
            None => true,
        };
        let role = if self.value("role") == Role::Spoke.as_str() {
            Role::Spoke
        } else {
            Role::Hub
        };
        let hub = (role == Role::Spoke)
            .then(|| self.value("hub"))
            .filter(|h| !h.is_empty());
        let address = self.value("address");
        let address = match address.parse::<Ipv4Addr>() {
            Ok(ip) => Some(ip),
            Err(_) => {
                let msg = if address.is_empty() {
                    "address is required".to_string()
                } else {
                    format!("\"{address}\" is not an IPv4 address (e.g. 10.8.0.1)")
                };
                self.set_error("address", msg);
                None
            }
        };
        let address = address?;
        name_ok.then_some(Site {
            name,
            role,
            hub,
            address,
        })
    }

    fn parse_map(&mut self) -> Option<VlanMap> {
        let site = self.value("site");
        if site.is_empty() {
            self.set_error("site", "the mesh has no sites");
        }
        let local_text = self.value("local");
        let local = if local_text.eq_ignore_ascii_case("native") {
            Some(LocalVid::Native)
        } else {
            match parse_vid(&local_text) {
                Ok(v) => Some(LocalVid::Vid(v)),
                Err(msg) => {
                    self.set_error("local", format!("\"native\" or {msg}"));
                    None
                }
            }
        };
        let shared = match parse_vid(&self.value("shared")) {
            Ok(v) => Some(v),
            Err(msg) => {
                self.set_error("shared", msg);
                None
            }
        };
        if site.is_empty() {
            return None;
        }
        Some(VlanMap {
            site,
            local: local?,
            shared: shared?,
        })
    }

    fn parse_vlans(&mut self) -> Option<VlanSet> {
        match VlanSet::from_str(&self.value("vlans")) {
            Ok(set) => Some(set),
            Err(e) => {
                self.set_error("vlans", e.to_string());
                None
            }
        }
    }

    fn parse_port(&mut self) -> Option<u16> {
        match self.value("control_port").parse::<u16>() {
            Ok(p) if p > 0 => Some(p),
            _ => {
                self.set_error("control_port", "a port number between 1 and 65535");
                None
            }
        }
    }

    fn parse_duration(&mut self, key: &str) -> Option<Duration> {
        let text = self.value(key);
        match humantime::parse_duration(&text) {
            Ok(d) => Some(d),
            Err(_) => {
                self.set_error(key, format!("\"{text}\" is not a duration like 60s or 5m"));
                None
            }
        }
    }

    fn parse_mss(&mut self) -> Option<Option<u16>> {
        let text = self.value("mss");
        if text.is_empty() {
            return Some(None);
        }
        match text.parse::<u16>() {
            Ok(v) => Some(Some(v)),
            Err(_) => {
                self.set_error("mss", "empty or a number between 536 and 1460");
                None
            }
        }
    }
}

fn vlans_field(vlans: &VlanSet) -> Field {
    Field::text(
        "vlans",
        "Shared VLANs",
        "VLANs stretched across all sites, e.g. 10-50,60",
        vlans.to_string(),
    )
}

fn parse_vid(text: &str) -> Result<u16, String> {
    text.parse::<u16>()
        .ok()
        .filter(|v| (VID_MIN..=VID_MAX).contains(v))
        .ok_or_else(|| format!("a VLAN ID between {VID_MIN} and {VID_MAX}"))
}

/// Hub suggested for a new spoke: this site if it is a hub, otherwise its
/// hub, otherwise the first hub.
fn default_hub(draft: &Draft) -> String {
    let own = draft
        .local
        .self_name
        .as_deref()
        .and_then(|n| draft.mesh.site(n));
    match own {
        Some(s) if s.role == Role::Hub => s.name.clone(),
        Some(s) if s.hub.is_some() => s.hub.clone().unwrap_or_default(),
        _ => draft
            .mesh
            .hubs()
            .next()
            .map(|h| h.name.clone())
            .unwrap_or_default(),
    }
}

/// Interfaces that can serve as trunk: no loopback, nothing wanderlan
/// created; sorted by name.
pub fn trunk_candidates(interfaces: &[Interface]) -> Vec<&Interface> {
    let mut list: Vec<&Interface> = interfaces
        .iter()
        .filter(|i| i.name != "lo" && !i.managed)
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

/// Help text of the trunk field: the usable interfaces.
pub fn trunk_hint(interfaces: &[Interface]) -> String {
    let names: Vec<&str> = trunk_candidates(interfaces)
        .into_iter()
        .map(|i| i.name.as_str())
        .collect();
    if names.is_empty() {
        "local interface carrying the VLANs (tagged)".to_string()
    } else {
        format!("interfaces: {}", names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::tui::draft::Running;
    use crate::tui::fixtures;

    fn draft() -> Draft {
        Draft::from_running(&Running::parse(&fixtures::config_texts()).unwrap()).unwrap()
    }

    fn type_text(form: &mut Form, draft: &Draft, text: &str) {
        for c in text.chars() {
            let ev = form.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            if ev == FormEvent::Changed {
                form.revalidate(draft);
            }
        }
    }

    fn press(form: &mut Form, draft: &Draft, code: KeyCode) -> FormEvent {
        let ev = form.handle_key(&KeyEvent::new(code, KeyModifiers::NONE));
        if ev == FormEvent::Changed {
            form.revalidate(draft);
        }
        ev
    }

    #[test]
    fn new_site_defaults_to_spoke_of_this_hub() {
        let d = draft();
        let form = Form::site(&d, None);
        assert_eq!(form.field("role").unwrap().value(), "spoke");
        assert_eq!(form.field("hub").unwrap().value(), "heim");
        // Empty required fields are errors, but hidden until touched.
        let name = form.field("name").unwrap();
        assert!(name.error.is_some());
        assert_eq!(name.visible_error(false), None);
        assert!(!form.is_valid());
    }

    #[test]
    fn duplicate_name_is_marked_on_the_name_field() {
        let d = draft();
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "buero");
        press(&mut form, &d, KeyCode::Tab);
        press(&mut form, &d, KeyCode::Tab);
        press(&mut form, &d, KeyCode::Tab);
        type_text(&mut form, &d, "10.8.0.20");
        let name = form.field("name").unwrap();
        assert!(
            name.visible_error(false).unwrap().contains("defined twice"),
            "{name:?}"
        );
        assert!(form.submit(&d).is_none());
        assert_eq!(form.focus, 0, "focus moves to the first invalid field");
    }

    #[test]
    fn adding_a_valid_spoke() {
        let d = draft();
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "haus4");
        form.focus = 3;
        type_text(&mut form, &d, "10.8.0.14");
        let new = form.submit(&d).expect("valid");
        let site = new.mesh.site("haus4").unwrap();
        assert_eq!(site.role, Role::Spoke);
        assert_eq!(site.hub.as_deref(), Some("heim"));
        assert_eq!(new.errors(), 0);
    }

    #[test]
    fn hub_field_follows_role() {
        let d = draft();
        let mut form = Form::site(&d, Some(0));
        assert!(!form.field("hub").unwrap().enabled);
        form.focus = 1;
        assert_eq!(press(&mut form, &d, KeyCode::Right), FormEvent::Changed);
        assert_eq!(form.field("role").unwrap().value(), "spoke");
        assert!(form.field("hub").unwrap().enabled);
        // heim becoming a spoke breaks its spokes: shown as notes.
        assert!(
            form.notes
                .iter()
                .any(|n| n.message.contains("is a spoke, not a hub")),
            "{:?}",
            form.notes
        );
    }

    #[test]
    fn invalid_address_and_vlan_mapping_conflict() {
        let d = draft();
        let mut form = Form::site(&d, Some(1));
        form.focus = 3;
        press(&mut form, &d, KeyCode::Backspace);
        type_text(&mut form, &d, "x");
        assert!(
            form.field("address")
                .unwrap()
                .visible_error(false)
                .unwrap()
                .contains("not an IPv4 address")
        );

        // A second native mapping for haus3 conflicts with map #1.
        let mut map = Form::map(&d, None, Some("haus3"));
        map.focus = 2;
        type_text(&mut map, &d, "40");
        let local = map.field("local").unwrap();
        assert!(
            local
                .error
                .as_deref()
                .unwrap()
                .contains("more than one native mapping"),
            "{local:?}"
        );
    }

    #[test]
    fn mesh_settings_parse_and_validate() {
        let d = draft();
        let mut form = Form::mesh(&d);
        form.focus = 4;
        type_text(&mut form, &d, "100");
        let mss = form.field("mss").unwrap();
        assert!(
            mss.error
                .as_deref()
                .unwrap()
                .contains("between 536 and 1460")
        );
        press(&mut form, &d, KeyCode::Backspace);
        press(&mut form, &d, KeyCode::Backspace);
        press(&mut form, &d, KeyCode::Backspace);
        type_text(&mut form, &d, "1300");
        let new = form.submit(&d).expect("valid");
        assert_eq!(new.mesh.mss, Some(1300));
    }

    #[test]
    fn shrinking_vlans_reports_broken_mappings() {
        let d = draft();
        let mut form = Form::vlans(&d);
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(form.handle_key(&ctrl_u), FormEvent::Changed);
        type_text(&mut form, &d, "10-25");
        assert!(form.is_valid());
        assert!(
            form.notes
                .iter()
                .any(|n| n.message.contains("shared VLAN 30 is not in mesh.vlans")),
            "{:?}",
            form.notes
        );
    }

    #[test]
    fn local_form_rejects_bad_trunk() {
        let d = draft();
        let mut form = Form::local(&d, &fixtures::interfaces());
        assert_eq!(
            form.field("trunk").unwrap().hint,
            "interfaces: br-lan, ens18, ens19, eth1"
        );
        form.focus = 1;
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        form.handle_key(&ctrl_u);
        type_text(&mut form, &d, "gt-x");
        assert!(
            form.field("trunk")
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("managed by wanderlan")
        );
    }
}
