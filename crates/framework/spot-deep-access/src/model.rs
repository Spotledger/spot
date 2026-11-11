//! Request, consent and immutable credential data.
use crate::DeepError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_iam_access::stepup::AuthMethod;
use spot_signed_command::KeyId;
use spot_support_session::{Scope, SessionGrant, SessionSpec, Subject, Tier};
use std::collections::{BTreeMap, BTreeSet};

/// A SHA-256 commitment.
pub type Hash = [u8; 32];
/// A request is data and conveys no authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessRequest {
    /// Caller-assigned request identifier.
    pub id: String,
    /// Support-side requester, matching the session subject.
    pub requester: Subject,
    /// Customer-readable reason.
    pub reason: String,
    /// Support ticket reference.
    pub ticket_ref: String,
    /// The existing support-session scope and window model.
    pub spec: SessionSpec,
    /// Request creation time, Unix seconds.
    pub requested_at: i64,
}

/// Domain-separated canonical JSON, recursively sorted by object key.
/// The signed-command canonical helper is private. This writer uses the same
/// serde_json string escaping and integer spellings, and refuses all floats.
pub(crate) fn canonical<T: Serialize>(tag: &[u8], value: &T) -> Result<Vec<u8>, DeepError> {
    fn sort(value: serde_json::Value) -> Result<serde_json::Value, DeepError> {
        use serde_json::Value;
        Ok(match value {
            Value::Number(ref n) if n.is_f64() => return Err(DeepError::InvalidRequest),
            Value::Object(map) => {
                let ordered: BTreeMap<_, _> = map.into_iter().collect();
                let mut result = serde_json::Map::new();
                for (key, value) in ordered {
                    result.insert(key, sort(value)?);
                }
                Value::Object(result)
            }
            Value::Array(values) => {
                Value::Array(values.into_iter().map(sort).collect::<Result<_, _>>()?)
            }
            other => other,
        })
    }
    let value = serde_json::to_value(value).map_err(|_| DeepError::InvalidRequest)?;
    let mut out = tag.to_vec();
    out.extend(serde_json::to_vec(&sort(value)?).map_err(|_| DeepError::InvalidRequest)?);
    Ok(out)
}

pub(crate) fn hash(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}

impl AccessRequest {
    /// The complete request, including SessionSpec, under SPOT-DEEP-REQUEST-V1.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, DeepError> {
        canonical(b"SPOT-DEEP-REQUEST-V1\n", self)
    }
    /// Commitment to the exact requested SessionSpec and its ticket.
    pub fn hash(&self) -> Result<Hash, DeepError> {
        Ok(hash(&self.canonical_bytes()?))
    }
}

/// Customer-signed approval of one exact request, optionally narrowed.
#[derive(Clone, Debug)]
pub struct ConsentReceipt {
    /// Exact request commitment.
    pub request_hash: Hash,
    /// Customer-side identity.
    pub approver: String,
    /// Customer-trusted signing key.
    pub approver_key: KeyId,
    /// Strength of customer authentication.
    pub method: AuthMethod,
    /// Approved subset of the original scope.
    pub approved_scope: Scope,
    /// Inclusive beginning, Unix seconds.
    pub valid_from: i64,
    /// Exclusive end, Unix seconds.
    pub valid_to: i64,
    /// Single-use, customer-key-scoped freshness tag.
    pub nonce: [u8; 16],
    /// Raw Ed25519 signature over signing_bytes().
    pub signature: Vec<u8>,
}
impl ConsentReceipt {
    /// Domain-separated canonical approval body; the signature is excluded.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, DeepError> {
        canonical(
            b"SPOT-DEEP-CONSENT-V1\n",
            &(
                self.request_hash,
                &self.approver,
                self.approver_key.as_str(),
                self.method,
                &self.approved_scope,
                self.valid_from,
                self.valid_to,
                self.nonce,
            ),
        )
    }
    /// Commitment includes both the approval body and its signature.
    pub fn hash(&self) -> Result<Hash, DeepError> {
        canonical(
            b"SPOT-DEEP-RECEIPT-V1\n",
            &(self.signing_bytes()?, &self.signature),
        )
        .map(|b| hash(&b))
    }
}

