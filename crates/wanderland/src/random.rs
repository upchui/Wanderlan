//! Random bytes from the kernel (`/dev/urandom`, also there under
//! `PrivateDevices=yes`).

use std::io::Read as _;

use anyhow::{Context as _, Result};

/// `N` random bytes.
pub fn bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut out))
        .context("cannot read random bytes from /dev/urandom")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_draws_differ() {
        let a = super::bytes::<32>().unwrap();
        let b = super::bytes::<32>().unwrap();
        assert_ne!(a, b);
    }
}
