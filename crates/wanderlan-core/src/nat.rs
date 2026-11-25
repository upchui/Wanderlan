//! Clients behind NAT: the endpoint of a client's tunnel and the keepalive
//! that keeps its NAT open.
//!
//! A client opens every flow towards its server: its probes, its sync and
//! its tunnel packets (GRE inside UDP). The server answers where they come
//! from. For the tunnel, the client sends a keepalive frame every
//! [`KEEPALIVE`] through it, also when its LAN is quiet: that keeps the
//! mapping of its NAT open and tells the server (table `ip wanderlan`, see
//! [`crate::sys::nft_ip`]) which address and port the NAT uses for it.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Bit 224 after the UDP header: 8 bytes UDP + 8 bytes GRE + 12
        // bytes MAC addresses = byte 12 of the frame.
        assert_eq!(
            u16::from_be_bytes([frame[12], frame[13]]),
            KEEPALIVE_ETHERTYPE
        );
    }
}
