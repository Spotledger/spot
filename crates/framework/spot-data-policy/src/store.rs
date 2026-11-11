//! The store seam and the in-memory reference implementation.
//!
//! The retention engine never edits a document itself. It asks a [`RetentionStore`] for the typed
//! record view, and hands it one [`ApplyItem`] per item. The contract is ADR-071's: the item's
//! effect, its outcome row, its audit record and (for a deletion) its proof are committed **in one
//! transaction**. A crash can therefore never leave work done but unrecorded, or recorded but not
//! done, and a resumed run reads the outcome rows rather than repeating effects.
//!
//! A real backend performs a deletion or an archive through the document lifecycle and an
//! anonymisation through the anonymise operation (ADR-051); [`MemoryStore`] is the reference used
//! by the tests, and logs each effect so a test can show that an item ran exactly once.
//!
//! Proofs and audit records live in their own maps: deleting a record never deletes its proof.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use spot_audit_chain::AuditRecord;

use crate::error::DataPolicyError;
use crate::proof::DeletionProof;
use crate::record::{FieldValue, Record};
use crate::rule::Action;

/// What the engine asks the store to do for one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing (a skipped item still gets an outcome row).
    None,
    /// Delete the record through the document lifecycle.
    Delete {
        /// The record.
        record_id: String,
    },
    /// Replace the listed personal fields through the anonymise operation.
    Anonymise {
        /// The record.
        record_id: String,
        /// The fields to replace, from the field classification.
        fields: Vec<String>,
    },
    /// Move the record to the archive store; it is still retained.
    Archive {
        /// The record.
        record_id: String,
    },
}

impl Effect {
    /// The effect's name, for audit payloads and test logs.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Delete { .. } => "delete",
            Self::Anonymise { .. } => "anonymise",
            Self::Archive { .. } => "archive",
        }
    }

    /// The record the effect names.
    #[must_use]
    pub fn record_id(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Delete { record_id }
            | Self::Anonymise { record_id, .. }
            | Self::Archive { record_id } => Some(record_id),
        }
    }
}

/// What happened to one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    /// The effect was applied.
    Applied,
    /// The effect would have been applied (dry run).
    WouldApply,
    /// The item was left alone, with a reason.
    Skipped,
    /// The item failed; the run follows its stop policy.
    Failed,
}

impl OutcomeStatus {
    /// The status as text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::WouldApply => "would_apply",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

/// Why the engine left a record alone. Every variant is a normal, explainable outcome — none is a
/// silent drop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The record's period has not ended yet.
    NotDue,
    /// A legal hold covers the record.
    LegalHold {
        /// The hold id.
        hold: String,
        /// The reason recorded on the hold.
        reason: String,
    },
    /// The statutory floor is later than the rule's date and has not been reached.
    StatutoryFloorPending {
        /// The earliest instant the law allows deletion, in Unix milliseconds.
        earliest: u64,
    },
    /// The record's class is subject to a statutory floor, but its jurisdiction is unknown.
    StatutoryFloorUnknown,
    /// The rule's exceptions name the record.
    Exception,
    /// The DocType has no field classification, so an anonymisation cannot name what to replace.
    Unclassified,
    /// Another retained record still links to this one.
    Referential {
        /// The referrers that blocked the deletion, in stable order.
        referrers: Vec<String>,
    },
    /// The rule's trigger is not present on the record (for example no `closed` instant).
    TriggerMissing,
    /// The injected calendar refused the record's instant or period.
    CalendarRefused {
        /// A fixed description of the refusal.
        detail: String,
    },
    /// The record was already gone when the item ran.
    AlreadyGone,
    /// The item was already done by an earlier attempt.
    AlreadyDone,
}

impl SkipReason {
    /// The reason as a stable word, written to plans and outcomes.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotDue => "not_due",
            Self::LegalHold { .. } => "legal_hold",
            Self::StatutoryFloorPending { .. } => "statutory_floor_pending",
            Self::StatutoryFloorUnknown => "statutory_floor_unknown",
            Self::Exception => "exception",
            Self::Unclassified => "unclassified",
            Self::Referential { .. } => "referential",
            Self::TriggerMissing => "trigger_missing",
            Self::CalendarRefused { .. } => "calendar_refused",
            Self::AlreadyGone => "already_gone",
            Self::AlreadyDone => "already_done",
        }
    }

    /// A human-readable explanation, with the hold, floor or referrers.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::NotDue => "the retention period has not ended".to_owned(),
            Self::LegalHold { hold, reason } => {
                format!("legal hold {hold} suspends every action: {reason}")
            }
            Self::StatutoryFloorPending { earliest } => {
                format!("the statutory period ends at {earliest} (Unix milliseconds)")
            }
            Self::StatutoryFloorUnknown => {
                "the record's class has a statutory floor but its jurisdiction is unknown"
                    .to_owned()
            }
            Self::Exception => "the rule's exceptions name this record".to_owned(),
            Self::Unclassified => {
                "the DocType has no field classification for anonymisation".to_owned()
            }
            Self::Referential { referrers } => {
                format!(
                    "retained records still link to it: {}",
                    referrers.join(", ")
                )
            }
            Self::TriggerMissing => "the rule's trigger is not present on the record".to_owned(),
            Self::CalendarRefused { detail } => format!("the calendar refused it: {detail}"),
            Self::AlreadyGone => "the record was already gone".to_owned(),
            Self::AlreadyDone => "an earlier attempt already produced this outcome".to_owned(),
        }
    }
}

