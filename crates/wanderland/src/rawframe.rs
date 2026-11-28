//! Ethernet frames through an interface (AF_PACKET): sending the keepalives
//! of the client tunnels and the marker of the switch check, receiving the
//! test frame of the server, the marker and DHCP (while a trunk is in one
//! switch network with a peer).
//!
//! The sending socket has protocol 0: the kernel never copies a received
//! frame to it. The receiving one ([`Tap`]) is open only for a test (a few
//! seconds), with a filter in the kernel and none of the sent frames. Both
//! need `CAP_NET_RAW`.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::time::{Duration, Instant};

fn invalid<E: std::error::Error + Send + Sync + 'static>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e)
}

/// The index of the interface `ifname`.
#[allow(unsafe_code)]
fn index_of(ifname: &str) -> io::Result<libc::c_int> {
    let name = CString::new(ifname).map_err(invalid)?;
    // SAFETY: `name` is a valid NUL-terminated string for the duration of
    // the call.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(io::Error::last_os_error());
    }
    libc::c_int::try_from(index).map_err(invalid)
}

/// A packet socket of protocol 0 (it receives nothing yet).
#[allow(unsafe_code)]
fn packet_socket() -> io::Result<OwnedFd> {
    // SAFETY: a plain socket(2) call; the result is checked before use.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a socket just opened and owned by nothing else; the
    // OwnedFd closes it when dropped.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The link-layer address of interface `index` for protocol `protocol`.
#[allow(unsafe_code)]
fn link_addr(index: libc::c_int, protocol: u16) -> libc::sockaddr_ll {
    // SAFETY: sockaddr_ll is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as libc::c_ushort;
    addr.sll_protocol = protocol.to_be();
    addr.sll_ifindex = index;
    addr
}

fn addr_len() -> io::Result<libc::socklen_t> {
    libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_ll>()).map_err(invalid)
}

