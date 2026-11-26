//! Ethernet frames through an interface (AF_PACKET): sending the keepalives
//! of the client tunnels, receiving the test frame of the server.
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

/// Receives the frames with one ethertype and four magic bytes after it
/// that come in at one interface, while it is open.
pub struct Tap {
    socket: OwnedFd,
}

impl Tap {
    /// Opens the tap on `ifname` for frames of `ethertype` that start with
    /// `magic` after it. The filter runs in the kernel before the socket
    /// sees anything; frames sent by this box are not copied.
    #[allow(unsafe_code)]
    pub fn open(ifname: &str, ethertype: u16, magic: [u8; 4]) -> io::Result<Self> {
        let index = index_of(ifname)?;
        let socket = packet_socket()?;
        // ldh [12]; jeq ethertype; ld [14]; jeq magic; ret 256; ret 0
        let mut program = [
            libc::sock_filter {
                code: 0x28,
                jt: 0,
                jf: 0,
                k: 12,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 3,
                k: u32::from(ethertype),
            },
            libc::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 14,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 1,
                k: u32::from_be_bytes(magic),
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 256,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0,
            },
        ];
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
