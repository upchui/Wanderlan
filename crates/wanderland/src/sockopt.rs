//! Socket options not exposed by the standard library.

use std::io;
use std::os::fd::AsRawFd;

/// Sets `IP_MTU_DISCOVER = IP_PMTUDISC_PROBE`: packets are sent with the DF
/// bit set and the cached path MTU is ignored, so probes larger than the
/// path are lost instead of being fragmented or rejected locally.
#[allow(unsafe_code)]
pub fn set_pmtudisc_probe(socket: &impl AsRawFd) -> io::Result<()> {
    let value: libc::c_int = libc::IP_PMTUDISC_PROBE;
    let len = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: the file descriptor is a valid, open socket owned by the
    // caller; the option value points to a c_int that lives for the duration
    // of the call and `len` is its exact size.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            (&raw const value).cast::<libc::c_void>(),
            len,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