/// The outcome of one item, written in the same transaction as its effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemOutcome {
    /// The run the item belongs to.
    pub run_id: String,
    /// The record the item is about.
    pub record_id: String,
    /// The rule that produced the item.
    pub rule_id: String,
    /// The action (absent for an item that never matched a rule).
    pub action: Option<Action>,
    /// What happened.
    pub status: OutcomeStatus,
    /// Why it was skipped or failed.
    pub reason: Option<SkipReason>,
    /// The attempt that produced the outcome.
    pub attempt: u32,
    /// When the item ran, in Unix milliseconds.
    pub at: u64,
    /// The deletion proof's digest, when the item deleted the record.
    pub proof_digest: Option<String>,
}

impl ItemOutcome {
    /// Whether the item's effect was applied (not merely planned).
    #[must_use]
    pub fn is_applied(&self) -> bool {
        self.status == OutcomeStatus::Applied
    }
}

/// Everything one item's transaction commits.
#[derive(Clone, Debug)]
pub struct ApplyItem {
    /// The effect on the record.
    pub effect: Effect,
    /// The outcome row.
    pub outcome: ItemOutcome,
    /// The audit record, when the item changed data.
    pub audit: Option<AuditRecord>,
    /// The deletion proof, when the effect is a deletion.
    pub proof: Option<DeletionProof>,
}

/// The checkpoint of a run: an optimisation, never the truth (ADR-071 section 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The run id: `{kind}:{plan identity}`.
    pub run_id: String,
    /// The plan's identity digest.
    pub plan_digest: [u8; 32],
    /// The next item index to read.
    pub next_index: usize,
    /// The attempt number, starting at 1.
    pub attempt: u32,
    /// Items read so far.
    pub read: u64,
    /// Items applied so far.
    pub applied: u64,
    /// Items skipped so far.
    pub skipped: u64,
    /// Items failed so far.
    pub failed: u64,
    /// Whether every item has an outcome.
    pub completed: bool,
}

impl Checkpoint {
    /// A fresh checkpoint for `run_id` and `plan_digest`.
    #[must_use]
    pub fn new(run_id: impl Into<String>, plan_digest: [u8; 32]) -> Self {
        Self {
            run_id: run_id.into(),
            plan_digest,
            next_index: 0,
            attempt: 1,
            read: 0,
            applied: 0,
            skipped: 0,
            failed: 0,
            completed: false,
        }
    }
}

/// One effect the store actually applied, for the in-memory log and for tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedEffect {
    /// The run that applied it.
    pub run_id: String,
    /// The record.
    pub record_id: String,
    /// The effect's name.
    pub effect: &'static str,
    /// The attempt that applied it.
    pub attempt: u32,
}

/// The store seam.
///
/// Implementations must:
/// - make [`RetentionStore::apply_item`] atomic in its backend (effect, outcome, audit, proof);
/// - make it idempotent for the same `(run_id, record_id)`: a second call must not apply the
///   effect twice;
/// - never delete a proof or an audit record when the record is deleted;
/// - perform delete/archive through the document lifecycle and anonymise through the anonymise
///   operation.
pub trait RetentionStore {
    /// Every live record, in stable id order.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails. A failed read is never an empty
    /// list: a plan built from "no records" would silently retain everything, and a check that
    /// the record is gone would pass.
    fn records(&self) -> Result<Vec<Record>, DataPolicyError>;

    /// One live record, `Ok(None)` when it is not there.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails (which is not "the record is
    /// gone").
    fn record(&self, id: &str) -> Result<Option<Record>, DataPolicyError>;

    /// Commits one item.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails.
    fn apply_item(&mut self, item: ApplyItem) -> Result<(), DataPolicyError>;

    /// The outcome of `record_id` in `run_id`, `Ok(None)` if it has none yet.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails (which is not "not applied yet").
    fn outcome(
        &self,
        run_id: &str,
        record_id: &str,
    ) -> Result<Option<ItemOutcome>, DataPolicyError>;

