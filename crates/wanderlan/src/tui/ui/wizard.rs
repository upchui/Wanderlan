//! View 6: the setup wizard.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use wanderlan_core::api::{Interface, SiteState};
use wanderlan_core::config::Role;

use super::titled;
use crate::tui::app::App;
use crate::tui::form::trunk_candidates;
use crate::tui::input::TextInput;
use crate::tui::wizard::{Step, Wizard};
use crate::tui::{format, theme};

/// Column where input fields start.
const INPUT_COL: usize = 16;
/// Width of input fields.
const INPUT_WIDTH: usize = 24;

/// Lines of the step plus the cursor position (row, column) within them.
#[derive(Debug, Default)]
struct Page {
    lines: Vec<Line<'static>>,
    cursor: Option<(usize, usize)>,
    width: usize,
}

impl Page {
    fn text(&mut self, text: &str, style: Style) {
        for line in format::wrap(text, self.width.saturating_sub(2)) {
            self.lines.push(Line::styled(format!(" {line}"), style));
        }
    }

    fn blank(&mut self) {
        self.lines.push(Line::raw(""));
    }

    fn input(&mut self, label: &str, input: &TextInput, focused: bool) {
        let (shown, col) = input.window(INPUT_WIDTH - 1);
        let style = if focused {
            theme::focused()
        } else {
            theme::field()
        };
        if focused {
            self.cursor = Some((self.lines.len(), INPUT_COL + col));
        }
        self.lines.push(Line::from(vec![
            Span::styled(format!("   {label:<w$}", w = INPUT_COL - 3), theme::dim()),
            Span::styled(format!("{shown:<INPUT_WIDTH$}"), style),
        ]));
    }

    fn value(&mut self, label: &str, value: String) {
        self.lines.push(Line::from(vec![
            Span::styled(format!("   {label:<w$}", w = INPUT_COL - 3), theme::dim()),
            Span::raw(value),
        ]));
    }

