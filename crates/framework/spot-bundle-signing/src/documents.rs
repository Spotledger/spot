//! The JSON file forms of certificates, bundle signatures and revocation lists.
//!
//! Each file names its format and version, and refuses unknown fields. Keys and signatures are
//! standard base64; key ids are their hex form. Parsing checks every field's shape, so the rest
//! of the crate only sees valid values. The files are for shipping and storage; the signed bytes
//! are the canonical ones in [`crate::canonical`], not these files.

use serde::{Deserialize, Serialize};

use crate::canonical::{b64, unb64};
use crate::certificate::{PublisherCertificate, SignedCertificate};
use crate::error::SigningError;
use crate::keys::{KeyId, PublicKey, SIGNATURE_BYTES};
use crate::revocation::{
    RevocationBody, RevocationList, RevokedBundle, RevokedKey, RevokedPublisher,
};
use crate::signature::BundleSignature;

const FORMAT_VERSION: u32 = 1;

fn malformed(document: &'static str, detail: impl Into<String>) -> SigningError {
    SigningError::DocumentMalformed {
        document,
        detail: detail.into(),
    }
}

fn check_format(
    document: &'static str,
    format: &str,
    version: u32,
    expected: &str,
) -> Result<(), SigningError> {
    if format != expected {
        return Err(malformed(
            document,
            format!("the format is `{format}`, not `{expected}`"),
        ));
    }
    if version != FORMAT_VERSION {
        return Err(malformed(
            document,
            format!("version {version} is not supported"),
        ));
    }
    Ok(())
}

fn signature_of(text: &str) -> Result<[u8; SIGNATURE_BYTES], SigningError> {
    let bytes = unb64(text).ok_or(SigningError::SignatureMalformed)?;
    <[u8; SIGNATURE_BYTES]>::try_from(bytes.as_slice())
        .map_err(|_| SigningError::SignatureMalformed)
}

fn key_of(text: &str) -> Result<PublicKey, SigningError> {
    let bytes = unb64(text).ok_or(SigningError::KeyMalformed)?;
    PublicKey::from_bytes(&bytes)
}

// --- certificates ----------------------------------------------------------------------------

const CERT: &str = "publisher certificate";
const CERT_FORMAT: &str = "spot-publisher-certificate";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CertificateDoc {
    format: String,
    version: u32,
    publisher_id: String,
    display_name: String,
    public_key: String,
    valid_from: i64,
    valid_to: i64,
    issued_by: String,
    serial: u64,
    signature: String,
}

impl SignedCertificate {
    /// The certificate's JSON file form.
    #[must_use]
    pub fn to_json(&self) -> String {
        let doc = CertificateDoc {
            format: CERT_FORMAT.to_owned(),
            version: FORMAT_VERSION,
            publisher_id: self.body.publisher_id.clone(),
            display_name: self.body.display_name.clone(),
            public_key: b64(self.body.key.as_bytes()),
            valid_from: self.body.valid_from,
            valid_to: self.body.valid_to,
            issued_by: self.body.issued_by.as_str().to_owned(),
            serial: self.body.serial,
            signature: b64(self.signature()),
        };
        serde_json::to_string_pretty(&doc).unwrap_or_default()
    }

    /// Parses a certificate file.
    ///
    /// # Errors
    /// [`SigningError::DocumentMalformed`], [`SigningError::KeyMalformed`] or
    /// [`SigningError::SignatureMalformed`] for a field that does not parse.
    pub fn from_json(bytes: &[u8]) -> Result<Self, SigningError> {
        let doc: CertificateDoc =
            serde_json::from_slice(bytes).map_err(|e| malformed(CERT, e.to_string()))?;
        check_format(CERT, &doc.format, doc.version, CERT_FORMAT)?;
        let body = PublisherCertificate {
            publisher_id: doc.publisher_id,
            display_name: doc.display_name,
            key: key_of(&doc.public_key)?,
            valid_from: doc.valid_from,
            valid_to: doc.valid_to,
            issued_by: KeyId::parse(&doc.issued_by)?,
            serial: doc.serial,
        };
        Ok(SignedCertificate::from_parts(
            body,
            signature_of(&doc.signature)?,
        ))
    }
}

// --- bundle signatures ----------------------------------------------------------------------

const SIG: &str = "bundle signature";
const SIG_FORMAT: &str = "spot-bundle-signature";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureDoc {
    format: String,
    version: u32,
    app_id: String,
    app_version: String,
    publisher_id: String,
    key_id: String,
    signed_at: i64,
    signature: String,
}

