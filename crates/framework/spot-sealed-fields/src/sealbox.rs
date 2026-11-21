//! The anonymous-sender public-key box a sealed value is written with.
//!
//! A sealed value is encrypted **to the hub's public key** with an ephemeral X25519 key pair:
//! the sender needs no key of its own and no identity is revealed, and only the holder of the
//! hub's private key can open the result. The Diffie-Hellman shared secret is run through
//! HMAC-SHA-256 in the HKDF-Extract role (salted with both public keys) to produce a 256-bit
//! key for XChaCha20-Poly1305, and the value's place ([`SealedAd`]) is authenticated as
//! associated data.
//!
//! Layout: `magic (4) || ephemeral public key (32) || nonce (24) || ciphertext || tag (16)`.
//!
//! This module holds the **public** half only in a node build (`BoxSecretKey` and `open` are
//! compiled out under the `node` feature): [`seal`] needs no secret, and [`open`] takes
//! the hub's private key as an argument. Only [`crate::hub`] ever holds one, and that module
//! is compiled out of a node build.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use curve25519_dalek::montgomery::MontgomeryPoint;
use hmac::{Hmac, Mac};
use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
#[cfg(not(feature = "node"))]
use spot_secrets::SecretBytes;
use zeroize::Zeroizing;

use crate::bind::SealedAd;
use crate::error::SealedFieldsError;

/// An X25519 public key.
pub const PUBLIC_KEY_LEN: usize = 32;
/// The XChaCha20-Poly1305 nonce length.
pub(crate) const NONCE_LEN: usize = 24;
/// The Poly1305 tag length.
pub(crate) const TAG_LEN: usize = 16;
/// The magic prefix of a sealed blob, so a version can be added later without ambiguity.
const MAGIC: &[u8; 4] = b"SFB1";
/// The domain tag the key derivation binds.
const KDF_TAG: &[u8] = b"spot-sealed-fields/box/v1";

/// A hub public key: the only half a node ever holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HubPublicKey([u8; PUBLIC_KEY_LEN]);

impl HubPublicKey {
    /// Wraps the raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; PUBLIC_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; PUBLIC_KEY_LEN] {
        self.0
    }
}

/// An X25519 private key, zeroised on drop. Never `Clone`, never `Debug`, never serialised.
#[cfg(not(feature = "node"))]
pub struct BoxSecretKey(Zeroizing<[u8; PUBLIC_KEY_LEN]>);

#[cfg(not(feature = "node"))]
impl BoxSecretKey {
    /// A fresh private key from the operating system CSPRNG.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the operating system RNG is unavailable.
    pub fn generate() -> Result<Self, SealedFieldsError> {
        let mut bytes = Zeroizing::new([0u8; PUBLIC_KEY_LEN]);
        OsRng
            .try_fill_bytes(bytes.as_mut())
            .map_err(|_| SealedFieldsError::Malformed("operating system RNG unavailable"))?;
        Ok(Self(bytes))
    }

    /// A private key from caller-supplied material (a test seam).
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the material is not 32 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SealedFieldsError> {
        let key: [u8; PUBLIC_KEY_LEN] = bytes
            .try_into()
            .map_err(|_| SealedFieldsError::Malformed("a private key is 32 bytes"))?;
        Ok(Self(Zeroizing::new(key)))
    }

    /// The public key this private key opens.
    #[must_use]
    pub fn public_key(&self) -> HubPublicKey {
        HubPublicKey(MontgomeryPoint::mul_base_clamped(*self.0).to_bytes())
    }

    /// The Diffie-Hellman shared secret with `peer`, in zeroised memory.
    fn shared(&self, peer: &HubPublicKey) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(MontgomeryPoint(peer.0).mul_clamped(*self.0).to_bytes())
    }
}

/// The hub public key a node seals to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealingKey {
    key: HubPublicKey,
}

impl SealingKey {
    /// From a hub public key.
    #[must_use]
    pub fn new(key: HubPublicKey) -> Self {
        Self { key }
    }

    /// The public key.
    #[must_use]
    pub fn public_key(&self) -> HubPublicKey {
        self.key
    }
}

/// A sealed-field writer that works on both sides: it needs the hub's **public** key only.
///
/// A node holds one; so does the hub. [`SealedWriter::seal`] is the only operation.
#[derive(Clone, Debug)]
pub struct SealedWriter {
    key: SealingKey,
}

