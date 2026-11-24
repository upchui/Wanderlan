//! Key bindings: the help bar of each context and the full help screen.

use super::app::{App, Conn, Modal, ReviewState, View};
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
            ("1-7, Tab", "switch view (Shift-Tab: previous)"),
            ("a", "review the draft (diff) and apply it"),
            ("x", "discard the draft"),
            ("c", "confirm a pending change"),
            ("r", "roll a pending change back now"),
            ("?", "this help"),
            ("M", "the last messages of the status line"),
            ("q", "quit (asks if the draft has unapplied changes)"),
            ("Ctrl-C", "quit immediately"),
        ],
        text: "",
    },
    Section {
        title: "1 Overview",
        keys: &[
            ("Up/Down", "select a peer"),
            ("Enter", "details: traffic graph, round trips, counters"),
            ("u", "unblock the port of the peer (loop guard)"),
            ("p", "measure the path MTU to all peers now"),
        ],
        text: "Green: reachable and up. Yellow: degraded (port down or offline, \
               config differs, late replies). Red: unreachable or blocked by the loop guard.",
    },
    Section {
        title: "2 Sites",
        keys: &[
            ("Up/Down", "select a site"),
            ("i", "invite a new site with its own rules (code + command)"),
            ("I", "open invitations: show, delete"),
            ("o", "take the site online / offline (in the draft)"),
            ("n", "new site by hand"),
            (
                "e, Enter",
                "edit the site; \"Changes\": who may change its VLANs",
            ),
            ("d, Del", "delete the site and its VLAN mappings"),
            (
                "m",
                "network settings (VLANs, port, MTU probe, timeout, MSS)",
            ),
            ("l", "local settings of this host: self, trunk"),
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
            ("n", "new mapping (native or local VLAN -> shared VLAN)"),
            ("e, Enter", "edit the mapping"),
            ("d, Del", "delete the mapping"),
            ("t", "tunnels: what each carries (Enter: limit it)"),
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
        ],
        text: "",
    },
    Section {
        title: "5 Log",
        keys: &[
            ("l", "level filter: all, warn and error, error"),
            ("p", "peer filter: all, then each peer"),
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
            ("R", "on a configured site: reset it (removes everything)"),
        ],
        text: "Unconfigured: start a new network, or join one with an invitation \
               (address of the inviting site and the code).",
    },
    Section {
        title: "7 Settings",
        keys: &[("Up/Down", "choose a setting"), ("Enter", "change it")],
        text: "Every setting of the network and of this host, with an explanation.",
    },
    Section {
        title: "Windows",
        keys: &[
            ("Esc", "close the window"),
            ("n, d", "open invitations: new, delete"),
            ("Enter, q", "also close the peer details"),
        ],
        text: "",
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
            Modal::Ask(_) => vec![("y/Enter", "yes"), ("n/Esc", "no")],
            Modal::Messages { .. } => vec![("Up/Down", "scroll"), ("Esc", "close")],
            Modal::Invites { .. } => vec![
                ("Up/Down", "choose"),
                ("n", "new"),
                ("d", "delete"),
                ("Esc", "close"),
            ],
            Modal::Peer { .. } => vec![("Esc", "close")],
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
    let common: [Binding; 2] = [("?", "help"), ("q", "quit")];
    let mut keys: Vec<Binding> = if !matches!(app.conn, Conn::Connected) {
        vec![("1-7", "views")]
    } else {
        match app.view {
            View::Overview => vec![
                ("Enter", "details"),
                ("u", "unblock"),
                ("p", "probe MTU"),
                ("Up/Down", "choose"),
            ],
            View::Sites => vec![
                ("Enter", "edit"),
                ("i", "invite"),
                ("o", "on/off"),
                ("n", "new"),
                ("d", "delete"),
                ("I", "invites"),
                ("m", "network"),
                ("l", "this host"),
            ],
            View::Settings => vec![("Enter", "change"), ("Up/Down", "choose")],
            View::Vlans if app.vlans_tunnels => vec![
                ("Enter", "limit"),
                ("d", "no limit"),
                ("t", "mappings"),
                ("v", "VLANs"),
                ("o", "own VLANs"),
            ],
            View::Vlans => vec![
                ("n", "new"),
                ("Enter", "edit"),
                ("d", "delete"),
                ("t", "tunnels"),
                ("v", "VLANs"),
                ("o", "own VLANs"),
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
                ("End", "follow"),
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
    if app.view != View::Setup || !matches!(app.conn, Conn::Connected) {
        if app.draft_dirty() {
            keys.push(("a", "apply"));
            keys.push(("x", "discard"));
        } else {
            keys.push(("1-7", "views"));
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
