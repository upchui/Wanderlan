//! Path MTU measurement and MSS calculation.
//!
//! The daemon measures the path MTU to every peer with UDP probes that have
//! the DF bit set, using a binary search over the probe size. The TCP MSS for
//! traffic through the tunnel is derived from it:
//!
//! `MSS = PMTU - 20 (outer IPv4) - 4 (GRE) - 18 (Ethernet + VLAN tag) - 40 (inner IPv4 + TCP)`

use std::future::Future;

/// Outer IPv4 header.
pub const OUTER_IP: u16 = 20;
/// GRE header (no key, no checksum).
pub const GRE: u16 = 4;
/// Inner Ethernet header plus one VLAN tag.
pub const ETHERNET_VLAN: u16 = 18;
/// Inner IPv4 and TCP headers.
pub const INNER_IP_TCP: u16 = 40;
/// Additional header size of IPv6 compared to IPv4.
pub const IPV6_EXTRA: u16 = 20;
/// Total overhead between path MTU and MSS.
pub const MSS_OVERHEAD: u16 = OUTER_IP + GRE + ETHERNET_VLAN + INNER_IP_TCP;

/// Smallest probe size (minimum IPv4 MTU).
pub const PROBE_MIN: u16 = 576;
/// Largest probe size worth testing: a full 1500-byte inner packet plus all
/// encapsulation headers. Paths at least this large never fragment.
pub const PROBE_MAX: u16 = 1500 + ETHERNET_VLAN + GRE + OUTER_IP;
/// Largest MSS that makes sense with the 1500-byte inner MTU.
pub const MSS_LIMIT: u16 = 1500 - INNER_IP_TCP;

/// IPv4 MSS for a path MTU.
pub fn mss_for_pmtu(pmtu: u16) -> u16 {
    pmtu.saturating_sub(MSS_OVERHEAD).min(MSS_LIMIT)
}

/// IPv6 MSS corresponding to an IPv4 MSS.
pub fn mss_ipv6(mss: u16) -> u16 {
    mss.saturating_sub(IPV6_EXTRA)
}

/// Finds the largest size in `lo..=hi` for which `probe` succeeds, assuming
/// that sizes below a working size also work. Returns `None` if even `lo`
/// fails. `hi` is tried first, so an unconstrained path needs one probe.
pub async fn search<F, Fut>(lo: u16, hi: u16, mut probe: F) -> Option<u16>
where
    F: FnMut(u16) -> Fut,
    Fut: Future<Output = bool>,
{
    if lo > hi {
        return None;
    }
    if probe(hi).await {
        return Some(hi);
    }
    if !probe(lo).await {
        return None;
    }
    // Invariant: lo works, hi does not.
    let (mut good, mut bad) = (lo, hi);
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if probe(mid).await {
            good = mid;
        } else {
            bad = mid;
        }
    }
    Some(good)
}

/// Wire format of the control probes (UDP on the site address).
pub mod wire {
    /// Magic bytes at the start of every probe.
    pub const MAGIC: &[u8; 4] = b"WLP1";
    /// Header size of a request (magic, kind, seq, revision, fingerprint).
    pub const REQUEST_HEADER: usize = 4 + 1 + 4 + 8 + 32;
    /// Size of a reply.
    pub const REPLY_LEN: usize = 4 + 1 + 4 + 2 + 8 + 32;
    /// IPv4 + UDP header bytes in front of the probe payload.
    pub const IP_UDP: usize = 28;

    const KIND_REQUEST: u8 = 1;
    const KIND_REPLY: u8 = 2;