    /// Every outcome of a run, in item order.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails.
    fn run_outcomes(&self, run_id: &str) -> Result<Vec<ItemOutcome>, DataPolicyError>;

    /// Saves a checkpoint.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails.
    fn save_checkpoint(&mut self, checkpoint: Checkpoint) -> Result<(), DataPolicyError>;

    /// The saved checkpoint of a run, `Ok(None)` if there is none.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails (which is not "a fresh run").
    fn checkpoint(&self, run_id: &str) -> Result<Option<Checkpoint>, DataPolicyError>;

    /// The deletion proof of a record, `Ok(None)` if none was written.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails.
    fn proof(&self, record_id: &str) -> Result<Option<DeletionProof>, DataPolicyError>;

    /// Every deletion proof, in record id order.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the backend fails.
    fn proofs(&self) -> Result<Vec<DeletionProof>, DataPolicyError>;
}

/// The in-memory reference store.
#[derive(Default)]
pub struct MemoryStore {
    records: BTreeMap<String, Record>,
    archived: BTreeSet<String>,
    outcomes: BTreeMap<(String, String), ItemOutcome>,
    checkpoints: BTreeMap<String, Checkpoint>,
    proofs: BTreeMap<String, DeletionProof>,
    audits: Vec<AuditRecord>,
    effects: Vec<AppliedEffect>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store holding `records`, validating each.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidRecord`] when a record is malformed or an id is repeated.
    pub fn from_records(records: Vec<Record>) -> Result<Self, DataPolicyError> {
        let mut store = Self::new();
        for record in records {
            record.validate()?;
            if store
                .records
                .insert(record.id.clone(), record.clone())
                .is_some()
            {
                return Err(DataPolicyError::InvalidRecord {
                    record: record.id,
                    reason: "the record id is repeated".to_owned(),
                });
            }
        }
        Ok(store)
    }

    /// Adds a record, validating it.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidRecord`] when it is malformed or its id is taken.
    pub fn insert(&mut self, record: Record) -> Result<(), DataPolicyError> {
        record.validate()?;
        if self.records.contains_key(&record.id) {
            return Err(DataPolicyError::InvalidRecord {
                record: record.id,
                reason: "the record id is taken".to_owned(),
            });
        }
        self.records.insert(record.id.clone(), record);
        Ok(())
    }

    /// Removes a record without a proof (test fixture only; the engine never does this).
    pub fn remove(&mut self, id: &str) -> Option<Record> {
        self.archived.remove(id);
        self.records.remove(id)
    }

    /// How many live records the store holds.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// Whether a record was archived by the engine.
    #[must_use]
    pub fn is_archived(&self, id: &str) -> bool {
        self.archived.contains(id)
    }

    /// Every applied effect, in the order the store applied them.
    #[must_use]
    pub fn effects(&self) -> &[AppliedEffect] {
        &self.effects
    }

    /// Every audit record the store committed, in order.
    #[must_use]
    pub fn audits(&self) -> &[AuditRecord] {
        &self.audits
    }

    fn apply_effect(&mut self, effect: &Effect) -> Result<(), DataPolicyError> {
        match effect {
            Effect::None => Ok(()),
            Effect::Delete { record_id } => {
                if self.records.remove(record_id).is_none() {
                    return Err(DataPolicyError::StoreFailed {
                        reason: "the record to delete is not present",
                    });
                }
                self.archived.remove(record_id);
                Ok(())
            }
            Effect::Anonymise { record_id, fields } => {
                let Some(record) = self.records.get_mut(record_id) else {
                    return Err(DataPolicyError::StoreFailed {
                        reason: "the record to anonymise is not present",
                    });
                };
                for field in fields {
                    if let Some(value) = record.fields.get_mut(field) {
                        *value = FieldValue::Text("[anonymised]".to_owned());
                    }
                }
                Ok(())
            }
            Effect::Archive { record_id } => {
                if !self.records.contains_key(record_id) {
                    return Err(DataPolicyError::StoreFailed {
                        reason: "the record to archive is not present",
                    });
                }
                self.archived.insert(record_id.clone());
                Ok(())
            }
        }
    }
}

impl RetentionStore for MemoryStore {
    fn records(&self) -> Result<Vec<Record>, DataPolicyError> {
        Ok(self.records.values().cloned().collect())
    }

    fn record(&self, id: &str) -> Result<Option<Record>, DataPolicyError> {
        Ok(self.records.get(id).cloned())
    }

