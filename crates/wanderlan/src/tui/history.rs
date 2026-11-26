//! The traffic and round trips of the last hour, as the daemon recorded
//! them ([`Request::History`](wanderlan_core::api::Request::History)):
//! fetched once, then only what is new.

use std::collections::{BTreeMap, VecDeque};

use wanderlan_core::api::{History, RttPoint, TrafficPoint};

/// How far back the graphs go (as far as the daemon records).
pub const KEEP_MS: u64 = 3_600_000;

/// Now on the wall clock, in ms since the Unix epoch.
pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The time range of the graphs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Range {
    /// The last 2 minutes.
    Short,
    /// The last 10 minutes.
    #[default]
    Medium,
    /// The last hour.
    Long,
}

impl Range {
    /// Its length in ms.
    pub fn ms(self) -> u64 {
        match self {
            Self::Short => 120_000,
            Self::Medium => 600_000,
            Self::Long => KEEP_MS,
        }
    }

    /// The next one (the key z).
    pub fn next(self) -> Self {
        match self {
            Self::Short => Self::Medium,
            Self::Medium => Self::Long,
            Self::Long => Self::Short,
        }
    }

    /// For titles: `2 min`, `10 min`, `1 h`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Short => "2 min",
            Self::Medium => "10 min",
            Self::Long => "1 h",
        }
    }

    /// The labels of the time axis, oldest first.
    pub fn ticks(self) -> [&'static str; 3] {
        match self {
            Self::Short => ["-2m", "-1m", "now"],
            Self::Medium => ["-10m", "-5m", "now"],
            Self::Long => ["-1h", "-30m", "now"],
        }
    }
}

/// What is recorded of one peer.
#[derive(Debug, Clone, Default)]
pub struct PeerSeries {
    /// The traffic of its tunnel.
    pub traffic: VecDeque<TrafficPoint>,
    /// Its probes.
    pub rtt: VecDeque<RttPoint>,
}

/// The history as far as fetched.
#[derive(Debug, Clone, Default)]
pub struct HistoryStore {
    /// When the daemon started that recorded it.
    started_ms: Option<u64>,
    /// The daemon's clock at the last answer: the next one asks after it.
    pub now_ms: u64,
    /// How often the daemon samples the traffic.
    pub interval_ms: u64,
    /// Per direct peer.
    pub peers: BTreeMap<String, PeerSeries>,
    /// Per trunk port.
    pub trunks: BTreeMap<String, VecDeque<TrafficPoint>>,
}

impl HistoryStore {
    /// What to ask for next: what came after the last answer.
    pub fn since(&self) -> Option<u64> {
        self.started_ms.map(|_| self.now_ms)
    }

    /// Takes over an answer: new points are added (a daemon that started
    /// again brings a history of its own), peers and ports that are gone
    /// are dropped, nothing older than an hour is kept.
    pub fn merge(&mut self, answer: History) {
        if self.started_ms != Some(answer.started_ms) {
            *self = Self {
                started_ms: Some(answer.started_ms),
                ..Self::default()
            };
        }
        self.now_ms = answer.now_ms;
        self.interval_ms = answer.interval_ms;
        let oldest = answer.now_ms.saturating_sub(KEEP_MS);
        let names: Vec<&str> = answer.peers.iter().map(|p| p.name.as_str()).collect();
        self.peers.retain(|name, _| names.contains(&name.as_str()));
        for peer in answer.peers {
            let series = self.peers.entry(peer.name).or_default();
            append(&mut series.traffic, peer.traffic, TrafficPoint::at, oldest);
            append(&mut series.rtt, peer.rtt, |p| p.0, oldest);
        }
        let ports: Vec<&str> = answer.trunks.iter().map(|p| p.port.as_str()).collect();
        self.trunks.retain(|port, _| ports.contains(&port.as_str()));
        for port in answer.trunks {
            let series = self.trunks.entry(port.port).or_default();
            append(series, port.traffic, TrafficPoint::at, oldest);
        }
    }

    /// The traffic of all tunnels together, per sample.
    pub fn tunnels_total(&self) -> Vec<TrafficPoint> {
        let mut sum: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
        for series in self.peers.values() {
            for p in &series.traffic {
                let e = sum.entry(p.0).or_default();
                e.0 = e.0.saturating_add(p.1);
                e.1 = e.1.saturating_add(p.2);
            }
        }
        sum.into_iter()
            .map(|(at, (rx, tx))| TrafficPoint(at, rx, tx))
            .collect()
    }
}

/// Appends the points newer than the last one there, drops those before
/// `oldest`.
fn append<T: Copy>(to: &mut VecDeque<T>, points: Vec<T>, at: impl Fn(&T) -> u64, oldest: u64) {
    let last = to.back().map(&at);
    to.extend(
        points
            .into_iter()
            .filter(|p| last.is_none_or(|l| at(p) > l)),
    );
    while to.front().is_some_and(|p| at(p) < oldest) {
        to.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use wanderlan_core::api::PeerHistory;

    use super::*;

    fn answer(started: u64, now: u64, points: &[(u64, u64)]) -> History {
        History {
            now_ms: now,
            started_ms: started,
            interval_ms: 2000,
            peers: vec![PeerHistory {
                name: "buero".into(),
                traffic: points
                    .iter()
                    .map(|&(at, v)| TrafficPoint(at, v, v))
                    .collect(),
                rtt: Vec::new(),
            }],
            trunks: Vec::new(),
        }
    }

    #[test]
    fn new_points_are_added_and_a_new_daemon_starts_over() {
        let mut store = HistoryStore::default();
        assert_eq!(store.since(), None, "the first time: everything");
        store.merge(answer(1, 10_000, &[(6_000, 1), (8_000, 2)]));
        assert_eq!(store.since(), Some(10_000));
        // Again with one old and one new point.
        store.merge(answer(1, 12_000, &[(8_000, 2), (12_000, 3)]));
        assert_eq!(store.peers["buero"].traffic.len(), 3);
        assert_eq!(
            store.tunnels_total().last(),
            Some(&TrafficPoint(12_000, 3, 3))
        );
        // The daemon started again: its own history.
        store.merge(answer(2, 3_000, &[(2_000, 9)]));
        assert_eq!(store.peers["buero"].traffic.len(), 1);
        // A peer that is gone is dropped.
        let mut gone = answer(2, 5_000, &[]);
        gone.peers.clear();
        store.merge(gone);
        assert!(store.peers.is_empty());
    }
}
