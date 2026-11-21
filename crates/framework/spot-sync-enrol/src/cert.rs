//! Certificates as this crate sees them: the DER bytes, the SHA-256 fingerprint, the validity
//! and the public key, checked once at the door.
//!
//! A node's certificate must be an **ECDSA P-256** certificate (the key type of the node
//! identity and of the proof of possession). Validity is checked with a tolerance of
//! [`SKEW_SECONDS`] on either side and no more.

use sha2::{Digest, Sha256};
use x509_parser::prelude::FromDer as _;

use crate::error::{EnrolError, Result};

/// The clock skew tolerated at the edges of a certificate's validity (five minutes).
pub const SKEW_SECONDS: i64 = 300;
/// The largest certificate accepted, in bytes.
pub const MAX_CERT_BYTES: usize = 8 * 1024;

/// A certificate that passed the door.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCert {
    der: Vec<u8>,
    fingerprint: String,
    not_before: i64,
    not_after: i64,
    public_key: Vec<u8>,
}

/// The SHA-256 fingerprint of DER bytes, lower-case hex (the pinning key; public).
#[must_use]
pub fn fingerprint_of(der: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

impl ParsedCert {
    /// Parses `der` and checks it is valid at `now` (Unix seconds, within the skew).
    ///
    /// # Errors
    /// `sync_enrol.cert_invalid` for unreadable bytes, an oversize certificate, a validity window
    /// that does not include `now` (beyond the skew), or a key that is not ECDSA P-256.
    pub fn parse(der: &[u8], now: i64) -> Result<Self> {
        let parsed = Self::parse_any_time(der)?;
        if now + SKEW_SECONDS < parsed.not_before || now - SKEW_SECONDS > parsed.not_after {
            return Err(EnrolError::CertInvalid);
        }
        Ok(parsed)
    }

    /// Parses `der` without checking the validity window (a stored certificate).
    ///
    /// # Errors
    /// `sync_enrol.cert_invalid` for unreadable bytes, an oversize certificate or a key that is
    /// not ECDSA P-256.
    pub fn parse_any_time(der: &[u8]) -> Result<Self> {
        if der.is_empty() || der.len() > MAX_CERT_BYTES {
            return Err(EnrolError::CertInvalid);
        }
        let (rest, certificate) = x509_parser::certificate::X509Certificate::from_der(der)
            .map_err(|_| EnrolError::CertInvalid)?;
        if !rest.is_empty() {
            return Err(EnrolError::CertInvalid);
        }
        let key = certificate
            .tbs_certificate
            .subject_pki
            .subject_public_key
            .data
            .to_vec();
        // An uncompressed P-256 point: 0x04 followed by two 32-byte coordinates.
        if key.len() != 65 || key.first() != Some(&0x04) {
            return Err(EnrolError::CertInvalid);
        }
        let validity = certificate.validity();
        Ok(Self {
            fingerprint: fingerprint_of(der),
            der: der.to_vec(),
            not_before: validity.not_before.timestamp(),
            not_after: validity.not_after.timestamp(),
            public_key: key,
        })
    }

    /// The DER bytes.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The SHA-256 fingerprint, lower-case hex.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The start of validity, Unix seconds.
    #[must_use]
    pub fn not_before(&self) -> i64 {
        self.not_before
    }

    /// The end of validity, Unix seconds.
    #[must_use]
    pub fn not_after(&self) -> i64 {
        self.not_after
    }

    /// The uncompressed P-256 public key point.
    #[must_use]
    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }
}