impl SealedWriter {
    /// A writer sealing to `key`.
    #[must_use]
    pub fn new(key: SealingKey) -> Self {
        Self { key }
    }

    /// A writer from the hub's public key.
    #[must_use]
    pub fn from_public_key(key: HubPublicKey) -> Self {
        Self::new(SealingKey::new(key))
    }

    /// The public key it seals to.
    #[must_use]
    pub fn sealing_key(&self) -> &SealingKey {
        &self.key
    }

    /// Seals `plaintext` to the hub, binding `ad`.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the operating system RNG is unavailable.
    pub fn seal(&self, plaintext: &[u8], ad: &SealedAd) -> Result<Sealed, SealedFieldsError> {
        seal_bound(plaintext, &self.key.public_key(), ad)
    }
}

/// A sealed value: ciphertext only the hub's private key opens.
///
/// It is safe to store, sync and serialise: the bytes are a box, never a value or a key.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sealed {
    /// `magic || ephemeral public key || nonce || ciphertext || tag`.
    bytes: Vec<u8>,
}

impl Sealed {
    /// The binary form, safe to store.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    /// Parses the binary form. Never panics on arbitrary input.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the blob is not a sealed value.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SealedFieldsError> {
        if bytes.len() < minimum_len() {
            return Err(SealedFieldsError::Malformed(
                "a sealed value is shorter than its header",
            ));
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(SealedFieldsError::Malformed("bad sealed-value magic"));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
        })
    }

    /// The length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always false; a sealed value carries a header, a body and a tag.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealed")
            .field("bytes", &"[sealed]")
            .field("len", &self.bytes.len())
            .finish()
    }
}

fn minimum_len() -> usize {
    MAGIC.len() + PUBLIC_KEY_LEN + NONCE_LEN + TAG_LEN
}

/// The 256-bit box key: HMAC-SHA-256 over the domain tag and shared secret, salted with both
/// public keys, so a key is bound to the exact pair that produced it.
fn derive(
    shared: &[u8; 32],
    ephemeral: &HubPublicKey,
    recipient: &HubPublicKey,
) -> Zeroizing<[u8; 32]> {
    let mut salt = [0u8; PUBLIC_KEY_LEN * 2];
    salt[..PUBLIC_KEY_LEN].copy_from_slice(&ephemeral.0);
    salt[PUBLIC_KEY_LEN..].copy_from_slice(&recipient.0);
    let mut out = Zeroizing::new([0u8; 32]);
    // HMAC accepts a key of any length; the only failure is an unsupported digest size.
    if let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(&salt) {
        mac.update(KDF_TAG);
        mac.update(shared);
        out.copy_from_slice(&mac.finalize().into_bytes());
    }
    out
}

fn fill_random(buf: &mut [u8]) -> Result<(), SealedFieldsError> {
    OsRng
        .try_fill_bytes(buf)
        .map_err(|_| SealedFieldsError::Malformed("operating system RNG unavailable"))
}

/// Seals `value` to `hub_public_key` without binding it to a document.
///
/// This is the plain form; [`seal_bound`] is the same box with a [`SealedAd`], which is what a
/// field write uses so a blob cannot be moved to another field or document.
///
/// # Errors
///
/// [`SealedFieldsError::Malformed`] when the operating system RNG is unavailable.
pub fn seal(value: &[u8], hub_public_key: &HubPublicKey) -> Result<Sealed, SealedFieldsError> {
    seal_bound(value, hub_public_key, &SealedAd::unbound())
}

/// Seals `plaintext` to `recipient`, authenticating `ad`.
///
/// The sender is anonymous: a fresh ephemeral key pair is generated per value, so two seals of
/// the same plaintext are unrelated and no sender identity is carried.
///
/// # Errors
///
/// [`SealedFieldsError::Malformed`] when the operating system RNG is unavailable.
pub fn seal_bound(
    plaintext: &[u8],
    recipient: &HubPublicKey,
    ad: &SealedAd,
) -> Result<Sealed, SealedFieldsError> {
    let mut ephemeral_secret = Zeroizing::new([0u8; PUBLIC_KEY_LEN]);
    fill_random(ephemeral_secret.as_mut())?;
    let ephemeral = HubPublicKey(MontgomeryPoint::mul_base_clamped(*ephemeral_secret).to_bytes());
    let shared = MontgomeryPoint(recipient.0).mul_clamped(*ephemeral_secret);
    let key = derive(&shared.to_bytes(), &ephemeral, recipient);

    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    let cipher = XChaCha20Poly1305::new((&*key).into());
    let body = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &ad.canonical_bytes(),
            },
        )
        .map_err(|_| SealedFieldsError::Tampered)?;

    let mut bytes = Vec::with_capacity(MAGIC.len() + PUBLIC_KEY_LEN + NONCE_LEN + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&ephemeral.0);
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&body);
    Ok(Sealed { bytes })
}

