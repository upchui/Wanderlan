//! View 6: the setup wizard.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use wanderlan_core::api::{Interface, SiteState};

use super::titled;
use crate::tui::app::App;
use crate::tui::form::trunk_candidates;
use crate::tui::input::TextInput;
use crate::tui::wizard::{Mode, Step};
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
    /// The line that must stay visible (the chosen row of a list).
    focus: Option<usize>,
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
    let configured = app
        .status
        .as_ref()
        .is_some_and(|s| s.state != SiteState::Unconfigured);
    let title = if configured && w.step == Step::Welcome {
        " Setup ".to_string()
    } else {
        match w.number() {
            Some(n) => format!(" Setup - step {n} of {}: {} ", Step::COUNT, w.step.title()),
            None => format!(" Setup: {} ", w.step.title()),
        }
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
        Step::Welcome if configured => configured_page(&mut page, app),
        Step::Welcome => welcome(&mut page, app),
        Step::Target => {
            page.text(
                "The address of the site that created the invitation, in the transport network (as shown by `wanderlan invite` there, e.g. 10.8.0.1). Or paste the whole `wanderlan join ...` line it shows.",
                Style::new(),
            );
            page.blank();
            page.input("Address", &w.target, true);
        }
        Step::Code => {
            page.text(
                "The invitation code, e.g. K7F3Q-9QX2M. Case and dashes do not matter.",
                Style::new(),
            );
            page.blank();
            page.input("Code", &w.code, true);
        }
        Step::Name => {
            page.text(
                "The name is what this site is called in the network, for people: the other sites show it, e.g. in the alias of its tunnel interface (ip link). a-z, 0-9 and '-', at most 12 characters; another site may have the same name.",
                Style::new(),
            );
            page.blank();
            page.input("Site name", &w.name, true);
        }
        Step::Trunk => trunk(&mut page, app),
        Step::Address => {
            page.text(
                "The IPv4 address of this host in the existing transport network, e.g. its VPN address. Every other site builds its tunnel to this address.",
                Style::new(),
            );
            page.blank();
            page.input("Address", &w.address, true);
            if !w.addresses.is_empty() {
                page.blank();
                let list: Vec<String> = w.addresses.iter().map(ToString::to_string).collect();
                page.text(&format!("This host: {}", list.join(", ")), theme::dim());
            }
            if let Some(warning) = w.address_warning() {
                page.blank();
                page.text(&warning, theme::warn());
            }
        }
        Step::Vlans => {
            page.text(
                "Empty: all VLANs and the untagged traffic pass, like on a trunk between two switches. Or list them, e.g. native,10-50 (native = untagged). Later every site can be limited to some of them, and VLANs can be mapped per site (VLANs, 3).",
                Style::new(),
            );
            page.blank();
            page.input("VLANs", &w.vlans, true);
        }
        Step::Summary => summary(&mut page, app),
        Step::Applied => {
            page.text(
                "The configuration is applied and waits for confirmation. Press c to keep it; without confirmation it is rolled back automatically (see the countdown), r rolls back now.",
                theme::warn(),
            );
            page.blank();
            page.text(
                "After confirming, the form for inviting the next site opens (Sites, i).",
                Style::new(),
            );
        }
        Step::Joining => {
            let target = w.target.value().trim().to_string();
            page.text(
                &format!("Asking {target} to let this site join ..."),
                theme::bold(),
            );
        }
    }
    page.error(w.error.as_ref());
    let cursor = page.cursor;
    // A long list scrolls: the chosen row (or the input) stays visible,
    // the error below it too where it fits.
    let height = usize::from(inner.height);
    let focus = page.focus.or(cursor.map(|(row, _)| row));
    let bottom = if w.error.is_some() {
        page.lines.len()
    } else {
        focus.map_or(0, |f| f + 1)
    };
    let offset = bottom
        .saturating_sub(height)
        .min(focus.unwrap_or(usize::MAX));
    let scroll = u16::try_from(offset).unwrap_or_default();
    frame.render_widget(Paragraph::new(page.lines).scroll((scroll, 0)), inner);
    if let Some((row, col)) = cursor {
        let x = inner.x + u16::try_from(col).unwrap_or_default();
        let y = inner.y + u16::try_from(row.saturating_sub(offset)).unwrap_or_default();
        if row >= offset && y < inner.bottom() {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn welcome(page: &mut Page, app: &App) {
    page.text(
        "wanderlan connects sites on layer 2 like one big switch: VLANs pass between all sites, and per site you choose which VLANs take part, map VLAN IDs and put untagged traffic into a VLAN. It does not build a VPN: the sites must already reach each other through an existing transport network (usually a VPN).",
        Style::new(),
    );
    page.blank();
    for (mode, label, what) in [
        (Mode::New, "Start a new network", "this is the first site"),
        (
            Mode::Join,
            "Join an existing network",
            "with an invitation code from any site",
        ),
    ] {
        let chosen = app.wizard.mode == mode;
        let marker = if chosen { ">" } else { " " };
        let style = if chosen {
            theme::focused()
        } else {
            Style::new()
        };
        page.lines.push(Line::from(vec![
            Span::raw(format!("   {marker} ")),
            Span::styled(format!("{label:<26}"), style),
            Span::styled(format!("  {what}"), theme::dim()),
        ]));
    }
    page.blank();
    page.text(
        "Up/Down to choose, Enter to start, Esc to leave the setup.",
        theme::bold(),
    );
}

/// The Setup view of a site that is already part of a network.
fn configured_page(page: &mut Page, app: &App) {
    let site = app
        .status
        .as_ref()
        .and_then(|s| s.site.clone())
        .unwrap_or_default();
    page.text(
        &format!("This site (\"{site}\") is part of a network."),
        theme::bold(),
    );
    page.blank();
    page.text(
        "i   invite a new site: creates a code and shows the command to run on the new site (also in Sites, 2).",
        Style::new(),
    );
    page.text(
        "R   reset this site: removes the bridge, the tunnels, all rules, the configuration and the invitations. Afterwards the site is unconfigured, like after the installation.",
        Style::new(),
    );
    page.text(
        "B   switch to another network of this box. One more network: `wanderlan networks add <name>`, then `wanderlan --net <name>` (its setup opens) or `wanderlan --net <name> join <address> <code>`.",
        Style::new(),
    );
}

fn interface_line(i: &Interface, transport: bool) -> String {
    let kind = if i.kind.is_empty() {
        "physical"
    } else {
        &i.kind
    };
    let state = if i.up { "up" } else { "down" };
    let note = if transport {
        "  (transport, not the trunk)".to_string()
    } else {
        i.master
            .as_ref()
            .map(|m| format!("  (in {m})"))
            .unwrap_or_default()
    };
    format!("{:<16}{kind:<10}{state:<6}mtu {}{note}", i.name, i.mtu)
}

fn trunk(page: &mut Page, app: &App) {
    let w = &app.wizard;
    page.text(
        "The trunk is the local interface that carries the VLANs (tagged) to the switch or hypervisor; it is added to the bridge br0. It must not be the interface that leads to the other sites.",
        Style::new(),
    );
    page.blank();
    if !app.interfaces_loaded {
        match &app.interfaces_error {
            Some(e) => page.text(&format!("Cannot list the interfaces: {e}"), theme::bad()),
            None => page.text("Loading the interface list ...", theme::dim()),
        }
    }
    let selected = w.trunk_index(&app.interfaces);
    let candidates = trunk_candidates(&app.interfaces);
    for (i, iface) in candidates.iter().enumerate() {
        let chosen = i == selected;
        let marker = if chosen { ">" } else { " " };
        let style = if chosen {
            theme::focused()
        } else {
            Style::new()
        };
        if chosen {
            page.focus = Some(page.lines.len());
        }
        let transport = w.transport.as_deref() == Some(iface.name.as_str());
        page.lines.push(Line::from(vec![
            Span::raw(format!("   {marker} ")),
            Span::styled(interface_line(iface, transport), style),
        ]));
    }
    if w.trunk_is_manual(&app.interfaces) {
        page.input("> other:", &w.trunk_manual, true);
    } else {
        page.lines
            .push(Line::styled("     other: type a name", theme::dim()));
    }
}

fn summary(page: &mut Page, app: &App) {
    let w = &app.wizard;
    match w.mode {
        Mode::New => {
            page.text(
                "A new network with this site as its first site:",
                theme::bold(),
            );
            page.blank();
            page.value("This site", w.name.value().trim().to_string());
            page.value("Address", w.address.value().trim().to_string());
            page.value("Trunk", w.trunk(&app.interfaces));
            let vlans = w.vlans.value().trim();
            page.value(
                "VLANs",
                if vlans.is_empty() {
                    "all (every VLAN and untagged)".to_string()
                } else {
                    vlans.to_string()
                },
            );
            page.blank();
            page.text(
                "Enter shows the configuration files and applies them. Afterwards add more sites with an invitation.",
                theme::dim(),
            );
        }
        Mode::Join => {
            page.value("Join via", w.target.value().trim().to_string());
            page.value("Invitation", w.code.value().trim().to_string());
            page.value("This site", w.name.value().trim().to_string());
            page.value("Trunk", w.trunk(&app.interfaces));
            page.blank();
            page.text(
                "Enter asks the inviting site to add this site. It answers with the configuration of the network; this site applies it and confirms it automatically as soon as the inviting site answers.",
                theme::dim(),
            );
        }
    }
}
