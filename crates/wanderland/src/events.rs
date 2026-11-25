//! Event log: every noteworthy event is logged with tracing and kept in a
//! ring buffer that the TUI can subscribe to.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;
use tracing::{error, info, warn};
use wanderlan_core::api::{Event, Level};

/// Number of events kept for new subscribers.
const CAPACITY: usize = 1000;

/// The event log.
#[derive(Debug)]
pub struct EventLog {
    history: Mutex<VecDeque<Event>>,
    tx: broadcast::Sender<Event>,
}

impl EventLog {
    /// Creates an empty log.
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            history: Mutex::new(VecDeque::with_capacity(CAPACITY)),
            tx,
        })
    }

    /// Records an event.
    pub fn emit(&self, level: Level, peer: Option<&str>, message: impl Into<String>) {
        let message = message.into();
        match level {
            Level::Info => info!("{message}"),
            Level::Warn => warn!("{message}"),
            Level::Error => error!("{message}"),
        }
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let event = Event {
            time_ms,
            level,
            peer: peer.map(str::to_string),
            message,
        };
        // Sent while the history is locked: a subscriber gets an event either
        // in the history or from the channel, never both.
        let mut history = self.history.lock();
        if let Ok(h) = &mut history {
            if h.len() == CAPACITY {
                h.pop_front();
            }
            h.push_back(event.clone());
        }
        let _ = self.tx.send(event);
        drop(history);
    }

    /// Informational event.
    pub fn info(&self, message: impl Into<String>) {
        self.emit(Level::Info, None, message);
    }

    /// Warning.
    pub fn warn(&self, message: impl Into<String>) {
        self.emit(Level::Warn, None, message);
    }

    /// Error.
    pub fn error(&self, message: impl Into<String>) {
        self.emit(Level::Error, None, message);
    }

    /// The history and a receiver for new events, without gaps in between.
    pub fn subscribe(&self) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let guard = self.history.lock();
        let rx = self.tx.subscribe();
        let history = guard
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default();
        (history, rx)
    }
}
