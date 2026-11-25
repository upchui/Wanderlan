//! Loop detection from bridge FDB updates.
//!
//! Split horizon makes the network itself loop-free, but a loop can still
//! form outside wanderlan's control, e.g. when the LANs of two sites are also
//! connected by another path. The symptom is a MAC address that keeps moving
//! between the trunk and a tunnel, or between two tunnels. [`LoopGuard`]
//! counts such moves per MAC/VLAN within a time window and reports a loop
//! once a threshold is reached.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Default observation window.
pub const WINDOW: Duration = Duration::from_secs(10);
/// Default number of moves within the window that counts as a loop. A real
/// loop moves an address hundreds of times a second; a device roaming
/// between sites or a duplicated MAC address moves it a few times.
pub const THRESHOLD: usize = 20;
/// After a detection, further detections are suppressed for this long so
/// that the remaining frames of the loop do not shut down more ports.
pub const HOLD_OFF: Duration = Duration::from_secs(30);

/// One observed move of a MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    /// When it happened.
    pub at: Instant,
    /// Port index the address was learned on before.
    pub from: u32,
    /// Port index it is learned on now.
    pub to: u32,
}

/// A detected loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopDetected {
    /// The flapping MAC address.
    pub mac: [u8; 6],
    /// Its VLAN.
    pub vlan: Option<u16>,
    /// The moves within the window, oldest first.
    pub moves: Vec<Move>,
}

#[derive(Debug, Default)]
struct Track {
    port: u32,
    moves: VecDeque<Move>,
}

/// Detects MAC flapping that involves tunnel ports.
#[derive(Debug)]
pub struct LoopGuard {
    window: Duration,
    threshold: usize,
    macs: HashMap<([u8; 6], Option<u16>), Track>,
    quiet_until: Option<Instant>,
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new(WINDOW, THRESHOLD)
    }
}

impl LoopGuard {
    /// Creates a guard with the given window and threshold.
    pub fn new(window: Duration, threshold: usize) -> Self {
        Self {
            window,
            threshold: threshold.max(1),
            macs: HashMap::new(),
            quiet_until: None,
        }
    }

    /// Records that `mac` in `vlan` is now learned on `port`. `is_tunnel`
    /// tells whether a port is a GRETAP tunnel; moves count if at least one
    /// of the two ports is a tunnel (trunk to tunnel, tunnel to tunnel).
    /// Returns a detection when the threshold is hit.
    pub fn learned(
        &mut self,
        now: Instant,
        mac: [u8; 6],
        vlan: Option<u16>,
        port: u32,
        is_tunnel: impl Fn(u32) -> bool,
    ) -> Option<LoopDetected> {
        let window = self.window;
        let track = self.macs.entry((mac, vlan)).or_insert(Track {
            port,
            moves: VecDeque::new(),
        });
        while track
            .moves
            .front()
            .is_some_and(|m| now.duration_since(m.at) > window)
        {
            track.moves.pop_front();
        }
        if track.port == port {
            return None;
        }
        let from = track.port;
        track.port = port;
        if !(is_tunnel(from) || is_tunnel(port)) {
            return None;
        }
        track.moves.push_back(Move {
            at: now,
            from,
            to: port,
        });
        if track.moves.len() < self.threshold {
            return None;
        }
        if self.quiet_until.is_some_and(|q| now < q) {
            // Hold-off: keep only what a detection needs, not every move of
            // a storm.
            while track.moves.len() > self.threshold {
                track.moves.pop_front();
            }
            return None;
        }
        let moves: Vec<Move> = track.moves.drain(..).collect();
        self.quiet_until = Some(now + HOLD_OFF);
        Some(LoopDetected { mac, vlan, moves })
    }

    /// Forgets an address (it was deleted from the FDB).
    pub fn forget(&mut self, mac: [u8; 6], vlan: Option<u16>) {
        self.macs.remove(&(mac, vlan));
    }

    /// Forgets everything (e.g. after the FDB was flushed).
    pub fn reset(&mut self) {
        self.macs.clear();
    }
}

/// Chooses the port to shut down for a detected loop: the tunnel port
/// involved that the address moved to most recently. The trunk is never
/// chosen; cutting the tunnel ends a loop between two sites' LANs.
pub fn choose_port(detected: &LoopDetected, is_tunnel: impl Fn(u32) -> bool) -> Option<u32> {
    let mut candidates: Vec<(usize, u32)> = Vec::new();
    for (i, m) in detected.moves.iter().enumerate() {
        for port in [m.from, m.to].into_iter().filter(|p| is_tunnel(*p)) {
            let recency = if port == m.to { 2 * i + 1 } else { 2 * i };
            match candidates.iter_mut().find(|c| c.1 == port) {
                Some(c) => c.0 = c.0.max(recency),
                None => candidates.push((recency, port)),
            }
        }
    }
    candidates.into_iter().max().map(|c| c.1)
}

