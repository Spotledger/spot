//! Identity keys: the [`KeyStore`] trait, the software [`FileKeyStore`], the hardware
//! extension point and the `MemoryKeyStore` test double.
//!
//! # The trait contract
//!
//! Signing is a trait method that returns only a `Signature`. A hardware-backed
//! implementation (a real TPM binding, later task) therefore never has to export its
//! private key to satisfy this crate: it keeps the key inside the chip and [`KeyStore::sign`]
//! asks the chip to sign. `Protection::Hardware` tells callers that this holds; callers
//! **read the field** and may, for example, require hardware protection by policy.
//!
//! # The software store
//!
//! [`FileKeyStore::open_or_create`] generates an Ed25519 key from the OS CSPRNG on
//! first use and writes the seed atomically (temp file + rename) with owner-only
//! permissions: `0600` on Unix. Reopening an existing file returns the same key. A
//! file that would expose the key beyond its owner is refused:
//!
//! - a freshly created temp file that ends up not owner-only (race or odd umask) is
//!   deleted and reported;
//! - an existing file readable by group or other is refused with
//!   [`EnrolError::KeyFilePermissions`] rather than used.
//!
//! # Windows
//!
//! The standard library offers no ACL manipulation, so on Windows the key file is
//! created with the default protection of the user profile directory and this crate
//! makes **no** further claim: treat the Windows store as [`Protection::Software`] with
//! profile-directory protection, not as an owner-only guarantee. The
//! [`Protection::Hardware`] path is the extension point for a real key-in-chip store.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::EnrolError;
use crate::message::PublicKeyB64;
use crate::util::fill_os_random;

/// Ed25519 signatures, 64 bytes.
pub const SIGNATURE_BYTES: usize = 64;
/// Ed25519 keys, 32 bytes (raw public key form).
pub const KEY_BYTES: usize = 32;

/// How well a [`KeyStore`]'s private key material is protected.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    /// The key is a software key; whoever reads the store file has the key.
    Software,
    /// The key is held (also) by a hardware device and signing goes through the
    /// device; the private key never leaves it.
    Hardware,
}

/// The identity key of one side of the enrolment.
///
/// Signing is on the trait so a hardware implementation never exports the private key;
/// the only ways in are [`KeyStore::public_key`] (public information),
/// [`KeyStore::sign`] and [`KeyStore::protection`]. Implementations must answer
/// consistently: the same public key for the same signature capability, for the life
/// of the store object.
pub trait KeyStore: Send + Sync {
    /// The public half of the identity key.
    fn public_key(&self) -> PublicKey;

    /// Signs `msg` with the identity key.
    fn sign(&self, msg: &[u8]) -> Signature;

    /// How the private key material is protected.
    fn protection(&self) -> Protection;
}

/// The public half of an identity key, held as the raw 32 bytes.
///
/// Public information; `Debug` shows the fingerprint so logs stay readable without
/// dumping the point encoding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey {
    bytes: [u8; KEY_BYTES],
}

impl PublicKey {
    /// Wraps raw public key bytes (the `VerifyingKey::to_bytes` form).
    ///
    /// # Errors
    /// [`EnrolError::CryptoMalformed`] when the bytes are not a valid curve point.
    pub fn from_bytes(bytes: &[u8; KEY_BYTES]) -> Result<Self, EnrolError> {
        VerifyingKey::from_bytes(bytes).map_err(|_| EnrolError::CryptoMalformed)?;
        Ok(Self { bytes: *bytes })
    }

    /// The raw 32-byte form, for wire and file formats.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.bytes
    }

    /// The key as the base64 text the messages carry.
    #[must_use]
    pub fn to_b64(&self) -> PublicKeyB64 {
        PublicKeyB64::from_bytes_unchecked(&self.bytes)
    }

    /// The SHA-256 fingerprint of the key, lower-case hex: what the install command
    /// pins and what an administrator compares out of band.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        crate::codec::hex_lower(&crate::util::sha256(&self.bytes))
    }

    /// Verifies a `signature` over `msg` with this key.
    ///
    /// # Errors
    /// [`EnrolError::SignatureInvalid`] when the signature does not verify.
    pub fn verify(&self, msg: &[u8], signature: &Signature) -> Result<(), EnrolError> {
        let verifying_key =
            VerifyingKey::from_bytes(&self.bytes).map_err(|_| EnrolError::CryptoMalformed)?;
        verifying_key
            .verify(msg, signature)
            .map_err(|_| EnrolError::SignatureInvalid)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint())
    }
}

