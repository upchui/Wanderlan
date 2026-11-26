//! Client tunnels: through NAT or direct, the endpoint of a client's tunnel,
//! the keepalive that keeps its NAT open and the test whether its server
//! reaches it directly.
//!
//! **Through NAT** (`nat`, where every client tunnel starts) a client opens
//! every flow towards its server: its probes, its sync and its tunnel
//! packets (VXLAN). The server answers where they come from. For the
//! tunnel, the client sends a keepalive frame every [`KEEPALIVE`] through
//! it, also when its LAN is quiet: that keeps the mapping of its NAT open
//! and tells the server (table `ip wanderlan`, see [`crate::sys::nft_ip`])
//! which address and port the NAT uses for it. Both ends send from the
//! tunnel port, so all packets of the tunnel are one UDP flow.
//!
//! **Direct** (`direct`): a client that its server reaches at the tunnel
//! port from any port gets its tunnel packets there, from a port per flow,
//! like the servers among each other; the receivers spread the flows over
//! their queues and cores. No keepalives, nothing learned.
//!
//! **The test.** Now and then the client asks for a test in one of its
//! authenticated probes ([`crate::mtu::wire2::FLAG_TEST_ME`]); its server
//! answers with a test frame ([`direct_test_packet`]) to the client's
//! address at the tunnel port, sent from another port than the tunnel
//! port: a packet that no mapping of a NAT and no connection of a firewall
//! lets in unless the client is reachable from outside. The frame carries a
//! tag of the pair's keys over the request, so only the server can send it
//! and only for this request. If it arrives, the client says so in its
//! probes ([`crate::mtu::wire2::FLAG_REACHED`]), the server runs the tunnel
//! direct and tells it in its answers ([`crate::mtu::wire2::FLAG_DIRECT`]);
//! the client follows. Both ends of a tunnel in different modes still work
//! while the test holds, so neither waits for the other.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use crate::identity::PairKeys;
use crate::mtu::wire2::Nonce;
use crate::sys::nft_ip::{KEEPALIVE_ETHERTYPE, Learned};

/// Interval of the keepalive frames (a NAT forgets an idle UDP flow after
/// 30 s or more).
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// Length of a keepalive frame (the shortest Ethernet frame).
pub const KEEPALIVE_LEN: usize = 60;
/// The source address of the keepalive frames: locally administered, never
/// learned anywhere (the server drops the untagged frame at the port).
pub const KEEPALIVE_MAC: [u8; 6] = [0x02, 0x77, 0x6c, 0x00, 0x00, 0x01];

/// Why no endpoint is used for a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatProblem {
    /// Nothing learned (yet).
    Nothing,
    /// The tunnel packets come from another address than the client's
    /// control traffic.
    OtherAddress(Ipv4Addr),
}

/// The endpoint to send a client's tunnel packets to: the most recently
/// refreshed entry, from the address its authenticated control traffic
/// comes from (if that is known). The port cannot be authenticated; the
/// address must match.
pub fn choose_endpoint(
    learned: &[Learned],
    control: Option<Ipv4Addr>,
) -> Result<SocketAddrV4, NatProblem> {
    let newest = learned
        .iter()
        .max_by_key(|l| l.expires)
        .ok_or(NatProblem::Nothing)?;
    let Some(control) = control else {
        return Ok(newest.addr);
    };
    learned
        .iter()
        .filter(|l| *l.addr.ip() == control)
        .max_by_key(|l| l.expires)
        .map(|l| l.addr)
        .ok_or(NatProblem::OtherAddress(*newest.addr.ip()))
}

/// The keepalive frame: untagged, ethertype `0x88b5`, `WLKA`, a version and
/// the fingerprint of the sending site (readable in tcpdump).
pub fn keepalive_frame(fingerprint: &str) -> [u8; KEEPALIVE_LEN] {
    let mut frame = [0u8; KEEPALIVE_LEN];
    frame[..6].copy_from_slice(&KEEPALIVE_MAC);
    frame[6..12].copy_from_slice(&KEEPALIVE_MAC);
    frame[12..14].copy_from_slice(&KEEPALIVE_ETHERTYPE.to_be_bytes());
    frame[14..18].copy_from_slice(b"WLKA");
    frame[18] = 1;
    for (slot, byte) in frame[19..27].iter_mut().zip(fingerprint.bytes()) {
        *slot = byte;
    }
    frame
}

/// Magic of a test frame, after the ethertype.
pub const DIRECT_MAGIC: &[u8; 4] = b"WLDT";
/// Label of the tag of a test frame.
const LABEL_DIRECT: &[u8] = b"wlp2 direct test";
/// The flags of a VXLAN header: the VNI is valid.
const VXLAN_FLAGS: u8 = 0x08;

