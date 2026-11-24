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
            ("1-6, Tab", "switch view (Shift-Tab: previous)"),
            ("a", "review the draft (diff) and apply it"),
            ("x", "discard the draft"),
            ("c", "confirm a pending change"),
            ("r", "roll a pending change back now"),
            ("?", "this help"),
            ("q", "quit (asks if the draft has unapplied changes)"),
            ("Ctrl-C", "quit immediately"),
        ],
        text: "",
    },
    Section {
        title: "1 Overview",
        keys: &[
            ("Up/Down", "select a peer"),
            ("u", "unblock the port of the peer (loop guard)"),
            ("p", "measure the path MTU to all peers now"),
        ],
        text: "Green: reachable and up. Yellow: degraded (port down, config differs, \
               late replies). Red: unreachable or blocked by the loop guard.",
    },
    Section {
        title: "2 Sites",
        keys: &[
            ("Up/Down", "select a site"),
            ("i", "invite a new site (shows the command to run there)"),
            ("n", "new site by hand"),
            (
                "e, Enter",
                "edit the site (renaming updates all references)",
            ),
            ("d, Del", "delete the site and its VLAN mappings"),
            (
                "m",
                "network settings (VLANs, port, MTU probe, timeout, MSS)",
            ),
            ("l", "local settings of this host: self, trunk"),
            ("u", "unblock the tunnel port to the site"),
        ],
        text: "Every site has a tunnel to every other site. Edit a site to limit it \
               to some VLANs (like the allowed VLANs of a switch port).",
    },
    Section {
        title: "3 VLANs",
        keys: &[
            ("v", "edit the default VLANs of the network (e.g. 10-50,60)"),
            ("n", "new mapping (native or local VLAN -> shared VLAN)"),
            ("e, Enter", "edit the mapping"),
            ("d, Del", "delete the mapping"),
        ],
        text: "",
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

/// Keys shown in the help bar for the current context, most important
/// first.
pub fn bar(app: &App) -> Vec<Binding> {
    if let Some(modal) = &app.modal {
        return match modal {
            Modal::Help { .. } => vec![("Up/Down", "scroll"), ("Esc", "close")],
            Modal::Ask(_) => vec![("y", "yes"), ("n", "no")],
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
        vec![("1-6", "views")]
    } else {
        match app.view {
            View::Overview => vec![("Up/Down", "peer"), ("u", "unblock"), ("p", "probe MTU")],
            View::Sites => vec![
                ("i", "invite"),
                ("n", "new"),
                ("e", "edit"),
                ("d", "delete"),
                ("m", "network"),
                ("l", "local"),
            ],
            View::Vlans => vec![
                ("v", "VLANs"),
                ("n", "new map"),
                ("e", "edit"),
                ("d", "delete"),
            ],
            View::Macs if app.macs.editing => {
                return vec![("Enter", "keep filter"), ("Esc", "clear filter")];
            }
            View::Macs => vec![("/", "filter"), ("s", "sort"), ("o", "order")],
            View::Log => vec![
                ("l", "level"),
                ("p", "peer"),
                ("Up/Down", "scroll"),
                ("End", "follow"),
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
            keys.push(("1-6", "views"));
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