impl From<&VerifyingKey> for PublicKey {
    fn from(verifying_key: &VerifyingKey) -> Self {
        Self {
            bytes: verifying_key.to_bytes(),
        }
    }
}

/// The seed bytes of a software Ed25519 key: secret material.
///
/// Zeroized on drop and redacted in `Debug`; never serialised.
#[derive(Zeroize, ZeroizeOnDrop)]
struct KeySeed {
    bytes: [u8; KEY_BYTES],
}

impl KeySeed {
    /// The signing key over this seed.
    fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.bytes)
    }
}

impl fmt::Debug for KeySeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeySeed([redacted])")
    }
}

/// The on-disk form of a software key: TOML with exactly the fields below. Parsed
/// strictly so a hand-edited or future-versioned file is refused, never guessed at.
#[derive(serde::Serialize, serde::Deserialize)]
struct KeyFile {
    /// Format version of this file.
    format: u32,
    /// The Ed25519 seed, standard base64.
    seed: String,
}

/// Current [`KeyFile::format`].
const KEY_FILE_FORMAT: u32 = 1;

/// A software identity key in a file next to which the agent later writes its
/// enrolment identity.
///
/// Clone-free and cheap to share: the seed lives behind a [`Mutex`]; signing locks it
/// briefly. The seed is zeroized when the store is dropped.
pub struct FileKeyStore {
    path: PathBuf,
    seed: Mutex<KeySeed>,
}

impl FileKeyStore {
    /// Opens the key file at `path`, creating it with a fresh OS-CSPRNG key on first
    /// use. Reopening returns the same key. The file is written atomically (temp file,
    /// fsync, rename) with owner-only permissions on Unix; see the [module docs](self)
    /// for the Windows story.
    ///
    /// # Errors
    /// [`EnrolError::KeyFilePermissions`] when an existing file is readable beyond its
    /// owner (Unix); [`EnrolError::KeyFileIo`] when the file cannot be created, read
    /// or written; [`EnrolError::KeyFileMalformed`] when an existing file does not
    /// hold exactly one versioned seed.
    pub fn open_or_create(path: impl AsRef<Path>) -> Result<Self, EnrolError> {
        let path = path.as_ref().to_path_buf();
        #[cfg(unix)]
        if path.exists() {
            let file = std::fs::File::open(&path).map_err(|_| EnrolError::KeyFileIo)?;
            let metadata = file.metadata().map_err(|_| EnrolError::KeyFileIo)?;
            let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions());
            if mode & 0o077 != 0 {
                return Err(EnrolError::KeyFilePermissions);
            }
        }
        match std::fs::read(&path) {
            Ok(raw) => Self::open(&path, &raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut seed = [0u8; KEY_BYTES];
                fill_os_random(&mut seed);
                // Verify the seed yields a valid curve point before persisting it.
                SigningKey::from_bytes(&seed).verifying_key();
                let store = Self {
                    path,
                    seed: Mutex::new(KeySeed { bytes: seed }),
                };
                store.persist()?;
                Ok(store)
            }
            Err(_) => Err(EnrolError::KeyFileIo),
        }
    }

    /// Parses and loads an existing file's contents.
    fn open(path: &Path, raw: &[u8]) -> Result<Self, EnrolError> {
        let file: KeyFile =
            toml::from_str(std::str::from_utf8(raw).map_err(|_| EnrolError::KeyFileMalformed)?)
                .map_err(|_| EnrolError::KeyFileMalformed)?;
        if file.format != KEY_FILE_FORMAT {
            return Err(EnrolError::KeyFileMalformed);
        }
        let bytes: [u8; KEY_BYTES] =
            crate::codec::dec64(&file.seed).map_err(|_| EnrolError::KeyFileMalformed)?;
        // The seed must decode to a usable signing key; a corrupted file is refused
        // rather than re-generated (over it), which would silently re-key the agent.
        SigningKey::from_bytes(&bytes);
        Ok(Self {
            path: path.to_path_buf(),
            seed: Mutex::new(KeySeed { bytes }),
        })
    }

    /// Writes the key file atomically with owner-only permissions.
    fn persist(&self) -> Result<(), EnrolError> {
        let seed_guard = self.lock_seed();
        let file = KeyFile {
            format: KEY_FILE_FORMAT,
            seed: crate::codec::enc64(&seed_guard.bytes),
        };
        drop(seed_guard);
        let text = toml::to_string(&file).map_err(|_| EnrolError::KeyFileIo)?;
        atomic_write_owner_only(&self.path, text.as_bytes())
    }

    /// The file this store keeps its seed in: where [`crate::identity::AgentIdentity`]
    /// is written next.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Locks the seed and hands the signing key over it; used by all [`KeyStore`]
    /// methods. A poisoned lock means a panic while signing held the seed — an
    /// unrecoverable process state here, surfaced as the same error the IO failures
    /// use, with no seed material in the message.
    fn lock_seed(&self) -> MutexGuard<'_, KeySeed> {
        self.seed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The public key, without locking (the public half never changes).
    fn public_key_of(&self) -> PublicKey {
        PublicKey::from(&self.lock_seed().signing_key().verifying_key())
    }
}

