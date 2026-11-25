//! State of the MAC table view: entries, text filter and sort order.

use std::cmp::Ordering;

use crossterm::event::{KeyCode, KeyEvent};
use wanderlan_core::api::MacEntry;
use wanderlan_core::topology::TUNNEL_PREFIX;

use super::input::TextInput;

/// Column the table is sorted by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortKey {
    /// MAC address.
    #[default]
    Mac,
    /// VLAN.
    Vlan,
    /// Bridge port.
    Port,
    /// Age of the entry.
    Age,
}

impl SortKey {
    /// Column name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mac => "MAC",
            Self::Vlan => "VLAN",
            Self::Port => "port",
            Self::Age => "age",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Mac => Self::Vlan,
            Self::Vlan => Self::Port,
            Self::Port => Self::Age,
            Self::Age => Self::Mac,
        }
    }
}

/// The MAC table view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacState {
    /// Entries from the daemon.
    pub entries: Vec<MacEntry>,
    /// True once a table arrived.
    pub loaded: bool,
    /// Error of the last request.
    pub error: Option<String>,
    /// Text filter.
    pub filter: TextInput,
    /// Keys go into the filter.
    pub editing: bool,
    /// Sort column.
    pub sort: SortKey,
    /// Reverse order.
    pub descending: bool,
    /// Selected row of the visible entries.
    pub selected: usize,
}

/// The site behind a bridge port: the peer for tunnel ports, `None` for
/// local ports.
pub fn port_site(port: &str) -> Option<&str> {
    port.strip_prefix(TUNNEL_PREFIX)
}

impl MacState {
    /// Stores a new table.
    pub fn set_entries(&mut self, entries: Vec<MacEntry>) {
        self.entries = entries;
        self.loaded = true;
        self.error = None;
        self.clamp();
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.visible().len().saturating_sub(1));
    }

    /// Entries passing the filter, in display order. All whitespace
    /// separated filter words must match (MAC, VLAN, port or site);
    /// `vlan:20` matches VLAN 20 only.
    pub fn visible(&self) -> Vec<&MacEntry> {
        let words: Vec<String> = self
            .filter
            .value()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut list: Vec<&MacEntry> = self
            .entries
            .iter()
            .filter(|e| {
                let vlan = e.vlan.map(|v| v.to_string()).unwrap_or_default();
                let port = e.port.to_lowercase();
                words.iter().all(|w| match w.strip_prefix("vlan:") {
                    Some(only) => vlan == only,
                    None => e.mac.contains(w.as_str()) || vlan == *w || port.contains(w.as_str()),
                })
            })
            .collect();
        list.sort_by(|a, b| {
            let primary = match self.sort {
                SortKey::Mac => Ordering::Equal,
                SortKey::Vlan => none_last(a.vlan, b.vlan),
                SortKey::Port => a.port.cmp(&b.port).then(none_last(a.vlan, b.vlan)),
                SortKey::Age => none_last(a.age_secs, b.age_secs),
            };
            let order = primary.then_with(|| a.mac.cmp(&b.mac));
            if self.descending {
                order.reverse()
            } else {
                order
            }
        });
        list
    }

    /// Handles a key. Returns false if it is not for the MAC table.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if self.editing {
            match key.code {
                KeyCode::Enter => self.editing = false,
                KeyCode::Esc => {
                    self.editing = false;
                    self.filter.set("");
                }
                _ => {
                    self.filter.handle(key);
                }
            }
            self.clamp();
            return true;
        }
        let rows = self.visible().len();
        match key.code {
            KeyCode::Char('/') => self.editing = true,
            KeyCode::Esc if !self.filter.value().is_empty() => self.filter.set(""),
            KeyCode::Char('s') => self.sort = self.sort.next(),
            KeyCode::Char('o') => self.descending = !self.descending,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected += 1,
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown => self.selected += 10,
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = rows,
            _ => return false,
        }
        self.clamp();
        true
    }
}

fn none_last<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::tui::fixtures;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn macs(state: &MacState) -> Vec<&str> {
        state.visible().iter().map(|e| e.mac.as_str()).collect()
    }

    #[test]
    fn filter_matches_all_words() {
        let mut state = MacState::default();
        state.set_entries(fixtures::macs());
        state.handle_key(&key(KeyCode::Char('/')));
        for c in "buero 20".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        state.handle_key(&key(KeyCode::Enter));
        assert!(!state.editing);
        assert_eq!(macs(&state), ["52:54:00:12:34:56"]);
        state.handle_key(&key(KeyCode::Esc));
        assert_eq!(state.visible().len(), fixtures::macs().len());
    }

    #[test]
    fn sorts_by_each_column() {
        let mut state = MacState::default();
        state.set_entries(fixtures::macs());
        assert_eq!(macs(&state)[0], "02:00:00:00:00:01");
        state.handle_key(&key(KeyCode::Char('s')));
        assert_eq!(state.sort, SortKey::Vlan);
        assert_eq!(state.visible()[0].vlan, Some(10));
        assert_eq!(state.visible().last().unwrap().vlan, None);
        state.handle_key(&key(KeyCode::Char('s')));
        assert_eq!(state.visible()[0].port, "ens19");
        state.handle_key(&key(KeyCode::Char('s')));
        assert_eq!(state.sort, SortKey::Age);
        assert_eq!(state.visible()[0].age_secs, Some(0));
        state.handle_key(&key(KeyCode::Char('o')));
        assert_eq!(state.visible()[0].age_secs, None);
    }

    #[test]
    fn port_sites() {
        assert_eq!(port_site("vx-buero"), Some("buero"));
        assert_eq!(port_site("ens19"), None);
    }
}
