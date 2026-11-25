//! Path MTU measurement and MSS calculation.
//!
//! The daemon measures the path MTU to every peer with UDP probes that have
//! the DF bit set, using a binary search over the probe size. The TCP MSS for
//! traffic through the tunnel is derived from it:
//!
//! `MSS = PMTU - 20 (outer IPv4) - 8 (UDP) - 8 (VXLAN) - 18 (Ethernet + VLAN tag) - 40 (inner IPv4 + TCP)`

use std::future::Future;

/// Outer IPv4 header.
pub const OUTER_IP: u16 = 20;
/// UDP header of the tunnel packets.
pub const UDP: u16 = 8;
/// VXLAN header.
pub const VXLAN: u16 = 8;
/// Inner Ethernet header plus one VLAN tag.
pub const ETHERNET_VLAN: u16 = 18;
/// Inner IPv4 and TCP headers.
pub const INNER_IP_TCP: u16 = 40;
/// Additional header size of IPv6 compared to IPv4.
pub const IPV6_EXTRA: u16 = 20;
/// Total overhead between path MTU and MSS.
pub const MSS_OVERHEAD: u16 = OUTER_IP + UDP + VXLAN + ETHERNET_VLAN + INNER_IP_TCP;

/// Smallest probe size (minimum IPv4 MTU).
pub const PROBE_MIN: u16 = 576;
/// Largest probe size worth testing: a full 1500-byte inner packet plus all
/// encapsulation headers. Paths at least this large never fragment.
pub const PROBE_MAX: u16 = 1500 + ETHERNET_VLAN + VXLAN + UDP + OUTER_IP;
/// Largest MSS that makes sense with the 1500-byte inner MTU.
pub const MSS_LIMIT: u16 = 1500 - INNER_IP_TCP;

/// IPv4 MSS for a path MTU through a tunnel.
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
        /// Network revision of the sender.
        pub revision: u64,
        /// SHA-256 fingerprint of the sender's network.
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

/// Version 2 of the probes, between sites that know each other's identity:
/// every probe carries an HMAC tag with the probe key of the tunnel
/// ([`crate::identity::PairKeys`]). A request carries the sender's identity
/// and a nonce of its run; the tag of the reply covers that nonce and the
/// sequence number, so a valid reply proves that the peer holds its private
/// key now (no replayed answer). The padding of MTU probes is outside the
/// tags, so a large probe costs as much as a small one.
pub mod wire2 {
    use super::wire::{Announce, IP_UDP};
    use crate::identity::{KEY_LEN, PairKeys, SiteId, TAG_LEN};

    /// Magic bytes at the start of every probe of this version.
    pub const MAGIC: &[u8; 4] = b"WLP2";
    /// Bytes of the nonce of a request.
    pub const NONCE_LEN: usize = 16;
    /// The signed part of a request: magic, kind, flags, seq, revision,
    /// fingerprint, sender, nonce.
    const REQUEST_SIGNED: usize = 4 + 1 + 1 + 4 + 8 + 32 + KEY_LEN + NONCE_LEN;
    /// Header size of a request (the signed part and the tag).
    pub const REQUEST_HEADER: usize = REQUEST_SIGNED + TAG_LEN;
    /// The signed part of a reply: magic, kind, flags, seq, received,
    /// revision, fingerprint, responder.
    const REPLY_SIGNED: usize = 4 + 1 + 1 + 4 + 2 + 8 + 32 + KEY_LEN;
    /// Size of a reply.
    pub const REPLY_LEN: usize = REPLY_SIGNED + TAG_LEN;

    const KIND_REQUEST: u8 = 1;
    const KIND_REPLY: u8 = 2;
    const LABEL_REQUEST: &[u8] = b"wlp2 request";
    const LABEL_REPLY: &[u8] = b"wlp2 reply";

    /// The nonce of a request.
    pub type Nonce = [u8; NONCE_LEN];

    /// Flag of a reply: the request's tag was wrong for the responder (it
    /// answers anyway, so that the requester learns that another box sits
    /// at the address).
    pub const FLAG_UNVERIFIED: u8 = 1;

