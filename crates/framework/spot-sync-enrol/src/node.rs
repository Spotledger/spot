//! The node side: its identity (an ECDSA P-256 key pair and a self-signed certificate it
//! generates itself, the key never leaving the node) and the proof of possession a rotation
//! carries.
//!
//! The private key is stored in a file in the node's data directory with mode `0600`; the node
//! **refuses to start** when that file is readable by its group or by others
//! (`sync_enrol.key_file_permissions`). A deployment with a key provider supplies the PKCS#8
//! bytes through [`NodeIdentity::from_parts`] instead of a file. The key never appears in a
//! `Debug` form, an error, a log or the audit record (the public fingerprint does).
//!
//! Generating a key pair needs entropy; `rcgen` draws it from the operating system's generator
//! through `ring`. That is the one place this crate touches a random source of its own, and it is
//! the node's own identity, not a value the hub or a test depends on.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, DnType, KeyPair};
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, UnparsedPublicKey,
};
use spot_sync_protocol::NodeId;
use time::{Duration, OffsetDateTime};

use crate::cert::{ParsedCert, fingerprint_of};
use crate::error::{EnrolError, Result};

/// The file the private key (PKCS#8 DER) is kept in.
pub const KEY_FILE: &str = "node.key";
/// The file the certificate (DER) is kept in.
pub const CERT_FILE: &str = "node.crt";
/// How long a generated certificate is valid: ten years (rotation is explicit).
pub const CERT_LIFETIME_DAYS: i64 = 3650;

/// A node's identity: its key pair and its certificate.
pub struct NodeIdentity {
    pkcs8: Vec<u8>,
    cert_der: Vec<u8>,
}

impl std::fmt::Debug for NodeIdentity {
    /// Shows the public fingerprint only, never the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

/// A proof that the sender holds the **current** key: an ECDSA signature over the node id, the
/// new certificate's fingerprint and the time, made with the current private key.
#[derive(Clone, PartialEq, Eq)]
pub struct ProofOfPossession {
    signature: Vec<u8>,
}

impl std::fmt::Debug for ProofOfPossession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofOfPossession").finish_non_exhaustive()
    }
}

impl ProofOfPossession {
    /// The signature bytes (public: a signature reveals nothing about the key).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.signature
    }

    /// A proof from received bytes.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { signature: bytes }
    }

    /// Verifies the proof against the node's current certificate.
    ///
    /// # Errors
    /// `sync_enrol.proof_invalid` when it does not verify; `sync_enrol.cert_invalid` for an
    /// unreadable certificate.
    pub fn verify(
        &self,
        current_cert_der: &[u8],
        node: &NodeId,
        new_fingerprint: &str,
        at: i64,
    ) -> Result<()> {
        let current = ParsedCert::parse_any_time(current_cert_der)?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, current.public_key())
            .verify(&message(node, new_fingerprint, at), &self.signature)
            .map_err(|_| EnrolError::ProofInvalid)
    }
}