    fn error(&mut self, error: Option<&String>) {
        if let Some(e) = error {
            self.blank();
            self.text(e, theme::bad());
        }
    }
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let w = &app.wizard;
    let title = match w.step.number() {
        Some(n) => format!(" Setup - step {n} of {}: {} ", Step::COUNT, w.step.title()),
        None => format!(" Setup: {} ", w.step.title()),
    };
    let block = titled(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut page = Page {
        width: usize::from(inner.width),
        ..Page::default()
    };
    page.blank();
    match w.step {
        Step::Welcome => welcome(&mut page, app),
        Step::Name => {
            page.text(
                "The name identifies this site in the mesh. It becomes part of the tunnel interface names on the other sites (gt-<name>): a-z, 0-9 and '-', at most 12 characters.",
                Style::new(),
            );
            page.blank();
            page.input("Site name", &w.name, true);
        }
        Step::Role => role(&mut page, w),
        Step::Trunk => trunk(&mut page, app),
        Step::Address => {
            page.text(
                "The IPv4 address of this host in the existing transport network, e.g. its VPN address. The other sites build their GRETAP tunnels to this address.",
                Style::new(),
            );
            page.blank();
            page.input("Address", &w.address, true);
        }
        Step::Vlans => {
            page.text(
                "The VLAN IDs stretched across all sites, e.g. 10-50 or 10-50,60. When this site joins an existing mesh, the value of the mesh replaces this one with the first sync.",
                Style::new(),
            );
            page.blank();
            page.input("Shared VLANs", &w.vlans, true);
        }
        Step::Peer => {
            page.text(
                "The first peer is a site that already runs wanderlan. A spoke's first peer is its hub; a hub's first peer is another hub.",
                Style::new(),
            );
            page.blank();
            page.input("Peer name", &w.peer_name, w.peer_focus == 0);
            page.input("Peer address", &w.peer_address, w.peer_focus == 1);
            page.value("Peer role", w.peer_role().as_str().to_string());
        }
        Step::Summary => summary(&mut page, app),
        Step::Applied => {
            page.text(
                "The configuration is applied and waits for confirmation. Check in the overview (1) that the peer is reachable, then press c to keep it. Without confirmation it is rolled back automatically (see the countdown); r rolls back now.",
                theme::warn(),
            );
            page.blank();
            sync_hint(&mut page, w);
        }
    }
    page.error(w.error.as_ref());
    let cursor = page.cursor;
    frame.render_widget(Paragraph::new(page.lines), inner);
    if let Some((row, col)) = cursor {
        let x = inner.x + u16::try_from(col).unwrap_or_default();
        let y = inner.y + u16::try_from(row).unwrap_or_default();
        if y < inner.bottom() {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn welcome(page: &mut Page, app: &App) {
    page.text(
        "wanderlan stretches VLANs across sites on layer 2. It does not build a VPN: the sites must already reach each other through an existing transport network (usually a VPN), and every site has an address in it.",
        Style::new(),
    );
    page.blank();
    page.text(
        "This wizard creates a minimal configuration: this site and one peer that already runs wanderlan. The full mesh configuration then arrives automatically from that peer as soon as the peer lists this site.",
        Style::new(),
    );
    let configured = app
        .status
        .as_ref()
        .is_some_and(|s| s.state != SiteState::Unconfigured);
    if configured {
        page.blank();
        page.text(
            "This site is already configured. Finishing the wizard replaces its configuration; to change single settings use Sites (2) and VLANs (3) instead.",
            theme::warn(),
        );
    }
    page.blank();
    page.text(
        "Press Enter to start, Esc to leave the setup.",
        theme::bold(),
    );
}

fn role(page: &mut Page, w: &Wizard) {
    page.text(
        "Hubs are fully meshed with each other; a spoke has exactly one tunnel, to its hub. Most branch sites are spokes.",
        Style::new(),
    );
    page.blank();
    for (role, what) in [
        (Role::Hub, "tunnels to all other hubs and to its own spokes"),
        (Role::Spoke, "exactly one tunnel, to its hub"),
    ] {
        let chosen = w.role == role;
        let marker = if chosen { ">" } else { " " };
        let style = if chosen {
            theme::focused()
        } else {
            Style::new()
        };
        page.lines.push(Line::from(vec![
            Span::raw(format!("   {marker} ")),
            Span::styled(format!("{:<6}", role.as_str()), style),
            Span::styled(format!("  {what}"), theme::dim()),
        ]));
    }
}

fn interface_line(i: &Interface) -> String {
    let kind = if i.kind.is_empty() {
        "physical"
    } else {
        &i.kind
    };
    let state = if i.up { "up" } else { "down" };
    let master = i
        .master
        .as_ref()
        .map(|m| format!("  (in {m})"))
        .unwrap_or_default();
    format!("{:<16}{kind:<10}{state:<6}mtu {}{master}", i.name, i.mtu)
}

fn trunk(page: &mut Page, app: &App) {
    let w = &app.wizard;
    page.text(
        "The trunk is the local interface that carries the VLANs (tagged) to the switch or hypervisor. It is added to the bridge br0. Loopback and interfaces created by wanderlan are not listed.",
        Style::new(),
    );
    page.blank();
    if !app.interfaces_loaded {
        match &app.interfaces_error {
            Some(e) => page.text(&format!("Cannot list the interfaces: {e}"), theme::bad()),
            None => page.text("Loading the interface list ...", theme::dim()),
        }
    }
    let candidates = trunk_candidates(&app.interfaces);
    for (i, iface) in candidates.iter().enumerate() {
        let chosen = i == w.trunk_selected;
        let marker = if chosen { ">" } else { " " };
        let style = if chosen {
            theme::focused()
        } else {
            Style::new()
        };
        page.lines.push(Line::from(vec![
            Span::raw(format!("   {marker} ")),
            Span::styled(interface_line(iface), style),
        ]));
    }
    if w.trunk_is_manual(&app.interfaces) {
        page.input("> other:", &w.trunk_manual, true);
    } else {
        page.lines
            .push(Line::styled("     other: type a name", theme::dim()));
    }
}

/// What the peer has to add so that the mesh configuration arrives here.
fn sync_hint(page: &mut Page, w: &Wizard) {
    let name = w.name.value().trim();
    let peer = w.peer_name.value().trim();
    let address = w.address.value().trim();
    let entry = match w.role {
        Role::Spoke => format!("name {name}, role spoke, hub {peer}, address {address}"),
        Role::Hub => format!("name {name}, role hub, address {address}"),
    };
    page.text(
        &format!(
            "Important: the full mesh configuration arrives automatically via sync once {peer} lists this site. The peer has to add this site first: on {peer} open wanderlan, go to Sites (2), press n and enter"
        ),
        theme::bold(),
    );
    page.lines
        .push(Line::styled(format!("     {entry}"), theme::ok()));
    page.text(
        "then apply and confirm it there. Until then only the tunnel to the peer exists.",
        theme::bold(),
    );
}

fn summary(page: &mut Page, app: &App) {
    let w = &app.wizard;
    let peer = w.peer_name.value().trim().to_string();
    let own = match w.role {
        Role::Spoke => format!("{}, spoke of {peer}", w.name.value().trim()),
        Role::Hub => format!("{}, hub", w.name.value().trim()),
    };
    page.value("This site", own);
    page.value("Address", w.address.value().trim().to_string());
    page.value("Trunk", w.trunk(&app.interfaces));
    page.value("Shared VLANs", w.vlans.value().trim().to_string());
    page.value(
        "First peer",
        format!(
            "{peer} ({}) at {}",
            w.peer_role(),
            w.peer_address.value().trim()
        ),
    );
    page.blank();
    sync_hint(page, w);
    page.blank();
    page.text(
        "Enter shows the configuration files and applies them.",
        theme::dim(),
    );
}
