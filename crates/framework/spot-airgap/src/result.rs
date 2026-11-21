//! Result bundles: what the air-gapped agent carries back out.
//!
//! After an agent has applied the commands it was given, it produces a [`ResultBundle`] with the
//! outcome of each command and a segment of its audit chain, signs it with its own key, and the
//! operator carries it out on the same medium. On the console side [`import_result`] verifies
//! the signature against the console's key anchor, checks the audit segment, and records the
//! bundle **idempotently**: re-importing the same bundle id with the same content is a no-op,
//! and the same id with different content is a conflict.
//!
//! The agent's signature is a detached Ed25519 signature over the bundle's canonical bytes,
//! verified with `spot-updater`'s key anchor and revocation rules; no cryptography is written
//! here.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use spot_signed_command::Signer;
use spot_updater::{Digest, KeyId as UpdaterKeyId, UpdateSignature};

use crate::error::AirgapError;
use crate::verify::{AgentTrust, verify_detached};

/// The `format` field of every result bundle document.
pub const RESULT_BUNDLE_FORMAT: &str = "spot-result-bundle";
/// The format version this module writes.
pub const RESULT_BUNDLE_VERSION: u32 = 1;
/// The domain prefix of the bytes the agent signs.
pub const RESULT_DOMAIN: &str = "SPOT-AIRGAP-RESULT-V1\n";

/// What happened to one command on the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultOutcome {
    /// The command ran and succeeded.
    Applied,
    /// The command was refused (bad signature, replay, policy) and did not run.
    Refused,
    /// The command ran and failed.
    Failed,
}

/// The outcome of one command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    /// The command's id.
    pub command_id: String,
    /// The operation the command named.
    pub op: String,
    /// What happened.
    pub outcome: ResultOutcome,
    /// A human-readable detail, never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A segment of the agent's audit chain, carried out so the console can verify it.
///
/// The segment's raw bytes travel base64-encoded; `head_hex` is their SHA-256, so the console
/// recomputes it and refuses a segment that does not match. The sequence bounds name the range
/// the segment covers. This crate does not rebuild the audit chain; it carries the segment and
/// its digest, and the audit layer verifies the chain itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSegment {
    /// The site the segment belongs to.
    pub site: String,
    /// The first entry's sequence number.
    pub first_seq: u64,
    /// The last entry's sequence number.
    pub last_seq: u64,
    /// The lower-case hex SHA-256 of `bytes`.
    pub head_hex: String,
    /// The segment's raw bytes, standard base64.
    pub bytes: String,
}

impl AuditSegment {
    /// Builds a segment from its raw bytes, computing the head.
    #[must_use]
    pub fn of(site: impl Into<String>, first_seq: u64, last_seq: u64, bytes: &[u8]) -> Self {
        Self {
            site: site.into(),
            first_seq,
            last_seq,
            head_hex: hex_of(&Sha256::digest(bytes)),
            bytes: STANDARD.encode(bytes),
        }
    }

    /// Checks the segment: the sequence bounds are ordered and the head is the bytes' SHA-256.
    ///
    /// # Errors
    /// [`AirgapError::AuditSegmentMalformed`] when the base64 does not decode, the head does not
    /// match, or the bounds are inverted.
    pub fn verify(&self) -> Result<(), AirgapError> {
        if self.first_seq > self.last_seq {
            return Err(AirgapError::AuditSegmentMalformed {
                detail: "the first sequence number is after the last".to_owned(),
            });
        }
        let bytes =
            STANDARD
                .decode(&self.bytes)
                .map_err(|error| AirgapError::AuditSegmentMalformed {
                    detail: error.to_string(),
                })?;
        let head = hex_of(&Sha256::digest(&bytes));
        if head != self.head_hex {
            return Err(AirgapError::AuditSegmentMalformed {
                detail: "the segment head does not match its bytes".to_owned(),
            });
        }
        Ok(())
    }
}

/// What the agent reports back after applying a command bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultBundle {
    /// A unique id for this bundle, so a re-import is recognised.
    pub id: String,
    /// The agent that produced it.
    pub agent_id: String,
    /// When it was produced (UTC seconds).
    pub produced_at: i64,
    /// The outcome of every command.
    pub results: Vec<CommandResult>,
    /// The audit-chain segment.
    pub audit: AuditSegment,
}

