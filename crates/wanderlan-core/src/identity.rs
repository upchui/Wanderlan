//! Site identities and the keys of a tunnel.
//!
//! Every site has an X25519 key pair; its public key is the identity of the
//! site (`id` in `network.toml`), like a WireGuard public key. The two sites
//! of a tunnel derive the keys of that tunnel from their key pairs
//! (Diffie-Hellman), so no key ever travels over the network or is written
//! to `network.toml`, and no third site knows them. Per tunnel there are
//!
//! * the VNI of its VXLAN tunnel (the kernel drops frames with another one),
//! * the probe key (an HMAC tag on every probe: the peer proves that it
//!   holds its private key) and
//! * the sync key (an HMAC tag on the configuration exchange).
//!
//! The VNI travels in every tunnel packet; it tells boxes apart (a second
//! box behind the same NAT, a cloned machine) but is no secret. The HMAC keys
//! never travel. Nothing here encrypts: confidentiality is the job of the
//! transport network (a VPN).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

/// Bytes of a public or private key.
pub const KEY_LEN: usize = 32;
/// Characters of a key as text (base64 with padding, like WireGuard keys).
pub const KEY_TEXT_LEN: usize = 44;
/// Characters of the short form of an identity shown in the UI.
pub const FINGERPRINT_LEN: usize = 8;
/// Bytes of an HMAC tag (HMAC-SHA256, truncated).
pub const TAG_LEN: usize = 16;

/// An HMAC tag.
pub type Tag = [u8; TAG_LEN];

/// Salt of the key derivation; a new version changes every derived key.
const SALT: &[u8] = b"wanderlan identity v1";

/// The identity of a site: its X25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SiteId([u8; KEY_LEN]);

impl SiteId {
    /// The identity with these key bytes.
    pub const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The key bytes.
    pub const fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// The first characters of the identity, to tell sites apart in the UI
    /// (also found with a text search in `network.toml`).
    pub fn fingerprint(&self) -> String {
        let mut text = self.to_string();
        text.truncate(FINGERPRINT_LEN);
        text
    }
}

impl fmt::Display for SiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&base64_encode(&self.0))
    }
}

impl fmt::Debug for SiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SiteId({self})")
    }
}

/// Why a text is not an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError;

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not a wanderlan identity ({KEY_TEXT_LEN} characters of base64, ending in \"=\")"
        )
    }
}

impl std::error::Error for IdError {}

impl FromStr for SiteId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        base64_decode_key(s.trim()).map(Self).ok_or(IdError)
    }
}

/// The key pair of this site.
pub struct Identity {
    secret: StaticSecret,
    id: SiteId,
}

impl Identity {
    /// The identity with this private key (32 random bytes).
    pub fn from_secret(bytes: [u8; KEY_LEN]) -> Self {
        let secret = StaticSecret::from(bytes);
        let id = SiteId(PublicKey::from(&secret).to_bytes());
        Self { secret, id }
    }

    /// Reads the private key as written by [`Identity::secret_text`].
    pub fn from_secret_text(text: &str) -> Result<Self, IdError> {
        base64_decode_key(text.trim())
            .map(Self::from_secret)
            .ok_or(IdError)
    }

    /// The private key as text (one line), for the identity file.
    pub fn secret_text(&self) -> String {
        let mut text = base64_encode(self.secret.as_bytes());
        text.push('\n');
        text
    }

    /// The public identity.
    pub const fn id(&self) -> SiteId {
        self.id
    }

