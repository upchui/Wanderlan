//! Key bindings: the help bar of each context and the full help screen.

use super::app::{App, Conn, Modal, ReviewState, View};
use super::form::FormKind;
use super::format;
use super::wizard::Step;

/// One key binding: key and what it does.
pub type Binding = (&'static str, &'static str);

/// A section of the help screen.
#[derive(Debug)]
pub struct Section {
    /// Heading.
    pub title: &'static str,
    /// Key bindings.
    pub keys: &'static [Binding],
    /// Explanation below the keys.
    pub text: &'static str,
}

/// Content of the help screen.
pub const SECTIONS: &[Section] = &[
    Section {
        title: "Everywhere",
        keys: &[
            ("1-8, Tab", "switch view (Shift-Tab: previous)"),
            ("a", "review the draft (diff) and apply it"),
            ("x", "discard the draft"),
            ("c", "confirm a pending change"),
            ("r", "roll a pending change back now (asks first)"),
            ("?", "this help"),
            ("M", "the last messages of the status line"),
            ("B", "the networks of this box: switch to another"),
            ("q", "quit (asks if the draft has unapplied changes)"),
            ("Ctrl-C", "quit immediately"),
        ],
        text: "",
    },
    Section {
        title: "1 Overview",
        keys: &[
            ("Up/Down", "select a peer, below them a site of the network"),
            ("Enter", "details: traffic graph, round trips, counters"),
            ("o", "the selected site online or offline (to the review)"),
            ("u", "unblock the port of the peer (loop guard)"),
            ("U", "unblock a trunk port (loop guard)"),
            ("p", "measure the path MTU to all peers now"),
            ("z", "the graphs: the last 2 min, 10 min or 1 h"),
        ],
        text: "Green: reachable and up. Yellow: degraded (port down or offline, \
               config differs, late replies). Red: unreachable or blocked by the loop guard. \
               The graphs show what came in above the line (green) and what went out \
               below it (magenta), each on its own scale; the daemon records the last hour, \
               LOAD compares the tunnels.",
    },
    Section {
        title: "2 Sites",
        keys: &[
            ("Up/Down", "select a site"),
            ("/", "search (name, address, VLANs, role, changes)"),
            ("i", "invite a new site with its own rules (code + command)"),
            ("I", "open invitations: show, delete"),
            ("o", "take the site online / offline (in the draft)"),
            ("n", "new site by hand"),
            ("Enter", "details: role, VLANs, tunnels, mappings, rights"),
            ("e", "edit the site; \"Changes\": who may change its VLANs"),
            ("N", "new name: this site always, and its clients"),
            ("d, Del", "delete the site and its VLAN mappings"),
            ("m", "the VLANs of the network"),
            ("l", "trunk ports of this host"),
            ("u", "unblock the tunnel port to the site"),
        ],
        text: "Servers are connected with each other; a client has one tunnel, to its \
               server. Edit a site to limit it to some VLANs (like the allowed VLANs \
               of a switch port), to choose who may change it and what it is connected to.",
    },
    Section {
        title: "3 VLANs",
        keys: &[
            ("v", "VLANs of the network (empty: all; e.g. native,10-50)"),
            ("o", "own VLANs of this site: trunk and own clients only"),
            ("n", "new mapping (source VLAN -> mapped VLAN or native)"),
            ("e, Enter", "edit the mapping"),
            ("d, Del", "delete the mapping"),
            ("t", "tunnels: what each carries (Enter: limit it)"),
            ("/", "search in the mappings or the tunnels"),
        ],
        text: "In the tunnel list (t): Enter limits what a tunnel carries, d lifts \
               the limit; below it says which VLANs this site passes on.",
    },
    Section {
        title: "4 MAC table",
        keys: &[
            (
                "/",
                "filter (all words must match; Enter keeps, Esc clears)",
            ),
            ("s", "sort by the next column (MAC, VLAN, port, age)"),
            ("o", "reverse the order"),
            ("Up/Down, PgUp/PgDn", "scroll"),
        ],
        text: "A filter word vlan:20 shows VLAN 20 only.",
    },
    Section {
        title: "5 Log",
        keys: &[
            ("l", "level filter: all, warn and error, error"),
            ("p", "peer filter: all, then each peer"),
            ("/", "search the messages"),
            ("t", "local time / UTC"),
            ("Up/Down, PgUp/PgDn", "scroll"),
            ("End / Home", "follow new events / oldest event"),
        ],
        text: "",
    },
    Section {
        title: "Forms",
        keys: &[
            ("Tab, Up/Down", "next / previous field"),
            ("Left/Right, Space", "change a choice"),
            ("Enter", "save into the draft"),
            ("Esc", "cancel"),
        ],
        text: "Problems are marked at the field as you type.",
    },
    Section {
        title: "6 Setup",
        keys: &[
            ("Enter", "next step"),
            ("Esc", "previous step; leave on the first"),
            ("i", "on a configured site: invite a new site"),
            ("I", "on a configured site: the open invitations"),
            ("R", "on a configured site: reset it (removes everything)"),
        ],
        text: "Unconfigured: start a new network, or join one with an invitation \
               (address of the inviting site and the code).",
    },
    Section {
        title: "7 Breakout",
        keys: &[
            ("Up/Down", "select a breakout"),
            ("n", "a VLAN of a client uses its own uplink"),
            (
                "e, Enter",
                "edit: uplink (empty: default route), exceptions",
            ),
            ("d, Del", "the VLAN goes through the tunnel again"),
            ("Enter on Uplink", "choose from the interfaces of this box"),
        ],
        text: "Public IPv4 and IPv6 destinations of the VLAN leave through the uplink of \
               that client (masqueraded) instead of the tunnel to its server; its devices \
               keep their gateway and DHCP. Private destinations and the exceptions still \
               go through the tunnel. Only clients break out: a server is where the \
               gateways are. Who changes the client's tunnel decides.",
    },
    Section {
        title: "8 Settings",
        keys: &[("Up/Down", "choose a setting"), ("Enter", "change it")],
        text: "The settings of this host only, each with an explanation: every site sets \
               its own. Trunk and the trunk per tunnel open a list of the interfaces of this host. \
               Saved, a setting is applied at once, not kept in the draft: c keeps it, otherwise \
               it is rolled back after the commit timeout. The other sites follow new ports at once.",
    },
    Section {
        title: "Trunk ports",
        keys: &[
            (
                "Space",
                "choose an interface or take it out; several are fine",
            ),
            ("m", "main port (number 1, never shut by the loop guard)"),
            ("other", "last row: a name not there yet, Enter adds it"),
            ("Enter", "apply the choice at once (c keeps it)"),
        ],
        text: "Grey interfaces cannot be a trunk port, the note says why: they are in \
               a bridge of someone else. A port of another segment moves here when chosen \
               (that segment keeps the rest; below the list it says what changes). A yellow note names the \
               tunnels that run over an interface: it can be chosen, but bridged it no \
               longer carries this host's own traffic. If the tunnels drop, do not confirm. \
               For a tunnel, nothing chosen means the default trunk.",
    },
    Section {
        title: "Windows",
        keys: &[
            ("Esc", "close the window (a form with input asks first)"),
            ("c, r", "confirm / roll back also in windows without text"),
            ("n, d", "open invitations: new, delete"),
            ("q", "also closes the peer, site and VLAN windows"),
            ("Up/Down", "site details: the previous / next site"),
        ],
        text: "",
    },
    Section {
        title: "Peer details and VLANs",
        keys: &[
            ("Up/Down", "a VLAN of the tunnel (those with traffic first)"),
            ("Enter", "that VLAN live: every port, its MACs, a graph"),
            ("Left/Right", "the previous / next peer"),
            ("VLAN: Up/Down", "the previous / next VLAN of this site"),
            ("z", "the graphs: the last 2 min, 10 min or 1 h"),
            ("VLAN: PgUp/PgDn", "scroll its ports (also the mouse wheel)"),
            ("VLAN: m", "the MAC table of this VLAN"),
            ("VLAN: Esc", "back to the peer"),
        ],
        text: "IN is what the bridge receives from a port (the peer or the trunk), \
               OUT what it sends there. The bridge counts per VLAN and port; after \
               the update it is built anew once for that (a short interruption).",
    },
    Section {
        title: "How changes work",
        keys: &[],
        text: "Edits go into a draft, a copy of the running configuration. `a` shows \
               the diff and applies it. The daemon then waits for confirmation (c) \
               for commit_timeout and rolls the change back otherwise, so a change \
               that cuts you off undoes itself. Changes received from other sites \
               show the same countdown.",
    },
];