/// Sends `frame` (a complete Ethernet frame) out of the interface `ifname`.
#[allow(unsafe_code)]
pub fn send(ifname: &str, frame: &[u8]) -> io::Result<()> {
    let index = index_of(ifname)?;
    let socket = packet_socket()?;
    let mut addr = link_addr(index, 0);
    addr.sll_halen = 6;
    addr.sll_addr[..6].copy_from_slice(frame.get(..6).unwrap_or(&[0; 6]));
    let len = addr_len()?;
    // SAFETY: the buffer and the address live for the duration of the call;
    // the lengths are their exact sizes.
    let sent = unsafe {
        libc::sendto(
            socket.as_raw_fd(),
            frame.as_ptr().cast::<libc::c_void>(),
            frame.len(),
            0,
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    };
    if sent < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `setsockopt(2)` option of a packet socket: no copies of the frames this
/// box sends (`linux/if_packet.h`).
const PACKET_IGNORE_OUTGOING: libc::c_int = 23;
/// `getsockopt(2)` option of a packet socket: the frames its filter took
/// since the last call, including those it had no room for.
const PACKET_STATISTICS: libc::c_int = 6;

/// `struct tpacket_stats` (`linux/if_packet.h`).
#[repr(C)]
#[derive(Default)]
struct TpacketStats {
    packets: u32,
    drops: u32,
}

/// Sends many frames out of one interface (a speed test): one socket for
/// all of them.
pub struct Sender {
    socket: OwnedFd,
    addr: libc::sockaddr_ll,
}

impl Sender {
    /// A sender for interface `ifname`; the frames go to `dst`.
    pub fn open(ifname: &str, dst: [u8; 6]) -> io::Result<Self> {
        let index = index_of(ifname)?;
        let socket = packet_socket()?;
        let mut addr = link_addr(index, 0);
        addr.sll_halen = 6;
        addr.sll_addr[..6].copy_from_slice(&dst);
        Ok(Self { socket, addr })
    }

    /// Sends `frame` up to `count` times with one call: how many went.
    #[allow(unsafe_code)]
    pub fn send_many(&self, frame: &[u8], count: usize) -> io::Result<usize> {
        let len = addr_len()?;
        let mut iov = libc::iovec {
            iov_base: frame.as_ptr().cast_mut().cast::<libc::c_void>(),
            iov_len: frame.len(),
        };
        // SAFETY: mmsghdr is plain old data; all-zero is a valid value.
        let mut header: libc::mmsghdr = unsafe { std::mem::zeroed() };
        header.msg_hdr.msg_name = (&raw const self.addr).cast_mut().cast::<libc::c_void>();
        header.msg_hdr.msg_namelen = len;
        header.msg_hdr.msg_iov = &raw mut iov;
        header.msg_hdr.msg_iovlen = 1;
        let mut headers = vec![header; count];
        let n = libc::c_uint::try_from(count).map_err(invalid)?;
        // SAFETY: every header points to `iov` (the frame) and to the
        // address, which live for the duration of the call; the kernel only
        // reads them (and writes `msg_len`).
        let sent = unsafe { libc::sendmmsg(self.socket.as_raw_fd(), headers.as_mut_ptr(), n, 0) };
        usize::try_from(sent).map_err(|_| io::Error::last_os_error())
    }
}

/// Classic BPF: the VLAN tag of the frame (`SKF_AD_OFF + SKF_AD_VLAN_TAG`).
/// The kernel moves the tag out of a received frame before a tap sees it,
/// so the ethertype is at offset 12 also in a tagged frame.
const ANCILLARY_VLAN_TAG: u32 = 0xFFFF_F000 + 44;
/// Classic BPF: 1 if the frame has a VLAN tag (`SKF_AD_VLAN_TAG_PRESENT`).
const ANCILLARY_VLAN_PRESENT: u32 = 0xFFFF_F000 + 48;

/// Classic BPF opcodes used here.
mod bpf {
    pub const LD_W_ABS: u16 = 0x20;
    pub const LD_H_ABS: u16 = 0x28;
    pub const LD_B_ABS: u16 = 0x30;
    pub const LD_H_IND: u16 = 0x48;
    pub const LDX_B_MSH: u16 = 0xb1;
    pub const AND_K: u16 = 0x54;
    pub const JEQ_K: u16 = 0x15;
    pub const JSET_K: u16 = 0x45;
    pub const RET_K: u16 = 0x06;
}

/// One BPF instruction without jumps.
fn op(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

/// Ends `program` with `ret 0` and points every test that fails there: a
/// `jeq` without jumps of its own when false, a `jset` when true.
fn with_reject(mut program: Vec<libc::sock_filter>) -> Vec<libc::sock_filter> {
    program.push(op(bpf::RET_K, 0));
    let reject = program.len() - 1;
    for (i, ins) in program.iter_mut().enumerate() {
        let to_reject = u8::try_from(reject - i - 1).unwrap_or(u8::MAX);
        match ins.code {
            // A plain test (no jump of its own): it must hold.
            bpf::JEQ_K if ins.jt == 0 && ins.jf == 0 => ins.jf = to_reject,
            bpf::JSET_K => ins.jt = to_reject,
            _ => {}
        }
    }
    program
}

/// `vid`: tagged with that VLAN; `None`: untagged.
fn vlan_check(vid: Option<u16>) -> Vec<libc::sock_filter> {
    match vid {
        Some(vid) => vec![
            op(bpf::LD_W_ABS, ANCILLARY_VLAN_PRESENT),
            op(bpf::JEQ_K, 1),
            op(bpf::LD_W_ABS, ANCILLARY_VLAN_TAG),
            op(bpf::AND_K, 0x0fff),
            op(bpf::JEQ_K, u32::from(vid)),
        ],
        None => vec![op(bpf::LD_W_ABS, ANCILLARY_VLAN_PRESENT), op(bpf::JEQ_K, 0)],
    }
}

/// DHCP (IPv4, UDP to port 67 or 68, no fragment) in VLAN `vid` (`None`:
/// untagged), the first 350 bytes.
fn dhcp_program(vid: Option<u16>) -> Vec<libc::sock_filter> {
    let mut program = vlan_check(vid);
    program.extend([
        op(bpf::LD_H_ABS, 12),
        op(bpf::JEQ_K, 0x0800),
        op(bpf::LD_B_ABS, 23),
        op(bpf::JEQ_K, 17),
        op(bpf::LD_H_ABS, 20),
        op(bpf::JSET_K, 0x1fff),
        // X = length of the IP header.
        op(bpf::LDX_B_MSH, 14),
        op(bpf::LD_H_IND, 14 + 2),
    ]);
    // dport 67 or 68: accept (the first jumps over the second test).
    let mut first = op(bpf::JEQ_K, 67);
    first.jt = 1;
    first.jf = 0;
    program.push(first);
    // `jf` is set by `with_reject`; true falls through to the accept.
    program.push(op(bpf::JEQ_K, 68));
    program.push(op(bpf::RET_K, 350));
    with_reject(program)
}

/// The UDP port of the test frames of a speed test (discard).
pub const TEST_PORT: u16 = 9;

/// The test frames of a speed test: IPv4 (header of 20 bytes, as built),
/// UDP to [`TEST_PORT`], `magic` at the start of the payload; the first 64
/// bytes.
fn test_program(magic: [u8; 4]) -> Vec<libc::sock_filter> {
    with_reject(vec![
        op(bpf::LD_H_ABS, 12),
        op(bpf::JEQ_K, 0x0800),
        op(bpf::LD_B_ABS, 23),
        op(bpf::JEQ_K, 17),
        op(bpf::LD_H_ABS, 36),
        op(bpf::JEQ_K, u32::from(TEST_PORT)),
        op(bpf::LD_W_ABS, 42),
        op(bpf::JEQ_K, u32::from_be_bytes(magic)),
        op(bpf::RET_K, 64),
    ])
}

/// `ldh [12]; jeq ethertype; ld [14]; jeq magic;` with `vid` also
/// `ld vlan_tag; and 0xfff; jeq vid;` then `ret 256; ret 0`.
fn filter_program(ethertype: u16, magic: [u8; 4], vid: Option<u16>) -> Vec<libc::sock_filter> {
    let mut program = vec![
        op(bpf::LD_H_ABS, 12),
        op(bpf::JEQ_K, u32::from(ethertype)),
        op(bpf::LD_W_ABS, 14),
        op(bpf::JEQ_K, u32::from_be_bytes(magic)),
    ];
    if let Some(vid) = vid {
        program.push(op(bpf::LD_W_ABS, ANCILLARY_VLAN_TAG));
        program.push(op(bpf::AND_K, 0x0fff));
        program.push(op(bpf::JEQ_K, u32::from(vid)));
    }
    program.push(op(bpf::RET_K, 256));
    with_reject(program)
}

/// Receives the frames a filter lets through that come in at one
/// interface, while it is open.
pub struct Tap {
    socket: OwnedFd,
}

impl Tap {
    /// Opens the tap on `ifname` for frames of `ethertype` that start with
    /// `magic` after it and, with `vid`, arrive tagged with that VLAN. The
    /// filter runs in the kernel before the socket sees anything; frames sent
    /// by this box are not copied. The tap sees the frames before tc and the
    /// bridge: also those a tc rule drops.
    pub fn open(
        ifname: &str,
        ethertype: u16,
        magic: [u8; 4],
        vid: Option<u16>,
    ) -> io::Result<Self> {
        Self::open_with(ifname, filter_program(ethertype, magic, vid))
    }

    /// Opens the tap on `ifname` for the test frames of a speed test with
    /// `magic`.
    pub fn open_test(ifname: &str, magic: [u8; 4]) -> io::Result<Self> {
        Self::open_with(ifname, test_program(magic))
    }

    /// Opens the tap on `ifname` for the DHCP frames (IPv4) that arrive in
    /// VLAN `vid` (`None`: untagged).
    pub fn open_dhcp(ifname: &str, vid: Option<u16>) -> io::Result<Self> {
        Self::open_with(ifname, dhcp_program(vid))
    }

    #[allow(unsafe_code)]
    fn open_with(ifname: &str, mut program: Vec<libc::sock_filter>) -> io::Result<Self> {
        let index = index_of(ifname)?;
        let socket = packet_socket()?;
        let filter = libc::sock_fprog {
            len: libc::c_ushort::try_from(program.len()).map_err(invalid)?,
            filter: program.as_mut_ptr(),
        };
        let fprog_len =
            libc::socklen_t::try_from(std::mem::size_of::<libc::sock_fprog>()).map_err(invalid)?;
        // SAFETY: `filter` points to `program`, both live for the call; the
        // kernel copies the program.
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                (&raw const filter).cast::<libc::c_void>(),
                fprog_len,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let one: libc::c_int = 1;
        let int_len =
            libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).map_err(invalid)?;
        // SAFETY: `one` lives for the call, its size is given. An older
        // kernel without the option copies the sent frames too (the filter
        // drops them): not an error.
        let _ = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_PACKET,
                PACKET_IGNORE_OUTGOING,
                (&raw const one).cast::<libc::c_void>(),
                int_len,
            )
        };
        // Only now the socket receives: from this interface, filtered.
        let all = u16::try_from(libc::ETH_P_ALL).map_err(invalid)?;
        let addr = link_addr(index, all);
        // SAFETY: the address lives for the call; its length is exact.
        if unsafe {
            libc::bind(
                socket.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                addr_len()?,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { socket })
    }

    /// The frames the filter took since the last call (also those the
    /// socket had no room for: they arrived all the same).
    #[allow(unsafe_code)]
    pub fn taken(&self) -> io::Result<u64> {
        let mut stats = TpacketStats::default();
        let mut len =
            libc::socklen_t::try_from(std::mem::size_of::<TpacketStats>()).map_err(invalid)?;
        // SAFETY: `stats` and `len` live for the call; `len` is its size.
        if unsafe {
            libc::getsockopt(
                self.socket.as_raw_fd(),
                libc::SOL_PACKET,
                PACKET_STATISTICS,
                (&raw mut stats).cast::<libc::c_void>(),
                &raw mut len,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(stats.packets))
    }

    /// A frame that is waiting, without blocking: its length in `buf`.
    #[allow(unsafe_code)]
    pub fn try_recv(&self, buf: &mut [u8]) -> Option<usize> {
        // SAFETY: the buffer lives for the call; its length is exact.
        let n = unsafe {
            libc::recv(
                self.socket.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        usize::try_from(n).ok()
    }

    /// Waits up to `timeout` until one of `taps` has a frame; true if one
    /// has (or on an error, so that the caller reads and finds nothing).
    #[allow(unsafe_code)]
    pub fn poll(taps: &[&Self], timeout: Duration) -> bool {
        let mut fds: Vec<libc::pollfd> = taps
            .iter()
            .map(|t| libc::pollfd {
                fd: t.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        let Ok(n) = libc::nfds_t::try_from(fds.len()) else {
            return false;
        };
        // SAFETY: `fds` is a valid array of `n` pollfds for the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), n, ms) };
        ready != 0
    }

    /// Waits up to `timeout` for a frame for which `accept` is true.
    #[allow(unsafe_code)]
    pub fn wait(&self, timeout: Duration, mut accept: impl FnMut(&[u8]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 256];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let mut poll = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
            // SAFETY: one valid pollfd for the duration of the call.
            let ready = unsafe { libc::poll(&raw mut poll, 1, ms) };
            if ready < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return false;
            }
            if ready == 0 {
                return false;
            }
            // SAFETY: the buffer lives for the call; its length is exact.
            let n = unsafe {
                libc::recv(
                    self.socket.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if let Ok(n) = usize::try_from(n)
                && accept(buf.get(..n).unwrap_or_default())
            {
                return true;
            }
        }
    }
}