    /// The keys of the tunnel to `peer`. `None` for this site's own identity
    /// and for keys no real peer has (a low-order point gives a shared
    /// secret everybody knows).
    pub fn pair(&self, peer: &SiteId) -> Option<PairKeys> {
        if *peer == self.id {
            return None;
        }
        let shared = self.secret.diffie_hellman(&PublicKey::from(peer.0));
        if !shared.was_contributory() {
            return None;
        }
        // Both sites put the two identities in the same order.
        let (low, high) = if self.id < *peer {
            (&self.id, peer)
        } else {
            (peer, &self.id)
        };
        let hkdf = Hkdf::<Sha256>::new(Some(SALT), shared.as_bytes());
        let expand = |label: &[u8], out: &mut [u8]| {
            hkdf.expand_multi_info(&[b"wanderlan pair ", label, &low.0, &high.0], out)
                .is_ok()
        };
        let mut vni = [0u8; 3];
        let mut probe = [0u8; KEY_LEN];
        let mut sync = [0u8; KEY_LEN];
        if !(expand(b"vni", &mut vni) && expand(b"probe", &mut probe) && expand(b"sync", &mut sync))
        {
            return None;
        }
        Some(PairKeys {
            // 24 bits; 0 would read as "no VNI" in some tools.
            vni: u32::from_be_bytes([0, vni[0], vni[1], vni[2]]).max(1),
            probe: Hmac::<Sha256>::new_from_slice(&probe).ok()?,
            sync: Hmac::<Sha256>::new_from_slice(&sync).ok()?,
        })
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What a site knows about the identity of a peer it has keys for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PeerIdentity {
    /// No authenticated answer yet: the tunnel waits.
    #[default]
    Waiting,
    /// It answered with a valid tag: the right box.
    Confirmed,
    /// Something at its address answers, but with another key (another
    /// box at that address, or a box that was reset).
    WrongKey,
}

/// The keys of the tunnel between two sites. The HMAC states are keyed once;
/// every tag clones them.
#[derive(Clone)]
pub struct PairKeys {
    vni: u32,
    probe: Hmac<Sha256>,
    sync: Hmac<Sha256>,
}

impl PairKeys {
    /// The VNI of the tunnel (the same at both ends; 24 bits, never 0).
    pub const fn vni(&self) -> u32 {
        self.vni
    }

    /// The tag of a probe over the concatenation of `parts`.
    pub fn probe_tag(&self, parts: &[&[u8]]) -> Tag {
        tag(&self.probe, parts)
    }

    /// True if `tag` is the probe tag over `parts` (constant time).
    pub fn probe_verify(&self, parts: &[&[u8]], tag: &[u8]) -> bool {
        verify(&self.probe, parts, tag)
    }

    /// The tag of a sync message over the concatenation of `parts`.
    pub fn sync_tag(&self, parts: &[&[u8]]) -> Tag {
        tag(&self.sync, parts)
    }

    /// True if `tag` is the sync tag over `parts` (constant time).
    pub fn sync_verify(&self, parts: &[&[u8]], tag: &[u8]) -> bool {
        verify(&self.sync, parts, tag)
    }
}

impl fmt::Debug for PairKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairKeys { .. }")
    }
}

fn tag(mac: &Hmac<Sha256>, parts: &[&[u8]]) -> Tag {
    let mut mac = mac.clone();
    for part in parts {
        mac.update(part);
    }
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&full[..TAG_LEN]);
    out
}

fn verify(mac: &Hmac<Sha256>, parts: &[&[u8]], tag: &[u8]) -> bool {
    if tag.len() != TAG_LEN {
        return false;
    }
    let mut mac = mac.clone();
    for part in parts {
        mac.update(part);
    }
    mac.verify_truncated_left(tag).is_ok()
}

/// The derived keys of this site towards its peers, cached: one
/// Diffie-Hellman per peer and identity change.
#[derive(Debug, Clone)]
pub struct Keyring {
    me: Arc<Identity>,
    pairs: BTreeMap<SiteId, Option<Arc<PairKeys>>>,
}

impl Keyring {
    /// A keyring for this site's identity.
    pub fn new(me: Arc<Identity>) -> Self {
        Self {
            me,
            pairs: BTreeMap::new(),
        }
    }

    /// This site's identity.
    pub fn me(&self) -> &Arc<Identity> {
        &self.me
    }