/// The signed message: length-prefixed parts under a domain label, so no choice of node id or
/// fingerprint can collide with another.
fn message(node: &NodeId, new_fingerprint: &str, at: i64) -> Vec<u8> {
    let mut out = Vec::new();
    for part in [
        b"spot-sync-enrol rotate v1".as_slice(),
        node.as_str().as_bytes(),
        new_fingerprint.as_bytes(),
        at.to_string().as_bytes(),
    ] {
        out.extend_from_slice(&u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        out.extend_from_slice(part);
    }
    out
}

fn generate(node: &NodeId, now: i64) -> Result<(Vec<u8>, Vec<u8>)> {
    let invalid = || EnrolError::InvalidArgument {
        reason: "an identity could not be generated",
    };
    let key = KeyPair::generate().map_err(|_| invalid())?;
    let mut params =
        CertificateParams::new(vec![node.as_str().to_owned()]).map_err(|_| invalid())?;
    params
        .distinguished_name
        .push(DnType::CommonName, node.as_str());
    let start = OffsetDateTime::from_unix_timestamp(now).map_err(|_| invalid())?;
    params.not_before = start - Duration::days(1);
    params.not_after = start + Duration::days(CERT_LIFETIME_DAYS);
    let cert = params.self_signed(&key).map_err(|_| invalid())?;
    Ok((key.serialize_der(), cert.der().to_vec()))
}

impl NodeIdentity {
    /// An identity from stored parts (a key provider's PKCS#8 bytes and the certificate DER).
    ///
    /// # Errors
    /// `sync_enrol.cert_invalid` when the certificate is unreadable or the key is not an
    /// ECDSA P-256 key matching it.
    pub fn from_parts(pkcs8: Vec<u8>, cert_der: Vec<u8>) -> Result<Self> {
        let cert = ParsedCert::parse_any_time(&cert_der)?;
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng)
            .map_err(|_| EnrolError::CertInvalid)?;
        let public = ring::signature::KeyPair::public_key(&pair);
        if public.as_ref() != cert.public_key() {
            return Err(EnrolError::CertInvalid);
        }
        Ok(Self { pkcs8, cert_der })
    }

    /// Loads the identity from `dir`, or generates and stores one on first start. `now` (Unix
    /// seconds) places the new certificate's validity.
    ///
    /// # Errors
    /// `sync_enrol.key_file_permissions` when the key file is readable by its group or others or
    /// is not a plain file; `sync_enrol.cert_invalid` for unreadable stored parts;
    /// `sync_enrol.store_failed` when the files cannot be read or written.
    pub fn load_or_create(dir: &Path, node: &NodeId, now: i64) -> Result<Self> {
        let key_path = dir.join(KEY_FILE);
        let cert_path = dir.join(CERT_FILE);
        if key_path.exists() {
            check_key_file(&key_path)?;
            let pkcs8 = read(&key_path)?;
            let cert = read(&cert_path)?;
            return Self::from_parts(pkcs8, cert);
        }
        let (pkcs8, cert) = generate(node, now)?;
        fs::create_dir_all(dir).map_err(|_| io_error())?;
        write_key(&key_path, &pkcs8)?;
        fs::write(&cert_path, &cert).map_err(|_| io_error())?;
        Self::from_parts(pkcs8, cert)
    }

    /// Generates a replacement identity (a new key pair and certificate) for a rotation; nothing
    /// is stored until the caller does so after the hub accepted it.
    ///
    /// # Errors
    /// `sync_enrol.invalid_argument` when generation fails.
    pub fn generate_replacement(node: &NodeId, now: i64) -> Result<Self> {
        let (pkcs8, cert) = generate(node, now)?;
        Self::from_parts(pkcs8, cert)
    }

    /// Stores this identity in `dir` (after a rotation was accepted), replacing the old files.
    ///
    /// # Errors
    /// `sync_enrol.store_failed`.
    pub fn store(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).map_err(|_| io_error())?;
        write_key(&dir.join(KEY_FILE), &self.pkcs8)?;
        fs::write(dir.join(CERT_FILE), &self.cert_der).map_err(|_| io_error())
    }

    /// The certificate's DER bytes (public).
    #[must_use]
    pub fn certificate_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// The certificate's SHA-256 fingerprint (public).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.cert_der)
    }

    /// The PKCS#8 private key bytes, for a TLS client configuration. Never log or echo them.
    #[must_use]
    pub fn private_key_pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// Signs the proof of possession for rotating to `new_fingerprint` at `at`, with this
    /// (current) key.
    ///
    /// # Errors
    /// `sync_enrol.proof_invalid` when signing fails.
    pub fn prove_rotation(
        &self,
        node: &NodeId,
        new_fingerprint: &str,
        at: i64,
    ) -> Result<ProofOfPossession> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &self.pkcs8, &rng)
            .map_err(|_| EnrolError::ProofInvalid)?;
        let signature = pair
            .sign(&rng, &message(node, new_fingerprint, at))
            .map_err(|_| EnrolError::ProofInvalid)?;
        Ok(ProofOfPossession {
            signature: signature.as_ref().to_vec(),
        })
    }
}

fn io_error() -> EnrolError {
    EnrolError::StoreFailed {
        reason: "an identity file could not be read or written",
    }
}

fn read(path: &PathBuf) -> Result<Vec<u8>> {
    fs::read(path).map_err(|_| io_error())
}

/// Refuses a key file that is not a plain file owned for reading by its owner only.
fn check_key_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| io_error())?;
    if !metadata.is_file() {
        return Err(EnrolError::KeyFilePermissions);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(EnrolError::KeyFilePermissions);
        }
    }
    Ok(())
}

/// Writes the key file with mode `0600` from the start (never world-readable, even briefly).
fn write_key(path: &Path, pkcs8: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| io_error())?;
    file.write_all(pkcs8).map_err(|_| io_error())?;
    file.sync_all().map_err(|_| io_error())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // A file that already existed keeps its old mode through `create`: set it explicitly.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|_| io_error())?;
    }
    Ok(())
}