    /// Configuration state announced with every probe (used by the sync).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Announce {
        /// Mesh revision of the sender.
        pub revision: u64,
        /// SHA-256 fingerprint of the sender's mesh.
        pub fingerprint: [u8; 32],
    }

    /// A decoded probe.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Probe {
        /// Echo request; `len` is the received payload length.
        Request {
            seq: u32,
            len: usize,
            announce: Announce,
        },
        /// Echo reply; `received` is the payload length the peer received.
        Reply {
            seq: u32,
            received: u16,
            announce: Announce,
        },
    }

    /// Encodes a request whose IP packet is `ip_size` bytes long.
    pub fn request(seq: u32, ip_size: u16, announce: &Announce) -> Vec<u8> {
        let len = usize::from(ip_size)
            .saturating_sub(IP_UDP)
            .max(REQUEST_HEADER);
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(MAGIC);
        buf.push(KIND_REQUEST);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&announce.revision.to_be_bytes());
        buf.extend_from_slice(&announce.fingerprint);
        buf.resize(len, 0);
        buf
    }

    /// Encodes a reply.
    pub fn reply(seq: u32, received: usize, announce: &Announce) -> Vec<u8> {
        let mut buf = Vec::with_capacity(REPLY_LEN);
        buf.extend_from_slice(MAGIC);
        buf.push(KIND_REPLY);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&u16::try_from(received).unwrap_or(u16::MAX).to_be_bytes());
        buf.extend_from_slice(&announce.revision.to_be_bytes());
        buf.extend_from_slice(&announce.fingerprint);
        buf
    }

    fn array<const N: usize>(buf: &[u8], at: usize) -> Option<[u8; N]> {
        buf.get(at..at + N)?.try_into().ok()
    }

    /// Decodes a probe; returns `None` for anything malformed.
    pub fn decode(buf: &[u8]) -> Option<Probe> {
        if buf.get(..4)? != MAGIC {
            return None;
        }
        let seq = u32::from_be_bytes(array(buf, 5)?);
        match *buf.get(4)? {
            KIND_REQUEST => Some(Probe::Request {
                seq,
                len: buf.len(),
                announce: Announce {
                    revision: u64::from_be_bytes(array(buf, 9)?),
                    fingerprint: array(buf, 17)?,
                },
            }),
            KIND_REPLY if buf.len() == REPLY_LEN => Some(Probe::Reply {
                seq,
                received: u16::from_be_bytes(array(buf, 9)?),
                announce: Announce {
                    revision: u64::from_be_bytes(array(buf, 11)?),
                    fingerprint: array(buf, 19)?,
                },
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::wire::{Announce, Probe};
    use super::*;

    #[test]
    fn mss_formula() {
        assert_eq!(MSS_OVERHEAD, 82);
        assert_eq!(mss_for_pmtu(1500), 1418);
        assert_eq!(mss_for_pmtu(1400), 1318);
        assert_eq!(mss_for_pmtu(PROBE_MAX), 1460);
        assert_eq!(mss_for_pmtu(9000), 1460);
        assert_eq!(mss_ipv6(1318), 1298);
        assert_eq!(PROBE_MAX, 1542);
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn search_finds_the_limit() {
        for limit in [576, 577, 1000, 1400, 1499, 1541, 1542] {
            let calls = Cell::new(0);
            let found = block_on(search(PROBE_MIN, PROBE_MAX, |size| {
                calls.set(calls.get() + 1);
                async move { size <= limit }
            }));
            assert_eq!(found, Some(limit));
            assert!(calls.get() <= 12, "{} probes for {limit}", calls.get());
        }
        assert_eq!(block_on(search(576, 1542, |_| async { false })), None);
        assert_eq!(block_on(search(1542, 1542, |_| async { true })), Some(1542));
    }

    #[test]
    fn wire_roundtrip() {
        let announce = Announce {
            revision: 7,
            fingerprint: [9; 32],
        };
        let req = wire::request(42, 1400, &announce);
        assert_eq!(req.len(), 1400 - 28);
        assert_eq!(
            wire::decode(&req),
            Some(Probe::Request {
                seq: 42,
                len: 1372,
                announce
            })
        );
        let small = wire::request(1, 10, &announce);
        assert_eq!(small.len(), wire::REQUEST_HEADER);
        let rep = wire::reply(42, 1372, &announce);
        assert_eq!(
            wire::decode(&rep),
            Some(Probe::Reply {
                seq: 42,
                received: 1372,
                announce
            })
        );
        assert_eq!(wire::decode(b"WLP1"), None);
        assert_eq!(wire::decode(b"XXXX\x01\0\0\0\0"), None);
        assert_eq!(wire::decode(&rep[..20]), None);
    }
}
