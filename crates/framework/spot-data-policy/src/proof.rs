//! The deletion proof: what survives after a record is gone.
//!
//! A retention deletion removes the document, so the only evidence left is the proof: the record
//! id, the DocType, the rule, the instant and a SHA-256 digest over them, written in the same item
//! transaction as the deletion and kept in a proof ledger that is separate from the record store
//! (ADR-071's per-item outcome and the task's "kept after the data is gone"). The proof contains
//! no field value and no personal data.
//!
//! ```text
//! digest = SHA-256("spot.data_policy.deletion_proof.v1\0"
//!                  || field(record id) || field(doctype) || field(rule id)
//!                  || field(action) || u64(instant))
//! ```
//!
//! `field` is a `u32` big-endian length followed by the bytes, so two fields can never run
//! together into the same digest.

use sha2::{Digest as _, Sha256};

use crate::error::DataPolicyError;
use crate::record::Record;
use crate::rule::Action;
use crate::store::RetentionStore;

/// The domain tag of the proof digest.
pub const PROOF_DOMAIN: &[u8] = b"spot.data_policy.deletion_proof.v1\0";

/// The evidence that one record was deleted by one rule at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionProof {
    /// The deleted record's id.
    pub record_id: String,
    /// The deleted record's DocType.
    pub doctype: String,
    /// The rule that deleted it.
    pub rule_id: String,
    /// The action (`delete`; a proof is only written for a deletion).
    pub action: Action,
    /// When the deletion happened, in Unix milliseconds.
    pub instant: u64,
    /// The digest over the fields above.
    pub digest: [u8; 32],
}

impl DeletionProof {
    /// Builds the proof for deleting `record` under `rule_id` at `instant`.
    #[must_use]
    pub fn compute(record: &Record, rule_id: &str, instant: u64) -> Self {
        let mut proof = Self {
            record_id: record.id.clone(),
            doctype: record.doctype.clone(),
            rule_id: rule_id.to_owned(),
            action: Action::Delete,
            instant,
            digest: [0; 32],
        };
        proof.digest = proof.recompute();
        proof
    }

    /// Recomputes the digest from the proof's fields.
    #[must_use]
    pub fn recompute(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PROOF_DOMAIN);
        put_field(&mut hasher, self.record_id.as_bytes());
        put_field(&mut hasher, self.doctype.as_bytes());
        put_field(&mut hasher, self.rule_id.as_bytes());
        put_field(&mut hasher, self.action.as_str().as_bytes());
        hasher.update(self.instant.to_be_bytes());
        hasher.finalize().into()
    }

    /// Checks the digest has not been altered.
    ///
    /// # Errors
    /// [`DataPolicyError::ProofInvalid`] when the digest does not match the fields.
    pub fn verify_digest(&self) -> Result<(), DataPolicyError> {
        if self.recompute() == self.digest {
            Ok(())
        } else {
            Err(DataPolicyError::ProofInvalid {
                record: self.record_id.clone(),
                reason: "the digest does not cover the proof's fields",
            })
        }
    }

    /// Checks the digest and that the record is gone from `store`.
    ///
    /// # Errors
    /// [`DataPolicyError::ProofInvalid`] when the digest is wrong, or the store still has the
    /// record (the proof then contradicts the data); [`DataPolicyError::StoreFailed`] when the
    /// store cannot answer (a proof is never verified against a record read that failed).
    pub fn verify(&self, store: &dyn RetentionStore) -> Result<(), DataPolicyError> {
        self.verify_digest()?;
        if store.record(&self.record_id)?.is_some() {
            return Err(DataPolicyError::ProofInvalid {
                record: self.record_id.clone(),
                reason: "the record the proof claims to have deleted is still present",
            });
        }
        Ok(())
    }

    /// The digest in lower-case hexadecimal.
    #[must_use]
    pub fn digest_hex(&self) -> String {
        to_hex(&self.digest)
    }
}

pub(crate) fn put_field(hasher: &mut Sha256, value: &[u8]) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    hasher.update(length.to_be_bytes());
    hasher.update(value);
}

/// Lower-case hexadecimal of arbitrary bytes.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{DeletionProof, to_hex};

    fn record(id: &str, doctype: &str) -> crate::record::Record {
        crate::record::Record {
            id: id.to_owned(),
            doctype: doctype.to_owned(),
            doc_class: None,
            jurisdiction: None,
            fields: Default::default(),
            created: 1,
            closed: None,
            events: Default::default(),
            references: Vec::new(),
        }
    }

    #[test]
    fn a_proof_digest_covers_every_field() {
        let proof = DeletionProof::compute(&record("a", "invoice"), "r1", 42);
        assert!(proof.verify_digest().is_ok());
        let mut changed = proof.clone();
        changed.record_id = "b".to_owned();
        assert!(changed.verify_digest().is_err());
        let mut changed = proof;
        changed.instant += 1;
        assert!(changed.verify_digest().is_err());
    }

    #[test]
    fn hex_is_two_digits_per_byte() {
        assert_eq!(to_hex(&[0, 1, 255]), "0001ff");
    }
}