/// The test frame for the request `seq` of a client that sent `nonce`:
/// like a keepalive (untagged, ethertype `0x88b5`, dropped at the tunnel
/// port), with `WLDT`, a version and the tag.
pub fn direct_test_frame(keys: &PairKeys, nonce: &Nonce, seq: u32) -> [u8; KEEPALIVE_LEN] {
    let mut frame = [0u8; KEEPALIVE_LEN];
    frame[..6].copy_from_slice(&KEEPALIVE_MAC);
    frame[6..12].copy_from_slice(&KEEPALIVE_MAC);
    frame[12..14].copy_from_slice(&KEEPALIVE_ETHERTYPE.to_be_bytes());
    frame[14..18].copy_from_slice(DIRECT_MAGIC);
    frame[18] = 1;
    frame[19..23].copy_from_slice(&seq.to_be_bytes());
    let tag = keys.probe_tag(&[LABEL_DIRECT, nonce, &seq.to_be_bytes()]);
    frame[23..23 + tag.len()].copy_from_slice(&tag);
    frame
}

/// The UDP payload of a test: the VXLAN header with `vni` and the frame.
pub fn direct_test_packet(vni: u32, keys: &PairKeys, nonce: &Nonce, seq: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(8 + KEEPALIVE_LEN);
    packet.extend_from_slice(&[VXLAN_FLAGS, 0, 0, 0]);
    packet.extend_from_slice(&(vni << 8).to_be_bytes());
    packet.extend_from_slice(&direct_test_frame(keys, nonce, seq));
    packet
}

/// True if `frame` (as it came out of the tunnel interface) is the test
/// frame for the request `seq` of this run (`nonce`), sent by the server.
pub fn verify_direct_frame(frame: &[u8], keys: &PairKeys, nonce: &Nonce, seq: u32) -> bool {
    let tag_len = crate::identity::TAG_LEN;
    frame.get(12..14) == Some(&KEEPALIVE_ETHERTYPE.to_be_bytes()[..])
        && frame.get(14..18) == Some(&DIRECT_MAGIC[..])
        && frame.get(18) == Some(&1)
        && frame.get(19..23) == Some(&seq.to_be_bytes()[..])
        && frame
            .get(23..23 + tag_len)
            .is_some_and(|tag| keys.probe_verify(&[LABEL_DIRECT, nonce, &seq.to_be_bytes()], tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_server_can_send_the_test_frame_for_a_request() {
        use crate::identity::Identity;
        let client = Identity::from_secret([1; 32]);
        let server = Identity::from_secret([2; 32]);
        let at_client = client.pair(&server.id()).unwrap();
        let at_server = server.pair(&client.id()).unwrap();
        let nonce = [7u8; crate::mtu::wire2::NONCE_LEN];
        let packet = direct_test_packet(at_server.vni(), &at_server, &nonce, 42);
        assert_eq!(packet[0], 0x08);
        assert_eq!(
            u32::from_be_bytes(packet[4..8].try_into().unwrap()) >> 8,
            at_client.vni()
        );
        let frame = &packet[8..];
        assert!(verify_direct_frame(frame, &at_client, &nonce, 42));
        // Another request, another run or another box: no.
        assert!(!verify_direct_frame(frame, &at_client, &nonce, 43));
        assert!(!verify_direct_frame(frame, &at_client, &[8; 16], 42));
        let other = Identity::from_secret([3; 32]).pair(&client.id()).unwrap();
        let forged = direct_test_frame(&other, &nonce, 42);
        assert!(!verify_direct_frame(&forged, &at_client, &nonce, 42));
        // A keepalive is no test frame.
        assert!(!verify_direct_frame(
            &keepalive_frame("abc"),
            &at_client,
            &nonce,
            42
        ));
    }

    fn learned(addr: &str, expires: u64) -> Learned {
        Learned {
            addr: addr.parse().unwrap(),
            expires,
        }
    }

    #[test]
    fn the_newest_entry_from_the_control_address_wins() {
        assert_eq!(choose_endpoint(&[], None), Err(NatProblem::Nothing));
        let entries = [
            learned("203.0.113.7:40001", 30),
            learned("203.0.113.7:40500", 88),
            learned("198.51.100.9:5000", 89),
        ];
        let control = "203.0.113.7".parse().ok();
        assert_eq!(
            choose_endpoint(&entries, control),
            Ok("203.0.113.7:40500".parse().unwrap())
        );
        // Without a known control address: the newest.
        assert_eq!(
            choose_endpoint(&entries, None),
            Ok("198.51.100.9:5000".parse().unwrap())
        );
        // Only another address: not used.
        assert_eq!(
            choose_endpoint(&entries[2..], control),
            Err(NatProblem::OtherAddress("198.51.100.9".parse().unwrap()))
        );
    }

    #[test]
    fn the_keepalive_is_an_untagged_frame_with_its_ethertype() {
        let frame = keepalive_frame("abcdefgh");
        assert_eq!(&frame[12..14], &[0x88, 0xb5]);
        assert_eq!(&frame[14..18], b"WLKA");
        assert_eq!(&frame[19..27], b"abcdefgh");
        // Bit 224 after the UDP header: 8 bytes UDP + 8 bytes VXLAN + 12
        // bytes MAC addresses = byte 12 of the frame.
        assert_eq!(
            u16::from_be_bytes([frame[12], frame[13]]),
            KEEPALIVE_ETHERTYPE
        );
    }
}