    fn apply_item(&mut self, item: ApplyItem) -> Result<(), DataPolicyError> {
        let key = (item.outcome.run_id.clone(), item.outcome.record_id.clone());
        if let Some(existing) = self.outcomes.get(&key) {
            // Idempotent by construction: the first outcome is the truth.
            if existing == &item.outcome {
                return Ok(());
            }
            return Err(DataPolicyError::StoreFailed {
                reason: "the item already has a different outcome",
            });
        }
        self.apply_effect(&item.effect)?;
        if item.outcome.is_applied()
            && let Some(record_id) = item.effect.record_id()
        {
            self.effects.push(AppliedEffect {
                run_id: item.outcome.run_id.clone(),
                record_id: record_id.to_owned(),
                effect: item.effect.as_str(),
                attempt: item.outcome.attempt,
            });
        }
        if let Some(proof) = item.proof {
            self.proofs.insert(proof.record_id.clone(), proof);
        }
        if let Some(audit) = item.audit {
            self.audits.push(audit);
        }
        self.outcomes.insert(key, item.outcome);
        Ok(())
    }

    fn outcome(
        &self,
        run_id: &str,
        record_id: &str,
    ) -> Result<Option<ItemOutcome>, DataPolicyError> {
        Ok(self
            .outcomes
            .get(&(run_id.to_owned(), record_id.to_owned()))
            .cloned())
    }

    fn run_outcomes(&self, run_id: &str) -> Result<Vec<ItemOutcome>, DataPolicyError> {
        Ok(self
            .outcomes
            .values()
            .filter(|outcome| outcome.run_id == run_id)
            .cloned()
            .collect())
    }

    fn save_checkpoint(&mut self, checkpoint: Checkpoint) -> Result<(), DataPolicyError> {
        self.checkpoints
            .insert(checkpoint.run_id.clone(), checkpoint);
        Ok(())
    }

    fn checkpoint(&self, run_id: &str) -> Result<Option<Checkpoint>, DataPolicyError> {
        Ok(self.checkpoints.get(run_id).cloned())
    }

    fn proof(&self, record_id: &str) -> Result<Option<DeletionProof>, DataPolicyError> {
        Ok(self.proofs.get(record_id).cloned())
    }

    fn proofs(&self) -> Result<Vec<DeletionProof>, DataPolicyError> {
        Ok(self.proofs.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{
        ApplyItem, Checkpoint, Effect, ItemOutcome, MemoryStore, OutcomeStatus, RetentionStore,
        SkipReason,
    };
    use crate::record::Record;

    fn record(id: &str) -> Record {
        Record {
            id: id.to_owned(),
            doctype: "invoice".to_owned(),
            doc_class: None,
            jurisdiction: None,
            fields: Default::default(),
            created: 1,
            closed: None,
            events: Default::default(),
            references: Vec::new(),
        }
    }

    fn outcome(run: &str, id: &str, status: OutcomeStatus) -> ItemOutcome {
        ItemOutcome {
            run_id: run.to_owned(),
            record_id: id.to_owned(),
            rule_id: "r".to_owned(),
            action: Some(crate::rule::Action::Delete),
            status,
            reason: None,
            attempt: 1,
            at: 1,
            proof_digest: None,
        }
    }

    #[test]
    fn an_item_is_applied_once() {
        let mut store = MemoryStore::from_records(vec![record("a")]).unwrap();
        let item = ApplyItem {
            effect: Effect::Delete {
                record_id: "a".to_owned(),
            },
            outcome: outcome("run", "a", OutcomeStatus::Applied),
            audit: None,
            proof: None,
        };
        store.apply_item(item.clone()).unwrap();
        assert_eq!(store.record_count(), 0);
        assert_eq!(store.effects().len(), 1);
        store.apply_item(item).unwrap();
        assert_eq!(store.effects().len(), 1, "the second call is a no-op");
    }

    #[test]
    fn a_skipped_item_records_an_outcome_but_applies_nothing() {
        let mut store = MemoryStore::from_records(vec![record("a")]).unwrap();
        let mut skipped = outcome("run", "a", OutcomeStatus::Skipped);
        skipped.reason = Some(SkipReason::NotDue);
        store
            .apply_item(ApplyItem {
                effect: Effect::None,
                outcome: skipped,
                audit: None,
                proof: None,
            })
            .unwrap();
        assert_eq!(store.record_count(), 1);
        assert!(store.effects().is_empty());
        assert_eq!(
            store.outcome("run", "a").unwrap().unwrap().status,
            OutcomeStatus::Skipped
        );
    }

    #[test]
    fn checkpoints_are_stored_per_run() {
        let mut store = MemoryStore::new();
        let checkpoint = Checkpoint::new("run", [7; 32]);
        store.save_checkpoint(checkpoint.clone()).unwrap();
        assert_eq!(store.checkpoint("run").unwrap(), Some(checkpoint));
    }
}
