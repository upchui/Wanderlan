//! Loop detection from bridge FDB updates.
//!
//! Split horizon makes the mesh itself loop-free, but a loop can still form
//! outside wanderlan's control (e.g. two sites bridged together by another
//! path). The symptom is a MAC address that keeps moving between tunnel
//! ports. [`LoopGuard`] counts such moves per MAC/VLAN within a time window
//! and reports a loop once a threshold is reached.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Default observation window.
pub const WINDOW: Duration = Duration::from_secs(10);
/// Default number of moves within the window that counts as a loop.
pub const THRESHOLD: usize = 5;
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

/// Detects MAC flapping between tunnel ports.
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
    /// tells whether a port is a GRETAP tunnel; only moves between two
    /// tunnel ports count. Returns a detection when the threshold is hit.
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
        if !(is_tunnel(from) && is_tunnel(port)) {
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

/// Chooses the port to shut down for a detected loop: among the tunnel
/// ports involved, prefer ports that are not isolated (spoke ports on a hub),
/// then the port the address moved to most recently.
pub fn choose_port(detected: &LoopDetected, isolated: impl Fn(u32) -> bool) -> Option<u32> {
    let mut candidates: Vec<(bool, usize, u32)> = Vec::new();
    for (i, m) in detected.moves.iter().enumerate() {
        for port in [m.from, m.to] {
            let recency = if port == m.to { 2 * i + 1 } else { 2 * i };
            match candidates.iter_mut().find(|c| c.2 == port) {
                Some(c) => c.1 = c.1.max(recency),
                None => candidates.push((!isolated(port), recency, port)),
            }
        }
    }
    candidates.into_iter().max().map(|c| c.2)
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
    fn moves_to_the_trunk_do_not_count() {
        let mut g = LoopGuard::new(Duration::from_secs(10), 2);
        let t = Instant::now();
        for i in 0..10u64 {
            let port = if i % 2 == 0 { GT_A } else { TRUNK };
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
    fn prefers_non_isolated_and_most_recent_port() {
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
        // Both isolated: the port moved to last.
        assert_eq!(choose_port(&det, |_| true), Some(GT_A));
        // GT_B is a spoke port (not isolated): preferred.
        assert_eq!(choose_port(&det, |p| p == GT_A), Some(GT_B));
    }
}
