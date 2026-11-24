//! State of the log view: event history, live events, filters, scrolling.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent};
use wanderlan_core::api::{Event, Level};

use super::filter::Filter;
use super::localtime::Zone;

/// Events kept in memory.
pub const MAX_EVENTS: usize = 2000;

/// Filters and scroll position of the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogState {
    /// All events, oldest first.
    pub events: Vec<Event>,
    /// True once the history arrived after `subscribe`.
    pub synced: bool,
    early: Vec<Event>,
    /// Show only events of at least this level.
    pub min_level: Option<Level>,
    /// Show only events about this peer.
    pub peer: Option<String>,
    /// Number of matching events hidden below the window; 0 follows new
    /// events.
    pub scroll: usize,
    /// Text search (`/`).
    pub text: Filter,
    /// The local time zone; `None`: UTC.
    pub zone: Option<Zone>,
    /// Show UTC even with a local zone (`t`).
    pub utc: bool,
}

impl LogState {
    /// The offset (seconds) and name of the shown time at `time_ms`.
    pub fn offset_at(&self, time_ms: u64) -> (i32, String) {
        match (&self.zone, self.utc) {
            (Some(zone), false) => zone.at(i64::try_from(time_ms / 1000).unwrap_or(i64::MAX)),
            _ => (0, "UTC".into()),
        }
    }
}

impl LogState {
    /// Called before `subscribe` is sent on a new connection.
    pub fn resubscribe(&mut self) {
        self.synced = false;
        self.early.clear();
    }

    /// Replaces the log with the history; events that arrived before the
    /// history are appended unless they are part of it.
    pub fn set_history(&mut self, history: Vec<Event>) {
        self.events = history;
        for event in std::mem::take(&mut self.early) {
            if !self.events.contains(&event) {
                self.events.push(event);
            }
        }
        self.synced = true;
        self.trim();
    }

    /// Adds a live event.
    pub fn push(&mut self, event: Event) {
        if !self.synced {
            self.early.push(event);
            return;
        }
        if self.scroll > 0 && self.matches(&event) {
            self.scroll += 1;
        }
        self.events.push(event);
        self.trim();
    }

    fn trim(&mut self) {
        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(..excess);
        }
    }

    /// True if the event passes the filters.
    pub fn matches(&self, event: &Event) -> bool {
        self.min_level.is_none_or(|l| event.level >= l)
            && self
                .peer
                .as_deref()
                .is_none_or(|p| event.peer.as_deref() == Some(p))
            && self.text.matches(&[
                &event.message,
                event.peer.as_deref().unwrap_or_default(),
                level_name(event.level),
            ])
    }

    /// Events passing the filters, oldest first.
    pub fn filtered(&self) -> Vec<&Event> {
        self.events.iter().filter(|e| self.matches(e)).collect()
    }

    /// Peers mentioned in the log, sorted.
    pub fn peers(&self) -> Vec<String> {
        let set: BTreeSet<&str> = self
            .events
            .iter()
            .filter_map(|e| e.peer.as_deref())
            .collect();
        set.into_iter().map(str::to_string).collect()
    }

    /// all → warn → error → all.
    pub fn cycle_level(&mut self) {
        self.min_level = match self.min_level {
            None | Some(Level::Info) => Some(Level::Warn),
            Some(Level::Warn) => Some(Level::Error),
            Some(Level::Error) => None,
        };
        self.scroll = 0;
    }

    /// all → each peer in turn → all.
    pub fn cycle_peer(&mut self, extra: &[String]) {
        let mut peers = self.peers();
        for p in extra {
            if !peers.contains(p) {
                peers.push(p.clone());
            }
        }
        peers.sort();
        self.peer = match &self.peer {
            None => peers.first().cloned(),
            Some(current) => peers
                .iter()
                .position(|p| p == current)
                .and_then(|i| peers.get(i + 1))
                .cloned(),
        };
        self.scroll = 0;
    }

    /// Scrolling keys. Returns false if the key is not for the log.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let max = self.filtered().len().saturating_sub(1);
        self.scroll = match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll + 1,
            KeyCode::Down | KeyCode::Char('j') => self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll + 10,
            KeyCode::PageDown => self.scroll.saturating_sub(10),
            KeyCode::Home => max,
            KeyCode::End => 0,
            _ => return false,
        }
        .min(max);
        true
    }
}

/// Name of a level as shown in the log.
pub fn level_name(level: Level) -> &'static str {
    match level {
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;

    fn ev(time_ms: u64, level: Level, peer: Option<&str>) -> Event {
        Event {
            time_ms,
            level,
            peer: peer.map(str::to_string),
            message: format!("event {time_ms}"),
        }
    }

    #[test]
    fn early_events_are_merged_with_the_history() {
        let mut log = LogState::default();
        log.resubscribe();
        log.push(ev(2, Level::Info, None));
        log.push(ev(3, Level::Info, None));
        assert!(log.events.is_empty());
        log.set_history(vec![ev(1, Level::Info, None), ev(2, Level::Info, None)]);
        let times: Vec<u64> = log.events.iter().map(|e| e.time_ms).collect();
        assert_eq!(times, [1, 2, 3]);
        log.push(ev(4, Level::Info, None));
        assert_eq!(log.events.len(), 4);
    }

    #[test]
    fn filters_by_level_and_peer() {
        let mut log = LogState::default();
        log.set_history(vec![
            ev(1, Level::Info, Some("a")),
            ev(2, Level::Warn, Some("b")),
            ev(3, Level::Error, None),
        ]);
        log.cycle_level();
        assert_eq!(log.filtered().len(), 2);
        log.cycle_level();
        assert_eq!(log.filtered().len(), 1);
        log.cycle_level();
        assert_eq!(log.filtered().len(), 3);
        log.cycle_peer(&["c".to_string()]);
        assert_eq!(log.peer.as_deref(), Some("a"));
        assert_eq!(log.filtered().len(), 1);
        log.cycle_peer(&[]);
        assert_eq!(log.peer.as_deref(), Some("b"));
        log.cycle_peer(&["c".to_string()]);
        assert_eq!(log.peer.as_deref(), Some("c"));
        log.cycle_peer(&["c".to_string()]);
        assert_eq!(log.peer, None);
    }

    #[test]
    fn scrolling_keeps_position_when_events_arrive() {
        let mut log = LogState::default();
        log.set_history((1..=5).map(|t| ev(t, Level::Info, None)).collect());
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert!(log.handle_key(&up));
        assert!(log.handle_key(&up));
        assert_eq!(log.scroll, 2);
        log.push(ev(6, Level::Info, None));
        assert_eq!(log.scroll, 3);
        log.handle_key(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(log.scroll, 0);
        log.handle_key(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(log.scroll, 5);
    }
}
