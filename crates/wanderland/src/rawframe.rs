//! Sending one Ethernet frame into an interface (AF_PACKET), for the
//! keepalives of the client tunnels.
//!
//! The socket has protocol 0: it only sends, the kernel never copies a
//! received frame to it. It needs `CAP_NET_RAW`.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

/// Sends `frame` (a complete Ethernet frame) out of the interface `ifname`.
#[allow(unsafe_code)]
pub fn send(ifname: &str, frame: &[u8]) -> io::Result<()> {
    let name = CString::new(ifname).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `name` is a valid NUL-terminated string for the duration of
    // the call.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a plain socket(2) call; the result is checked before use.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a socket just opened and owned by nothing else; the
    // OwnedFd closes it when dropped.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: sockaddr_ll is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as libc::c_ushort;
    addr.sll_ifindex =
        libc::c_int::try_from(index).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    addr.sll_halen = 6;
    addr.sll_addr[..6].copy_from_slice(frame.get(..6).unwrap_or(&[0; 6]));
    let len = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_ll>())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
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