/// Width the help text is wrapped to.
pub const TEXT_WIDTH: usize = 74;

/// One line of the help screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpLine {
    /// Section heading.
    Title(&'static str),
    /// A key binding.
    Key(&'static str, &'static str),
    /// Explanation.
    Text(String),
    /// Empty line.
    Blank,
}

/// The help screen as lines.
pub fn lines() -> Vec<HelpLine> {
    let mut out = Vec::new();
    for section in SECTIONS {
        out.push(HelpLine::Title(section.title));
        out.extend(section.keys.iter().map(|(k, w)| HelpLine::Key(k, w)));
        if !section.text.is_empty() {
            out.extend(
                format::wrap(section.text, TEXT_WIDTH)
                    .into_iter()
                    .map(HelpLine::Text),
            );
        }
        out.push(HelpLine::Blank);
    }
    out.pop();
    out
}

/// The line of the help screen where the section of `view` starts (the
/// help opens there).
pub fn section_line(view: View) -> u16 {
    let number = format!("{} ", view.index() + 1);
    lines()
        .iter()
        .position(|l| matches!(l, HelpLine::Title(t) if t.starts_with(&number)))
        .and_then(|i| u16::try_from(i).ok())
        .unwrap_or(0)
}

/// Largest useful scroll offset of the help screen.
pub fn max_scroll() -> u16 {
    u16::try_from(lines().len().saturating_sub(1)).unwrap_or(u16::MAX)
}