/// Opens `sealed` with `secret`, checking `ad`.
///
/// # Errors
///
/// [`SealedFieldsError::Tampered`] for a wrong key, a wrong place or altered bytes, and
/// [`SealedFieldsError::Malformed`] for a blob that is not a sealed value.
#[cfg(not(feature = "node"))]
pub fn open(
    sealed: &Sealed,
    secret: &BoxSecretKey,
    ad: &SealedAd,
) -> Result<SecretBytes, SealedFieldsError> {
    let bytes = &sealed.bytes;
    if bytes.len() < minimum_len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(SealedFieldsError::Malformed("not a sealed value"));
    }
    let mut ephemeral = [0u8; PUBLIC_KEY_LEN];
    ephemeral.copy_from_slice(&bytes[MAGIC.len()..MAGIC.len() + PUBLIC_KEY_LEN]);
    let nonce_start = MAGIC.len() + PUBLIC_KEY_LEN;
    let nonce = &bytes[nonce_start..nonce_start + NONCE_LEN];
    let body = &bytes[nonce_start + NONCE_LEN..];

    let ephemeral = HubPublicKey(ephemeral);
    let shared = secret.shared(&ephemeral);
    let key = derive(&shared, &ephemeral, &secret.public_key());
    let cipher = XChaCha20Poly1305::new((&*key).into());
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: &ad.canonical_bytes(),
            },
        )
        .map(SecretBytes::new)
        .map_err(|_| SealedFieldsError::Tampered)
}

#[cfg(all(test, not(feature = "node")))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn pair() -> (BoxSecretKey, HubPublicKey) {
        let secret = BoxSecretKey::generate().expect("secret");
        let public = secret.public_key();
        (secret, public)
    }

    #[test]
    fn seal_round_trips_and_only_the_holder_opens_it() {
        let (secret, public) = pair();
        let (other, _) = pair();
        let sealed = seal(b"DE89", &public).expect("seal");
        let opened = open(&sealed, &secret, &SealedAd::unbound()).expect("open");
        assert_eq!(opened.expose(), b"DE89");
        assert!(open(&sealed, &other, &SealedAd::unbound()).is_err());
    }

    #[test]
    fn the_place_is_authenticated() {
        let (secret, public) = pair();
        let tenant = spot_protected_keys::TenantId::new("acme").expect("tenant");
        let sealed = seal_bound(
            b"DE89",
            &public,
            &SealedAd::of_field(tenant.clone(), "Bank Account", "iban", "BA-1"),
        )
        .expect("seal");
        assert!(open(&sealed, &secret, &SealedAd::unbound()).is_err());
        let moved = SealedAd::of_field(tenant, "Bank Account", "iban", "BA-2");
        assert!(open(&sealed, &secret, &moved).is_err());
    }

    #[test]
    fn two_seals_of_the_same_value_differ() {
        let (_, public) = pair();
        let a = seal(b"same", &public).expect("seal");
        let b = seal(b"same", &public).expect("seal");
        assert_ne!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let (secret, _) = pair();
        for len in 0..64 {
            let bytes = vec![0x41u8; len];
            if let Ok(sealed) = Sealed::from_bytes(&bytes) {
                let _ = open(&sealed, &secret, &SealedAd::unbound());
            }
        }
    }

    #[test]
    fn debug_never_prints_the_body() {
        let (_, public) = pair();
        let sealed = seal(b"DE89", &public).expect("seal");
        let text = format!("{sealed:?}");
        assert!(text.contains("[sealed]"));
        assert!(!text.contains("DE89"));
    }

    #[test]
    fn a_private_key_never_prints_its_material() {
        let secret = BoxSecretKey::generate().expect("secret");
        // `BoxSecretKey` has no `Debug`, so this is a compile-time property; prove the type is
        // usable and the public half is stable.
        assert_eq!(secret.public_key(), secret.public_key());
    }
}