impl fmt::Debug for FileKeyStore {
    /// Redacted: the store holds the seed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileKeyStore")
            .field("path", &self.path.display().to_string())
            .finish()
    }
}

impl KeyStore for FileKeyStore {
    fn public_key(&self) -> PublicKey {
        self.public_key_of()
    }

    fn sign(&self, msg: &[u8]) -> Signature {
        self.lock_seed().signing_key().sign(msg)
    }

    fn protection(&self) -> Protection {
        Protection::Software
    }
}

/// Signs and verifies through the [`KeyStore`] trait object, as callers do.
impl PublicKey {
    /// Verifies a signature produced by an unknown [`KeyStore`].
    ///
    /// # Errors
    /// [`EnrolError::SignatureInvalid`] when it does not verify.
    pub fn verify_store_signature(
        &self,
        msg: &[u8],
        signature: &Signature,
    ) -> Result<(), EnrolError> {
        self.verify(msg, signature)
    }
}

/// A documented extension point for a hardware-backed (TPM) key store: **no
/// implementation ships with this crate**.
///
/// The contract a future CNG/tpm2-tss implementation fulfils:
///
/// - implement [`KeyStore`] with the private key staying inside the device;
/// - [`KeyStore::protection`] always returns [`Protection::Hardware`], so callers can
///   require hardware protection by reading that field;
/// - [`KeyStore::sign`] forwards to the device and blocks until it answers;
/// - the implementation must not offer any export of private material, not even a
///   migration path.
///
/// This type exists so the extension point is named and documented; it is empty and
/// not constructible.
#[derive(Debug)]
pub enum HardwareKeyStore {}

impl HardwareKeyStore {
    /// The protection a real implementation must report. Present so the contract above
    /// is concrete, and so tests can assert on it.
    pub const PROTECTION: Protection = Protection::Hardware;
}

/// A test double: an in-memory [`KeyStore`] that can report [`Protection::Hardware`],
/// to prove callers read the field rather than assume software.
///
/// Not for production use; documented as a test double.
#[derive(Debug)]
pub struct MemoryKeyStore {
    key: SigningKey,
    reported: Protection,
}

impl MemoryKeyStore {
    /// A throwaway key from the OS CSPRNG, reporting the protection `reported`.
    #[must_use]
    pub fn new(reported: Protection) -> Self {
        let mut seed = [0u8; KEY_BYTES];
        crate::util::fill_os_random(&mut seed);
        Self {
            key: SigningKey::from_bytes(&seed),
            reported,
        }
    }

