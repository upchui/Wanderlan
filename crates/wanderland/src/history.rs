//! The traffic of the tunnels and trunk ports over the last hour, recorded
//! with every sample of the rates (every 2 s), for the graphs of the
//! terminal UI: they are full as soon as it starts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wanderlan_core::api::TrafficPoint;

/// How far back the history goes.
pub const KEEP: Duration = Duration::from_secs(3600);

/// The history, shared between the sampler and the API.
pub type SharedHistory = Arc<RwLock<History>>;

/// The recorded traffic per interface.
#[derive(Debug, Default)]
pub struct History {
    /// The interfaces to record (tunnels and trunk ports), set by the
    /// controller; the others are dropped.
    wanted: BTreeSet<String>,
    series: BTreeMap<String, VecDeque<TrafficPoint>>,
}

impl History {
    /// Records only these interfaces from now on.
    pub fn want(&mut self, names: BTreeSet<String>) {
        self.series.retain(|name, _| names.contains(name));
        self.wanted = names;
    }

    /// Adds one sample of the rates (bit/s per interface) taken at `at_ms`;
    /// what is older than [`KEEP`] goes.
    pub fn record(&mut self, at_ms: u64, rates: &BTreeMap<String, (u64, u64)>) {
        let oldest = at_ms.saturating_sub(duration_ms(KEEP));
        for name in &self.wanted {
            if let Some(&(rx, tx)) = rates.get(name) {
                self.series
                    .entry(name.clone())
                    .or_default()
                    .push_back(TrafficPoint(at_ms, rx, tx));
            }
        }
        for points in self.series.values_mut() {
            while points.front().is_some_and(|p| p.at() < oldest) {
                points.pop_front();
            }
        }
        self.series.retain(|_, points| !points.is_empty());
    }

    /// The points of `name` after `since_ms`, oldest first.
    pub fn since(&self, name: &str, since_ms: u64) -> Vec<TrafficPoint> {
        self.series.get(name).map_or_else(Vec::new, |points| {
            let start = points.partition_point(|p| p.at() <= since_ms);
            points.range(start..).copied().collect()
        })
    }

    /// A renamed tunnel keeps its history.
    pub fn rename(&mut self, old: &str, new: &str) {
        if let Some(points) = self.series.remove(old) {
            self.series.insert(new.to_string(), points);
        }
        if self.wanted.remove(old) {
            self.wanted.insert(new.to_string());
        }
    }
}

/// Now, in ms since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, duration_ms)
}

/// A duration in ms.
pub fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_hour_of_the_wanted_interfaces() {
        let mut h = History::default();
        h.want(["vx-a".to_string()].into());
        let rates: BTreeMap<String, (u64, u64)> =
            [("vx-a".to_string(), (10, 20)), ("eth9".to_string(), (1, 1))].into();
        let hour = duration_ms(KEEP);
        h.record(1_000, &rates);
        h.record(3_000, &rates);
        assert_eq!(h.since("vx-a", 0).len(), 2);
        assert!(h.since("eth9", 0).is_empty(), "not wanted");
        assert_eq!(h.since("vx-a", 1_000), [TrafficPoint(3_000, 10, 20)]);
        // An hour later the first one is gone.
        h.record(1_000 + hour + 1, &rates);
        assert_eq!(
            h.since("vx-a", 0).first().map(TrafficPoint::at),
            Some(3_000)
        );
        h.rename("vx-a", "vx-b");
        assert_eq!(h.since("vx-b", 0).len(), 2);
        h.want(BTreeSet::new());
        assert!(h.since("vx-b", 0).is_empty());
    }
}