    /// Keeps the keys of exactly these peers, deriving the missing ones.
    pub fn update<'a>(&mut self, peers: impl IntoIterator<Item = &'a SiteId>) {
        let wanted: Vec<SiteId> = peers.into_iter().copied().collect();
        self.pairs.retain(|id, _| wanted.contains(id));
        for id in wanted {
            let me = &self.me;
            self.pairs
                .entry(id)
                .or_insert_with(|| me.pair(&id).map(Arc::new));
        }
    }

    /// The keys towards `peer`, if it is one of the peers of
    /// [`Keyring::update`] and a valid identity.
    pub fn get(&self, peer: &SiteId) -> Option<Arc<PairKeys>> {
        self.pairs.get(peer).cloned().flatten()
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes exactly one key in the canonical form [`base64_encode`] writes.
fn base64_decode_key(text: &str) -> Option<[u8; KEY_LEN]> {
    let bytes = text.as_bytes();
    if bytes.len() != KEY_TEXT_LEN || bytes[KEY_TEXT_LEN - 1] != b'=' {
        return None;
    }
    let value = |c: u8| ALPHABET.iter().position(|&a| a == c).map(|v| v as u32);
    let mut out = [0u8; KEY_LEN];
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut n = 0;
    for &c in &bytes[..KEY_TEXT_LEN - 1] {
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            *out.get_mut(n)? = (acc >> bits) as u8;
            n += 1;
            acc &= (1 << bits) - 1;
        }
    }
    // 43 characters carry 258 bits: the last two must be zero (canonical).
    (n == KEY_LEN && acc == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(seed: u8) -> Identity {
        Identity::from_secret([seed; KEY_LEN])
    }

    #[test]
    fn an_identity_reads_back_from_its_text() {
        let me = identity(7);
        let text = me.id().to_string();
        assert_eq!(text.len(), KEY_TEXT_LEN);
        assert!(text.ends_with('='));
        assert_eq!(text.parse::<SiteId>().unwrap(), me.id());
        assert_eq!(format!(" {text}\n").parse::<SiteId>().unwrap(), me.id());
        let again = Identity::from_secret_text(&me.secret_text()).unwrap();
        assert_eq!(again.id(), me.id());
        assert_eq!(me.id().fingerprint(), text[..FINGERPRINT_LEN]);
    }

    #[test]
    fn base64_matches_the_standard_encoding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // A WireGuard-style key of 32 zero bytes.
        assert_eq!(
            base64_encode(&[0u8; 32]),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        );
    }

    #[test]
    fn only_canonical_keys_are_identities() {
        let good = identity(1).id().to_string();
        for bad in [
            "",
            "short=",
            &good[..KEY_TEXT_LEN - 1],
            &format!("{good}A"),
            &good.replace('=', "A"),
            // Not base64.
            &format!("{}!=", &good[..KEY_TEXT_LEN - 2]),
            // The unused low bits of the last character set.
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB=",
        ] {
            assert!(bad.parse::<SiteId>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn both_sites_derive_the_same_keys() {
        let a = identity(1);
        let b = identity(2);
        let ab = a.pair(&b.id()).unwrap();
        let ba = b.pair(&a.id()).unwrap();
        assert_eq!(ab.vni(), ba.vni());
        assert!(ab.vni() > 0 && ab.vni() < 1 << 24);
        let parts: [&[u8]; 2] = [b"probe", b"payload"];
        let tag = ab.probe_tag(&parts);
        assert!(ba.probe_verify(&parts, &tag));
        assert!(!ba.sync_verify(&parts, &tag), "probe and sync keys differ");
        assert!(ba.sync_verify(&parts, &ab.sync_tag(&parts)));
    }

    #[test]
    fn another_pair_has_other_keys() {
        let a = identity(1);
        let b = identity(2);
        let c = identity(3);
        let ab = a.pair(&b.id()).unwrap();
        let ac = a.pair(&c.id()).unwrap();
        assert_ne!(ab.vni(), ac.vni());
        let parts: [&[u8]; 1] = [b"x"];
        assert!(!ac.probe_verify(&parts, &ab.probe_tag(&parts)));
    }

    #[test]
    fn a_changed_message_or_tag_fails() {
        let a = identity(1);
        let b = identity(2);
        let keys = a.pair(&b.id()).unwrap();
        let tag = keys.probe_tag(&[b"hello"]);
        assert!(
            keys.probe_verify(&[b"hel", b"lo"], &tag),
            "parts concatenate"
        );
        assert!(!keys.probe_verify(&[b"hellO"], &tag));
        let mut bad = tag;
        bad[3] ^= 1;
        assert!(!keys.probe_verify(&[b"hello"], &bad));
        assert!(!keys.probe_verify(&[b"hello"], &tag[..8]));
    }

    #[test]
    fn no_keys_towards_itself_or_a_weak_key() {
        let a = identity(1);
        assert!(a.pair(&a.id()).is_none());
        // The identity point and other low-order points.
        assert!(a.pair(&SiteId::from_bytes([0; KEY_LEN])).is_none());
        let mut one = [0u8; KEY_LEN];
        one[0] = 1;
        assert!(a.pair(&SiteId::from_bytes(one)).is_none());
    }

    #[test]
    fn the_keyring_keeps_only_the_current_peers() {
        let me = Arc::new(identity(1));
        let b = identity(2).id();
        let c = identity(3).id();
        let mut ring = Keyring::new(me);
        ring.update([&b, &c]);
        let kb = ring.get(&b).unwrap();
        ring.update([&b]);
        assert!(ring.get(&c).is_none());
        assert!(
            Arc::ptr_eq(&kb, &ring.get(&b).unwrap()),
            "cached, not derived again"
        );
        assert!(ring.get(&SiteId::from_bytes([9; KEY_LEN])).is_none());
    }

    #[test]
    fn the_private_key_is_not_in_the_debug_output() {
        let me = identity(5);
        let secret = me.secret_text();
        let debug = format!("{me:?}");
        assert!(!debug.contains(secret.trim()), "{debug}");
        assert!(debug.contains(&me.id().to_string()));
    }
}