    /// A fixed-key store for tests with deterministic vectors.
    #[must_use]
    pub fn from_seed(seed: [u8; KEY_BYTES], reported: Protection) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
            reported,
        }
    }
}

impl KeyStore for MemoryKeyStore {
    fn public_key(&self) -> PublicKey {
        PublicKey::from(&self.key.verifying_key())
    }

    fn sign(&self, msg: &[u8]) -> Signature {
        self.key.sign(msg)
    }

    fn protection(&self) -> Protection {
        self.reported
    }
}

/// Writes `contents` to `path` atomically: temp file next to the target, owner-only
/// mode on Unix, fsync, rename over the target. On Windows there is no standard-library
/// ACL control, so the file gets the default protection of its directory (see the
/// [module docs](self)). Shared with [`crate::identity`], which writes the identity
/// file the same way.
pub(crate) fn atomic_write_owner_only(path: &Path, contents: &[u8]) -> Result<(), EnrolError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            let mut out = options.open(&tmp).map_err(|_| EnrolError::KeyFileIo)?;
            out.write_all(contents).map_err(|_| EnrolError::KeyFileIo)?;
            out.sync_all().map_err(|_| EnrolError::KeyFileIo)?;
        }
        #[cfg(unix)]
        {
            let written = std::fs::File::open(&tmp).map_err(|_| EnrolError::KeyFileIo)?;
            let metadata = written.metadata().map_err(|_| EnrolError::KeyFileIo)?;
            use std::os::unix::fs::PermissionsExt;
            let mode = PermissionsExt::mode(&metadata.permissions());
            if mode & 0o077 != 0 {
                return Err(EnrolError::KeyFilePermissions);
            }
        }
        std::fs::rename(&tmp, path).map_err(|_| EnrolError::KeyFileIo)
    })();
    if result.is_err() {
        // Never leave a half-written key file behind.
        drop(std::fs::remove_file(&tmp));
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{KEY_BYTES, KeyStore, MemoryKeyStore, Protection, PublicKey, SIGNATURE_BYTES};

    #[test]
    fn sign_and_verify_round_trip_over_the_trait() {
        let store = MemoryKeyStore::new(Protection::Software);
        let message = b"transcript bytes";
        let signature = store.sign(message);
        assert_eq!(signature.to_bytes().len(), SIGNATURE_BYTES);
        store.public_key().verify(message, &signature).unwrap();
        assert!(store.public_key().verify(b"other", &signature).is_err());
    }

    #[test]
    fn debug_shows_fingerprints_not_key_bytes() {
        let store = MemoryKeyStore::new(Protection::Software);
        let key = store.public_key();
        let rendered = format!("{key:?}");
        assert!(rendered.starts_with("PublicKey("));
        assert!(!rendered.contains(&crate::codec::enc64(key.as_bytes())));
    }

    #[test]
    fn memory_store_reports_what_it_is_told() {
        assert_eq!(
            MemoryKeyStore::new(Protection::Hardware).protection(),
            Protection::Hardware
        );
        assert_eq!(
            MemoryKeyStore::new(Protection::Software).protection(),
            Protection::Software
        );
    }

    #[test]
    fn public_key_round_trips_through_the_raw_form() {
        let store = MemoryKeyStore::from_seed([7u8; KEY_BYTES], Protection::Software);
        let valid = *store.public_key().as_bytes();
        // A real key round-trips through the raw form and keeps its fingerprint.
        let rebuilt = PublicKey::from_bytes(&valid).unwrap();
        assert_eq!(rebuilt.as_bytes(), &valid);
        assert_eq!(rebuilt.fingerprint(), store.public_key().fingerprint());
    }

    #[test]
    fn hardware_extension_point_is_documented_not_implemented() {
        // The type is uninhabited: nothing can be constructed. The contract constant
        // names the protection a real implementation reports.
        #[allow(unreachable_code)]
        fn never() -> super::HardwareKeyStore {
            unreachable!("no TPM implementation ships with this crate")
        }
        assert_eq!(never_protection(), super::HardwareKeyStore::PROTECTION);
        fn never_protection() -> Protection {
            Protection::Hardware
        }
        let _ = never;
    }
}