/// One customer-controlled approver entry.
#[derive(Clone, Debug)]
pub struct TrustedApprover {
    /// Identity that owns the trusted key.
    pub identity: String,
    /// Raw public key.
    pub public_key: [u8; 32],
    /// Whether this customer identity is a tenant owner.
    pub is_owner: bool,
}
/// Customer trust is segregated by tenant and key id.
#[derive(Clone, Debug, Default)]
pub struct CustomerTrust {
    entries: BTreeMap<(String, KeyId), TrustedApprover>,
}
impl CustomerTrust {
    /// Add a customer-authorised public key; callers load this from tenant configuration.
    pub fn add(&mut self, tenant: &str, entry: TrustedApprover) -> KeyId {
        let id = KeyId::from_public_key(&entry.public_key);
        self.entries.insert((tenant.to_owned(), id.clone()), entry);
        id
    }
    pub(crate) fn get(&self, tenant: &str, id: &KeyId) -> Option<&TrustedApprover> {
        self.entries.get(&(tenant.to_owned(), id.clone()))
    }
}

/// Deployment limits; maxima are additionally capped by the consent and request windows.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Maximum seconds at T0, T1, T2 and T3 respectively.
    pub max_session: [i64; 4],
    /// Trusted catalogue operations touching protected groups; every use requires step-up.
    pub protected_actions: BTreeSet<String>,
    /// Consecutive denied uses causing irreversible anomaly revocation.
    pub max_denials: u32,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            max_session: [8 * 3600, 8 * 3600, 2 * 3600, 30 * 60],
            protected_actions: BTreeSet::new(),
            max_denials: 5,
        }
    }
}
pub(crate) fn tier_index(tier: Tier) -> usize {
    match tier {
        Tier::T0 => 0,
        Tier::T1 => 1,
        Tier::T2 => 2,
        Tier::T3 => 3,
    }
}

/// A hardware-bound credential. Only issue() can construct this type.
/// It intentionally implements neither Default nor Deserialize nor Clone.
///
/// ```compile_fail
/// use spot_deep_access::DeepCredential;
/// let forged = DeepCredential { session_id: [0; 16] };
/// ```
pub struct DeepCredential {
    pub(crate) grant: SessionGrant,
    pub(crate) request_hash: Hash,
    pub(crate) receipt_hash: Hash,
    pub(crate) operator_key: [u8; 32],
    pub(crate) operator_key_id: KeyId,
    pub(crate) attestation_hash: Hash,
    pub(crate) session_id: [u8; 16],
    pub(crate) secret: [u8; 32],
    pub(crate) step_action: Option<String>,
    pub(crate) protected_actions: BTreeSet<String>,
    pub(crate) max_denials: u32,
}
impl std::fmt::Debug for DeepCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepCredential")
            .field("session_id", &self.session_id)
            .field("operator_key", &"<redacted>")
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}
impl DeepCredential {
    /// Public session identifier; it is not a bearer credential.
    #[must_use]
    pub fn session_id(&self) -> [u8; 16] {
        self.session_id
    }
    /// The consented request commitment.
    #[must_use]
    pub fn request_hash(&self) -> Hash {
        self.request_hash
    }
    /// The exact consent receipt commitment.
    #[must_use]
    pub fn receipt_hash(&self) -> Hash {
        self.receipt_hash
    }
    /// Operator hardware-key identity.
    #[must_use]
    pub fn operator_key_id(&self) -> &KeyId {
        &self.operator_key_id
    }
    /// Hardware attestation commitment.
    #[must_use]
    pub fn attestation_hash(&self) -> Hash {
        self.attestation_hash
    }
    /// Inclusive beginning, Unix seconds.
    #[must_use]
    pub fn not_before(&self) -> i64 {
        self.grant.starts_at
    }
    /// Exclusive end, Unix seconds.
    #[must_use]
    pub fn not_after(&self) -> i64 {
        self.grant.expires_at
    }
    /// Approved scope, never the original wider scope.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.grant.scope
    }
    /// Highest consented tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.grant.tier_ceiling
    }
    /// Tenant owning this session and its log.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.grant.tenant
    }
}