    /// A request, as decoded (its tag not checked yet).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Request {
        /// Sequence number.
        pub seq: u32,
        /// Received payload length.
        pub len: usize,
        /// Flags (none defined yet).
        pub flags: u8,
        /// The sender's configuration state.
        pub announce: Announce,
        /// The sender's identity.
        pub sender: SiteId,
        /// The sender's nonce.
        pub nonce: Nonce,
    }

    /// A reply, as decoded (its tag not checked yet).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Reply {
        /// Sequence number of the request.
        pub seq: u32,
        /// Flags ([`FLAG_UNVERIFIED`]).
        pub flags: u8,
        /// Payload length of the request as received.
        pub received: u16,
        /// The responder's configuration state.
        pub announce: Announce,
        /// The responder's identity.
        pub responder: SiteId,
    }

    /// A decoded probe of this version.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Probe {
        /// An echo request.
        Request(Request),
        /// An echo reply.
        Reply(Reply),
    }

    /// Encodes a request whose IP packet is `ip_size` bytes long.
    pub fn request(
        seq: u32,
        ip_size: u16,
        announce: &Announce,
        sender: &SiteId,
        nonce: &Nonce,
        keys: &PairKeys,
    ) -> Vec<u8> {
        let len = usize::from(ip_size)
            .saturating_sub(IP_UDP)
            .max(REQUEST_HEADER);
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(MAGIC);
        buf.push(KIND_REQUEST);
        buf.push(0);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&announce.revision.to_be_bytes());
        buf.extend_from_slice(&announce.fingerprint);
        buf.extend_from_slice(sender.as_bytes());
        buf.extend_from_slice(nonce);
        let tag = keys.probe_tag(&[LABEL_REQUEST, &buf]);
        buf.extend_from_slice(&tag);
        buf.resize(len, 0);
        buf
    }

    /// Encodes the reply to `request`.
    pub fn reply(
        request: &Request,
        flags: u8,
        announce: &Announce,
        responder: &SiteId,
        keys: &PairKeys,
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(REPLY_LEN);
        buf.extend_from_slice(MAGIC);
        buf.push(KIND_REPLY);
        buf.push(flags);
        buf.extend_from_slice(&request.seq.to_be_bytes());
        buf.extend_from_slice(&u16::try_from(request.len).unwrap_or(u16::MAX).to_be_bytes());
        buf.extend_from_slice(&announce.revision.to_be_bytes());
        buf.extend_from_slice(&announce.fingerprint);
        buf.extend_from_slice(responder.as_bytes());
        let tag = keys.probe_tag(&[LABEL_REPLY, &request.nonce, &buf]);
        buf.extend_from_slice(&tag);
        buf
    }

    fn array<const N: usize>(buf: &[u8], at: usize) -> Option<[u8; N]> {
        buf.get(at..at + N)?.try_into().ok()
    }

    fn announce(buf: &[u8], at: usize) -> Option<Announce> {
        Some(Announce {
            revision: u64::from_be_bytes(array(buf, at)?),
            fingerprint: array(buf, at + 8)?,
        })
    }

    /// Decodes a probe of this version; `None` for anything malformed. The
    /// tag is checked with [`verify_request`] or [`verify_reply`].
    pub fn decode(buf: &[u8]) -> Option<Probe> {
        if buf.get(..4)? != MAGIC {
            return None;
        }
        let flags = *buf.get(5)?;
        let seq = u32::from_be_bytes(array(buf, 6)?);
        match *buf.get(4)? {
            KIND_REQUEST if buf.len() >= REQUEST_HEADER => Some(Probe::Request(Request {
                seq,
                len: buf.len(),
                flags,
                announce: announce(buf, 10)?,
                sender: SiteId::from_bytes(array(buf, 50)?),
                nonce: array(buf, 50 + KEY_LEN)?,
            })),
            KIND_REPLY if buf.len() == REPLY_LEN => Some(Probe::Reply(Reply {
                seq,
                flags,
                received: u16::from_be_bytes(array(buf, 10)?),
                announce: announce(buf, 12)?,
                responder: SiteId::from_bytes(array(buf, 52)?),
            })),
            _ => None,
        }
    }

    /// True if the request in `buf` carries a valid tag of `keys`.
    pub fn verify_request(buf: &[u8], keys: &PairKeys) -> bool {
        match (
            buf.get(..REQUEST_SIGNED),
            buf.get(REQUEST_SIGNED..REQUEST_HEADER),
        ) {
            (Some(signed), Some(tag)) => keys.probe_verify(&[LABEL_REQUEST, signed], tag),
            _ => false,
        }
    }

    /// True if the reply in `buf` answers the request with `nonce` and
    /// carries a valid tag of `keys`.
    pub fn verify_reply(buf: &[u8], nonce: &Nonce, keys: &PairKeys) -> bool {
        match (buf.get(..REPLY_SIGNED), buf.get(REPLY_SIGNED..REPLY_LEN)) {
            (Some(signed), Some(tag)) => keys.probe_verify(&[LABEL_REPLY, nonce, signed], tag),
            _ => false,
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
        assert_eq!(MSS_OVERHEAD, 94);
        assert_eq!(mss_for_pmtu(1500), 1406);
        assert_eq!(mss_for_pmtu(1400), 1306);
        assert_eq!(mss_ipv6(1306), 1286);
        assert_eq!(PROBE_MAX, 1554);
        // Full-size inner frames fit through a path this large.
        assert_eq!(mss_for_pmtu(PROBE_MAX), MSS_LIMIT);
        assert_eq!(mss_for_pmtu(9000), MSS_LIMIT);
    }

    #[test]
    fn wire2_roundtrip_and_tags() {
        use crate::identity::Identity;
        let a = Identity::from_secret([1; 32]);
        let b = Identity::from_secret([2; 32]);
        let ab = a.pair(&b.id()).unwrap();
        let ba = b.pair(&a.id()).unwrap();
        let announce = Announce {
            revision: 7,
            fingerprint: [9; 32],
        };
        let nonce = [5u8; wire2::NONCE_LEN];
        let req = wire2::request(42, 1400, &announce, &a.id(), &nonce, &ab);
        assert_eq!(req.len(), 1400 - 28);
        let wire2::Probe::Request(decoded) = wire2::decode(&req).unwrap() else {
            panic!("a request");
        };
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.len, 1372);
        assert_eq!(decoded.sender, a.id());
        assert_eq!(decoded.nonce, nonce);
        assert_eq!(decoded.announce, announce);
        assert!(wire2::verify_request(&req, &ba));
        // The padding is not signed; everything else is.
        let mut padded = req.clone();
        padded[1000] = 0xff;
        assert!(wire2::verify_request(&padded, &ba));
        let mut forged = req.clone();
        forged[12] ^= 1;
        assert!(!wire2::verify_request(&forged, &ba));
        let small = wire2::request(1, 10, &announce, &a.id(), &nonce, &ab);
        assert_eq!(small.len(), wire2::REQUEST_HEADER);
        assert_eq!(wire2::REQUEST_HEADER, 114);

        let rep = wire2::reply(&decoded, 0, &announce, &b.id(), &ba);
        assert_eq!(rep.len(), wire2::REPLY_LEN);
        assert_eq!(wire2::REPLY_LEN, 100);
        let wire2::Probe::Reply(r) = wire2::decode(&rep).unwrap() else {
            panic!("a reply");
        };
        assert_eq!((r.seq, r.received, r.responder), (42, 1372, b.id()));
        assert!(wire2::verify_reply(&rep, &nonce, &ab));
        // An answer to another request (a replay) does not count.
        assert!(!wire2::verify_reply(&rep, &[6; wire2::NONCE_LEN], &ab));
        // A box with another key cannot answer.
        let c = Identity::from_secret([3; 32]);
        let ca = c.pair(&a.id()).unwrap();
        let fake = wire2::reply(&decoded, 0, &announce, &b.id(), &ca);
        assert!(!wire2::verify_reply(&fake, &nonce, &ab));
        // Version 1 does not read version 2 and the other way round.
        assert_eq!(wire::decode(&req), None);
        assert_eq!(wire2::decode(&wire::reply(1, 2, &announce)), None);
        assert_eq!(wire2::decode(&rep[..50]), None);
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