impl BundleSignature {
    /// The signature file's text form (`app.sig`).
    #[must_use]
    pub fn to_json(&self) -> String {
        let doc = SignatureDoc {
            format: SIG_FORMAT.to_owned(),
            version: FORMAT_VERSION,
            app_id: self.app_id.clone(),
            app_version: self.app_version.clone(),
            publisher_id: self.publisher_id.clone(),
            key_id: self.key_id.as_str().to_owned(),
            signed_at: self.signed_at,
            signature: b64(self.signature()),
        };
        serde_json::to_string_pretty(&doc).unwrap_or_default()
    }

    /// Parses a signature file.
    ///
    /// # Errors
    /// [`SigningError::DocumentMalformed`] or [`SigningError::SignatureMalformed`].
    pub fn from_json(bytes: &[u8]) -> Result<Self, SigningError> {
        let doc: SignatureDoc =
            serde_json::from_slice(bytes).map_err(|e| malformed(SIG, e.to_string()))?;
        check_format(SIG, &doc.format, doc.version, SIG_FORMAT)?;
        Ok(BundleSignature::from_parts(
            doc.app_id,
            doc.app_version,
            doc.publisher_id,
            KeyId::parse(&doc.key_id)?,
            doc.signed_at,
            signature_of(&doc.signature)?,
        ))
    }
}

// --- revocation lists -----------------------------------------------------------------------

const REV: &str = "revocation list";
const REV_FORMAT: &str = "spot-revocation-list";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokedKeyDoc {
    key_id: String,
    effective_from: i64,
    compromised: bool,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokedPublisherDoc {
    publisher_id: String,
    effective_from: i64,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokedBundleDoc {
    digest: String,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevocationDoc {
    format: String,
    version: u32,
    generation: u64,
    issued_at: i64,
    issued_by: String,
    keys: Vec<RevokedKeyDoc>,
    publishers: Vec<RevokedPublisherDoc>,
    bundles: Vec<RevokedBundleDoc>,
    signature: String,
}

impl RevocationList {
    /// The list's JSON file form.
    #[must_use]
    pub fn to_json(&self) -> String {
        let b = &self.body;
        let doc = RevocationDoc {
            format: REV_FORMAT.to_owned(),
            version: FORMAT_VERSION,
            generation: b.generation,
            issued_at: b.issued_at,
            issued_by: b.issued_by.as_str().to_owned(),
            keys: b
                .keys
                .iter()
                .map(|k| RevokedKeyDoc {
                    key_id: k.key_id.as_str().to_owned(),
                    effective_from: k.effective_from,
                    compromised: k.compromised,
                    reason: k.reason.clone(),
                })
                .collect(),
            publishers: b
                .publishers
                .iter()
                .map(|p| RevokedPublisherDoc {
                    publisher_id: p.publisher_id.clone(),
                    effective_from: p.effective_from,
                    reason: p.reason.clone(),
                })
                .collect(),
            bundles: b
                .bundles
                .iter()
                .map(|x| RevokedBundleDoc {
                    digest: x.digest_hex.clone(),
                    reason: x.reason.clone(),
                })
                .collect(),
            signature: b64(self.signature()),
        };
        serde_json::to_string_pretty(&doc).unwrap_or_default()
    }

    /// Parses a revocation list file.
    ///
    /// # Errors
    /// [`SigningError::DocumentMalformed`], [`SigningError::SignatureMalformed`] or a key id
    /// that does not parse.
    pub fn from_json(bytes: &[u8]) -> Result<Self, SigningError> {
        let doc: RevocationDoc =
            serde_json::from_slice(bytes).map_err(|e| malformed(REV, e.to_string()))?;
        check_format(REV, &doc.format, doc.version, REV_FORMAT)?;
        let keys = doc
            .keys
            .into_iter()
            .map(|k| {
                Ok(RevokedKey {
                    key_id: KeyId::parse(&k.key_id)?,
                    effective_from: k.effective_from,
                    compromised: k.compromised,
                    reason: k.reason,
                })
            })
            .collect::<Result<Vec<_>, SigningError>>()?;
        let body = RevocationBody {
            generation: doc.generation,
            issued_at: doc.issued_at,
            issued_by: KeyId::parse(&doc.issued_by)?,
            keys,
            publishers: doc
                .publishers
                .into_iter()
                .map(|p| RevokedPublisher {
                    publisher_id: p.publisher_id,
                    effective_from: p.effective_from,
                    reason: p.reason,
                })
                .collect(),
            bundles: doc
                .bundles
                .into_iter()
                .map(|x| RevokedBundle {
                    digest_hex: x.digest,
                    reason: x.reason,
                })
                .collect(),
        };
        Ok(RevocationList::from_parts(
            body,
            signature_of(&doc.signature)?,
        ))
    }
}
