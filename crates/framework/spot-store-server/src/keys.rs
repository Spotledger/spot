//! Key material: the store key (an adapter over `spot-bundle-signing`'s key pair), the operator
//! credential, publisher key tokens and the constant-time comparison of their hashes.
//!
//! Secrets are read from files named in the configuration, never from the configuration itself,
//! and on Unix a file readable by anyone but its owner is refused. A publisher key is only ever
//! compared as its SHA-256; no `Debug` output prints a key, a hash or a seed.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};
use spot_bundle_signing::{KeyId, KeyPair, RevocationSigner, generate_keypair};
use spot_store_commerce::StoreSigner;
use spot_store_state::KeyHash;

use crate::config::StartupError;
use crate::error::codes;

/// The most bytes a key or secret file may hold.
const MAX_KEY_FILE: u64 = 4096;

static CT_COMPARES: AtomicU64 = AtomicU64::new(0);

/// How many constant-time comparisons [`ct_eq`] has made in this process (a test hook: it proves
/// the credential checks go through the constant-time path).
#[must_use]
pub fn ct_compare_count() -> u64 {
    CT_COMPARES.load(Ordering::Relaxed)
}

/// Compares two 32-byte digests in time that does not depend on where they first differ.
#[must_use]
pub fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    CT_COMPARES.fetch_add(1, Ordering::Relaxed);
    let mut difference = 0_u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    std::hint::black_box(difference) == 0
}

/// The SHA-256 of a presented credential.
#[must_use]
pub fn sha256_bytes(text: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    out.copy_from_slice(&Sha256::digest(text.as_bytes()));
    out
}

/// The stored form of a publisher key.
#[must_use]
pub fn hash_key(token: &str) -> KeyHash {
    KeyHash::from_bytes(sha256_bytes(token))
}

/// Lower-case hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Whether `token` has the shape of a publisher key (`spk_` and 64 lower-case hex digits).
#[must_use]
pub fn is_publisher_token(token: &str) -> bool {
    token.len() == 68
        && token.starts_with("spk_")
        && token
            .bytes()
            .skip(4)
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A fresh publisher key: `spk_` and 64 hex digits from the operating system's random source
/// (two random UUIDs, 244 random bits). Shown once to the operator; only its hash is stored.
pub(crate) fn new_publisher_token() -> String {
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    format!("spk_{}{}", first.simple(), second.simple())
}

fn read_secret_file(path: &Path) -> Result<String, StartupError> {
    let fail = |reason: &str| StartupError::new(codes::KEY_FILE, format!("a key file {reason}"));
    let meta = std::fs::metadata(path).map_err(|_| fail("cannot be read"))?;
    if !meta.is_file() {
        return Err(fail("is not a regular file"));
    }
    if meta.len() > MAX_KEY_FILE {
        return Err(fail("is too large"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(fail("is readable by other users; use mode 0600"));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|_| fail("cannot be read as text"))?;
    Ok(text.trim().to_owned())
}

/// Reads a one-line secret that may be spread over a file with a trailing newline.
pub(crate) fn read_secret_text(path: &Path) -> Result<String, StartupError> {
    read_secret_file(path)
}

/// The one store key: signs revocation lists (and, in WP-STR-08, licences) and nothing else. It
/// never signs a bundle, a certificate or a review. Its public half is a root that sites pin.
pub struct StoreKey {
    pair: KeyPair,
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "StoreKey {{ key_id: {}, secret: <redacted> }}",
            self.pair.id().as_str()
        )
    }
}

impl StoreKey {
    /// The key made from a 32-byte Ed25519 seed.
    #[must_use]
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            pair: generate_keypair(seed),
        }
    }

    /// Loads the key from `path`: 64 hex digits (the Ed25519 seed), mode 0600 on Unix.
    ///
    /// # Errors
    /// `store_server.key_file` when the file is missing, unreadable, too permissive or not a seed.
    pub fn from_file(path: &Path) -> Result<Self, StartupError> {
        let text = read_secret_file(path)?;
        let bad = || StartupError::new(codes::KEY_FILE, "the store key file is not 64 hex digits");
        if text.len() != 64 {
            return Err(bad());
        }
        let mut seed = [0_u8; 32];
        for (index, slot) in seed.iter_mut().enumerate() {
            let pair = text.get(index * 2..index * 2 + 2).ok_or_else(bad)?;
            *slot = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
        }
        Ok(Self::from_seed(&seed))
    }

    /// The key pair, for the in-process V1 revocation list the registry's own checks use.
    pub(crate) fn pair(&self) -> &KeyPair {
        &self.pair
    }

    /// The key id.
    #[must_use]
    pub fn id(&self) -> &KeyId {
        self.pair.id()
    }

    /// The public key, which sites pin.
    #[must_use]
    pub fn public(&self) -> spot_bundle_signing::PublicKey {
        self.pair.public()
    }
}