/// Formats a MAC address.
pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const TRUNK: u32 = 1;
    const GT_A: u32 = 10;
    const GT_B: u32 = 11;

    fn tunnel(p: u32) -> bool {
        p >= 10
    }

    #[test]
    fn a_storm_during_the_hold_off_keeps_only_what_is_needed() {
        let mut g = LoopGuard::new(Duration::from_secs(10), 5);
        let t = Instant::now();
        let mut detections = 0;
        for i in 0..1000u32 {
            let port = if i % 2 == 0 { GT_A } else { GT_B };
            if g.learned(t, MAC, Some(20), port, tunnel).is_some() {
                detections += 1;
            }
        }
        assert_eq!(detections, 1, "once, then the hold-off");
        let kept = g
            .macs
            .values()
            .map(|track| track.moves.len())
            .max()
            .unwrap();
        assert!(kept <= 5, "{kept} moves kept");
    }

    #[test]
    fn the_default_threshold_ignores_a_few_moves() {
        let mut g = LoopGuard::default();
        let t = Instant::now();
        // A device that changes sites a few times (roaming, a duplicated MAC).
        for i in 0..10u32 {
            let port = if i % 2 == 0 { GT_A } else { GT_B };
            assert!(g.learned(t, MAC, Some(20), port, tunnel).is_none());
        }
    }

    #[test]
    fn single_move_is_not_a_loop() {
        let mut g = LoopGuard::default();
        let t = Instant::now();
        assert!(g.learned(t, MAC, Some(20), GT_A, tunnel).is_none());
        assert!(g.learned(t, MAC, Some(20), GT_B, tunnel).is_none());
        assert!(g.learned(t, MAC, Some(20), GT_B, tunnel).is_none());
    }

    #[test]
    fn flapping_between_tunnels_is_detected_once() {
        let mut g = LoopGuard::new(Duration::from_secs(10), 5);
        let t = Instant::now();
        let mut found = None;
        for i in 0..6u64 {
            let port = if i % 2 == 0 { GT_A } else { GT_B };
            let r = g.learned(
                t + Duration::from_millis(i * 10),
                MAC,
                Some(20),
                port,
                tunnel,
            );
            if r.is_some() {
                found = r;
            }
        }
        let found = found.expect("loop detected");
        assert_eq!(found.moves.len(), 5);
        assert_eq!(format_mac(&found.mac), "02:00:00:00:00:01");
        // Hold-off: more flapping right afterwards is not reported again.
        for i in 0..10u64 {
            let port = if i % 2 == 0 { GT_A } else { GT_B };
            assert!(
                g.learned(
                    t + Duration::from_millis(100 + i),
                    MAC,
                    Some(20),
                    port,
                    tunnel
                )
                .is_none()
            );
        }
    }

    #[test]
    fn flapping_between_trunk_and_tunnel_is_detected() {
        // The LANs of two sites are connected by another path: a local MAC
        // comes back through the tunnel again and again.
        let mut g = LoopGuard::new(Duration::from_secs(10), 3);
        let t = Instant::now();
        let mut found = None;
        for i in 0..4u64 {
            let port = if i % 2 == 0 { TRUNK } else { GT_A };
            found = found.or(g.learned(t, MAC, None, port, tunnel));
        }
        let found = found.expect("loop detected");
        assert_eq!(choose_port(&found, tunnel), Some(GT_A));
    }

    #[test]
    fn moves_between_non_tunnel_ports_do_not_count() {
        let mut g = LoopGuard::new(Duration::from_secs(10), 2);
        let t = Instant::now();
        for i in 0..10u64 {
            let port = if i % 2 == 0 { TRUNK } else { 2 };
            assert!(g.learned(t, MAC, None, port, tunnel).is_none());
        }
    }

    #[test]
    fn old_moves_expire() {
        let mut g = LoopGuard::new(Duration::from_secs(1), 3);
        let t = Instant::now();
        g.learned(t, MAC, None, GT_A, tunnel);
        g.learned(t, MAC, None, GT_B, tunnel);
        g.learned(t, MAC, None, GT_A, tunnel);
        let later = t + Duration::from_secs(5);
        assert!(g.learned(later, MAC, None, GT_B, tunnel).is_none());
    }

    #[test]
    fn chooses_the_most_recent_tunnel_never_the_trunk() {
        let t = Instant::now();
        let det = LoopDetected {
            mac: MAC,
            vlan: None,
            moves: vec![
                Move {
                    at: t,
                    from: GT_A,
                    to: GT_B,
                },
                Move {
                    at: t,
                    from: GT_B,
                    to: GT_A,
                },
            ],
        };
        // The port the address moved to last.
        assert_eq!(choose_port(&det, tunnel), Some(GT_A));
        // Only GT_B is a tunnel: GT_B, even though GT_A is more recent.
        assert_eq!(choose_port(&det, |p| p == GT_B), Some(GT_B));
        let trunk_only = LoopDetected {
            mac: MAC,
            vlan: None,
            moves: vec![Move {
                at: t,
                from: TRUNK,
                to: TRUNK,
            }],
        };
        assert_eq!(choose_port(&trunk_only, tunnel), None);
    }
}