/// How much a key of the help bar matters: lower is shown first when the
/// bar is too narrow for all keys. Help, quit and close always fit; the
/// draft keys come right after the most important key of the view.
pub fn priority(index: usize, key: &str) -> usize {
    match key {
        "?" | "q" | "Esc" | "Ctrl-C" => 0,
        "a" | "x" => 3,
        _ if index == 0 => 2,
        _ => 2 * index + 2,
    }
}

/// Keys shown in the help bar for the current context, most important
/// first (see [`priority`]).
pub fn bar(app: &App) -> Vec<Binding> {
    if let Some(modal) = &app.modal {
        return match modal {
            Modal::Help { .. } => vec![("Up/Down", "scroll"), ("Esc", "close")],
            Modal::Ask(ask) if ask.action.enter_means_yes() => {
                vec![("y/Enter", "yes"), ("n/Esc", "no")]
            }
            Modal::Ask(_) => vec![("y", "yes"), ("n/Enter/Esc", "no")],
            Modal::Messages { .. } => vec![("Up/Down", "scroll"), ("Esc", "close")],
            Modal::Uplinks(_) => vec![("Up/Down", "choose"), ("Enter", "take it"), ("Esc", "back")],
            Modal::Networks { .. } => {
                vec![("Up/Down", "choose"), ("Enter", "switch"), ("Esc", "close")]
            }
            Modal::Invites { .. } => vec![
                ("Up/Down", "choose"),
                ("n", "new"),
                ("d", "delete"),
                ("Esc", "close"),
            ],
            Modal::Peer { .. } => vec![
                ("Up/Down", "VLAN"),
                ("Enter", "open VLAN"),
                ("Left/Right", "other peer"),
                ("z", "range"),
                ("Esc", "close"),
            ],
            Modal::Vlan { back, .. } => vec![
                ("Up/Down", "other VLAN"),
                ("m", "its MACs"),
                ("z", "range"),
                ("Esc", if back.is_some() { "back" } else { "close" }),
            ],
            Modal::Site { .. } => vec![
                ("Up/Down", "next site"),
                ("e", "edit"),
                ("N", "rename"),
                ("Esc", "close"),
            ],
            Modal::Ports(_) => vec![
                ("Space", "choose"),
                ("m", "main port"),
                ("Enter", "apply"),
                ("Esc", "cancel"),
            ],
            // The settings of this host are applied when saved.
            Modal::Form(f) if matches!(f.kind, FormKind::Local | FormKind::OwnPorts(_)) => vec![
                ("Tab", "next field"),
                ("Left/Right", "choose"),
                ("Enter", "apply"),
                ("Esc", "cancel"),
            ],
            Modal::Form(f) if f.kind == FormKind::Invite => vec![
                ("Tab", "next field"),
                ("Left/Right", "choose"),
                ("Enter", "create invitation"),
                ("Esc", "cancel"),
            ],
            Modal::Form(_) => vec![
                ("Tab", "next field"),
                ("Left/Right", "choose"),
                ("Enter", "save to draft"),
                ("Esc", "cancel"),
            ],
            Modal::Review(review) => match &review.state {
                ReviewState::Applying => vec![("Up/Down", "scroll")],
                ReviewState::Applied(_) => {
                    vec![("c", "confirm"), ("r", "roll back"), ("Esc", "close")]
                }
                ReviewState::Ready | ReviewState::Failed(_) if review.is_valid() => {
                    vec![("Enter", "apply"), ("Up/Down", "scroll"), ("Esc", "back")]
                }
                ReviewState::Ready | ReviewState::Failed(_) => {
                    vec![("Up/Down", "scroll"), ("Esc", "back to the draft")]
                }
            },
        };
    }
    let typing = match app.view {
        View::Sites => app.sites_filter.editing,
        View::Vlans if app.vlans_tunnels => app.tunnels_filter.editing,
        View::Vlans => app.maps_filter.editing,
        View::Log => app.log.text.editing,
        _ => false,
    };
    if typing {
        return vec![("Enter", "keep filter"), ("Esc", "clear filter")];
    }
    let common: [Binding; 2] = [("?", "help"), ("q", "quit")];
    let mut keys: Vec<Binding> = if !matches!(app.conn, Conn::Connected) {
        // Every view shows the same "not reachable" box.
        Vec::new()
    } else {
        match app.view {
            View::Overview => vec![
                ("Enter", "details"),
                ("o", "on/offline"),
                ("u", "unblock"),
                ("p", "probe MTU"),
                ("Up/Down", "choose"),
            ],
            View::Sites => vec![
                ("Enter", "details"),
                ("e", "edit"),
                ("i", "invite"),
                ("o", "on/off"),
                ("N", "rename"),
                ("n", "new"),
                ("d", "delete"),
                ("I", "invites"),
                ("m", "network VLANs"),
                ("l", "trunk ports"),
                ("/", "search"),
            ],
            View::Settings => vec![("Enter", "change"), ("Up/Down", "choose")],
            View::Breakout => vec![
                ("n", "new"),
                ("Enter", "edit"),
                ("d", "remove"),
                ("Up/Down", "choose"),
            ],
            View::Vlans if app.vlans_tunnels => vec![
                ("Enter", "limit"),
                ("d", "no limit"),
                ("t", "mappings"),
                ("v", "VLANs"),
                ("o", "own VLANs"),
                ("/", "search"),
            ],
            View::Vlans => vec![
                ("t", "tunnels"),
                ("n", "new"),
                ("Enter", "edit"),
                ("d", "delete"),
                ("v", "VLANs"),
                ("o", "own VLANs"),
                ("/", "search"),
            ],
            View::Macs if app.macs.editing => {
                return vec![("Enter", "keep filter"), ("Esc", "clear filter")];
            }
            View::Macs => vec![
                ("/", "filter"),
                ("s", "sort"),
                ("o", "order"),
                ("PgUp/PgDn", "page"),
            ],
            View::Log => vec![
                ("l", "level"),
                ("p", "peer"),
                ("t", "UTC/local"),
                ("End", "follow"),
                ("/", "search"),
                ("PgUp/PgDn", "page"),
                ("Home", "oldest"),
            ],
            View::Setup if app.wizard.captures_text(&app.interfaces) => {
                // Letters and digits go into the field: only special keys.
                let mut keys = vec![("Enter", "next"), ("Esc", "back")];
                if app.wizard.step == Step::Trunk {
                    keys.insert(0, ("Up/Down", "choose"));
                }
                keys.push(("Ctrl-C", "quit"));
                return keys;
            }
            View::Setup
                if app.status.as_ref().is_some_and(|s| s.site.is_some())
                    && app.wizard.step == Step::Welcome =>
            {
                vec![("i", "invite a site"), ("R", "reset this site")]
            }
            View::Setup => match app.wizard.step {
                Step::Welcome => vec![
                    ("Up/Down", "choose"),
                    ("Enter", "start"),
                    ("Esc", "leave setup"),
                ],
                Step::Summary => vec![("Enter", "apply / join"), ("Esc", "back")],
                Step::Applied => vec![("c", "confirm"), ("r", "roll back")],
                Step::Joining => vec![],
                _ => vec![("Up/Down", "choose"), ("Enter", "next"), ("Esc", "back")],
            },
        }
    };
    if app.view != View::Setup && matches!(app.conn, Conn::Connected) {
        if app.draft_dirty() {
            keys.push(("a", "apply"));
            keys.push(("x", "discard"));
        } else {
            keys.push(("1-8", "views"));
        }
    }
    keys.dedup();
    keys.extend(common);
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_fits_and_scrolls() {
        assert!(lines().len() > 20);
        assert!(max_scroll() > 0);
        for s in SECTIONS {
            for (key, what) in s.keys {
                assert!(key.len() <= 20, "{key}");
                assert!(what.len() <= 56, "{what}");
            }
        }
    }
}