impl RevocationSigner for StoreKey {
    fn key_id(&self) -> &KeyId {
        self.pair.id()
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        <KeyPair as RevocationSigner>::sign(&self.pair, message)
    }
}

impl StoreSigner for StoreKey {
    fn key_id(&self) -> &str {
        self.pair.id().as_str()
    }

    fn public_key_bytes(&self) -> [u8; 32] {
        *self.pair.public().as_bytes()
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        <KeyPair as RevocationSigner>::sign(&self.pair, message)
    }
}

/// The operator credential: a bearer secret kept only as its SHA-256.
pub struct AdminCredential {
    hash: [u8; 32],
}

impl fmt::Debug for AdminCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdminCredential(..)")
    }
}

impl AdminCredential {
    /// The credential for `secret` (at least 32 printable ASCII characters).
    ///
    /// # Errors
    /// `store_server.key_file` for a short or non-printable secret.
    pub fn from_secret(secret: &str) -> Result<Self, StartupError> {
        let printable = secret.bytes().all(|b| (0x21..=0x7e).contains(&b));
        if secret.len() < 32 || secret.len() > 256 || !printable {
            return Err(StartupError::new(
                codes::KEY_FILE,
                "the operator secret is not 32 to 256 printable characters",
            ));
        }
        Ok(Self {
            hash: sha256_bytes(secret),
        })
    }

    /// Loads the credential from `path` (mode 0600 on Unix).
    ///
    /// # Errors
    /// `store_server.key_file`.
    pub fn from_file(path: &Path) -> Result<Self, StartupError> {
        Self::from_secret(&read_secret_file(path)?)
    }

    /// Whether `presented` is the operator secret (constant time on the hashes).
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        ct_eq(&self.hash, &sha256_bytes(presented))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_counts_every_comparison_and_compares_whole_digests() {
        let before = ct_compare_count();
        let a = sha256_bytes("alpha");
        let mut b = a;
        assert!(ct_eq(&a, &b));
        b[31] ^= 1;
        assert!(!ct_eq(&a, &b));
        b = a;
        b[0] ^= 1;
        assert!(!ct_eq(&a, &b));
        assert!(ct_compare_count() >= before + 3);
    }

    #[test]
    fn publisher_tokens_have_a_fixed_shape() {
        let token = new_publisher_token();
        assert!(is_publisher_token(&token), "{token}");
        assert!(!is_publisher_token("spk_short"));
        assert!(!is_publisher_token(&format!("spk_{}", "G".repeat(64))));
        assert_ne!(new_publisher_token(), token);
    }

    #[test]
    fn the_admin_credential_matches_only_its_secret() {
        let secret = "a-long-operator-secret-that-is-fine-0123456789";
        let admin = AdminCredential::from_secret(secret).unwrap();
        assert!(admin.matches(secret));
        assert!(!admin.matches("a-long-operator-secret-that-is-fine-0123456780"));
        assert!(AdminCredential::from_secret("short").is_err());
        assert!(!format!("{admin:?}").contains(secret));
    }

    #[test]
    fn the_store_key_debug_hides_the_secret_and_signs_as_the_pair_does() {
        let key = StoreKey::from_seed(&[7; 32]);
        assert!(format!("{key:?}").contains("redacted"));
        let signature = <StoreKey as RevocationSigner>::sign(&key, b"message");
        key.public().verify(b"message", &signature).unwrap();
        assert_eq!(StoreSigner::key_id(&key), key.id().as_str());
    }
}