impl ResultBundle {
    /// The canonical JSON of every field: sorted keys, no whitespace, integers only.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.signed_fields()).unwrap_or_default()
    }

    /// The bytes the agent signs: [`RESULT_DOMAIN`] and the canonical bytes.
    #[must_use]
    pub fn signing_message(&self) -> Vec<u8> {
        let mut message = RESULT_DOMAIN.as_bytes().to_vec();
        message.extend_from_slice(&self.canonical_bytes());
        message
    }

    /// The bundle's digest: the SHA-256 of its signing message, used to recognise a re-import.
    #[must_use]
    pub fn digest(&self) -> Digest {
        Digest::of(&self.signing_message())
    }

    /// The bundle's JSON file form.
    ///
    /// # Errors
    /// [`AirgapError::ResultMalformed`] when serialisation fails (never for this shape).
    pub fn to_json(&self) -> Result<String, AirgapError> {
        serde_json::to_string_pretty(self).map_err(|error| AirgapError::ResultMalformed {
            detail: error.to_string(),
        })
    }

    /// Parses a result bundle, checking every field.
    ///
    /// # Errors
    /// [`AirgapError::ResultMalformed`] when the document does not parse or a field is missing.
    pub fn from_json(bytes: &[u8]) -> Result<Self, AirgapError> {
        serde_json::from_slice(bytes).map_err(|error| AirgapError::ResultMalformed {
            detail: error.to_string(),
        })
    }

    /// The bundle as the sorted map the canonical bytes are built from.
    fn signed_fields(&self) -> BTreeMap<&str, Value> {
        let results = self
            .results
            .iter()
            .map(|result| {
                let mut fields = BTreeMap::new();
                fields.insert("command_id", Value::from(result.command_id.as_str()));
                fields.insert("detail", optional(result.detail.as_deref()));
                fields.insert("op", Value::from(result.op.as_str()));
                fields.insert("outcome", Value::from(outcome_name(result.outcome)));
                canonical_object(fields)
            })
            .collect::<Vec<_>>();
        let audit = {
            let mut fields = BTreeMap::new();
            fields.insert("bytes", Value::from(self.audit.bytes.as_str()));
            fields.insert("first_seq", Value::from(self.audit.first_seq));
            fields.insert("head", Value::from(self.audit.head_hex.as_str()));
            fields.insert("last_seq", Value::from(self.audit.last_seq));
            fields.insert("site", Value::from(self.audit.site.as_str()));
            canonical_object(fields)
        };
        let mut fields = BTreeMap::new();
        fields.insert("agent_id", Value::from(self.agent_id.as_str()));
        fields.insert("audit", audit);
        fields.insert("id", Value::from(self.id.as_str()));
        fields.insert("produced_at", Value::from(self.produced_at));
        fields.insert("results", Value::from(results));
        fields
    }
}

/// Signs a result bundle with the agent's own key, producing the detached signature document.
///
/// # Errors
/// [`AirgapError::SignerRefused`] when the agent's key id is not its usual shape.
pub fn sign_result(
    bundle: &ResultBundle,
    signer: &dyn Signer,
) -> Result<UpdateSignature, AirgapError> {
    let key_id = UpdaterKeyId::parse(signer.key_id().as_str()).map_err(|error| {
        AirgapError::SignerRefused {
            detail: error.to_string(),
        }
    })?;
    let signature = signer.sign(&bundle.signing_message());
    Ok(UpdateSignature::from_parts(key_id, signature.to_bytes()))
}

/// Where the console keeps the result bundles it has already recorded.
pub trait ResultSink {
    /// The digest already recorded for `id`, if any.
    ///
    /// # Errors
    /// The sink's own failure mode.
    fn contains(&self, id: &str) -> Result<Option<Digest>, AirgapError>;

    /// Records a bundle under its id.
    ///
    /// # Errors
    /// The sink's own failure mode.
    fn record(&mut self, id: &str, digest: Digest, bundle: ResultBundle)
    -> Result<(), AirgapError>;
}

/// What an import of a result bundle did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultImport {
    /// The bundle was verified and recorded.
    Imported,
    /// The bundle was already recorded with the same content.
    AlreadyImported,
}

/// Verifies a result bundle and records it idempotently. The signature is checked against the
/// console's key anchor, the audit segment is checked, and the bundle's id decides a re-import.
///
/// # Errors
/// [`AirgapError::ResultSignatureInvalid`], [`AirgapError::AuditSegmentMalformed`],
/// [`AirgapError::ResultConflict`] or the sink's failure.
pub fn import_result(
    sink: &mut dyn ResultSink,
    bundle: &ResultBundle,
    signature: Option<&UpdateSignature>,
    trust: &AgentTrust<'_>,
) -> Result<ResultImport, AirgapError> {
    verify_detached(&bundle.signing_message(), signature, trust).map_err(|error| match error {
        AirgapError::Unsigned => AirgapError::ResultUnsigned,
        AirgapError::SignatureInvalid => AirgapError::ResultSignatureInvalid,
        other => other,
    })?;
    bundle.audit.verify()?;
    let digest = bundle.digest();
    match sink.contains(&bundle.id)? {
        Some(recorded) if recorded == digest => Ok(ResultImport::AlreadyImported),
        Some(_) => Err(AirgapError::ResultConflict {
            id: bundle.id.clone(),
        }),
        None => {
            sink.record(&bundle.id, digest, bundle.clone())?;
            Ok(ResultImport::Imported)
        }
    }
}

/// A nested object, built in sorted key order.
fn canonical_object(fields: BTreeMap<&str, Value>) -> Value {
    serde_json::to_value(fields).unwrap_or(Value::Null)
}

/// A `null` for `None`, a string for `Some`, so the canonical bytes always carry the key.
fn optional(value: Option<&str>) -> Value {
    match value {
        Some(text) => Value::from(text),
        None => Value::Null,
    }
}

/// The lower-case name of an outcome, matching its serde spelling.
fn outcome_name(outcome: ResultOutcome) -> &'static str {
    match outcome {
        ResultOutcome::Applied => "applied",
        ResultOutcome::Refused => "refused",
        ResultOutcome::Failed => "failed",
    }
}

/// Lower-case hex of some bytes.
fn hex_of(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
