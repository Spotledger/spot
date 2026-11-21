//! The executor: apply a plan in batches, with identity, checkpoints, per-item outcomes and
//! resume (ADR-071).
//!
//! The plan is what a person approves; the executor is what changes data. It is safe to kill at
//! any point:
//!
//! - the run's identity is `{kind}:{plan identity}`, so starting the same plan twice is refused
//!   and only `resume` continues the existing run (ADR-071 section 2);
//! - every item commits its effect, outcome, audit record and (for a deletion) proof through
//!   [`RetentionStore::apply_item`], so an item is done **and** recorded or neither;
//! - the checkpoint is written after each batch; it saves re-scanning, while the outcome rows are
//!   the truth. A stale checkpoint therefore repeats no effect;
//! - a dry run is the default and writes nothing.
//!
//! Conditions are re-checked against the live store before an item runs: a legal hold set after
//! planning still stops it, a statutory floor still wins, and a record a retained record still
//! links to is not deleted.

use std::collections::BTreeMap;

use spot_audit_chain::{AuditRecord, PayloadValue, Subject};

use crate::calendar::CivilDate;
use crate::error::DataPolicyError;
use crate::plan::{PlannedAction, PolicySet, RetentionPlan};
use crate::proof::DeletionProof;
use crate::statutory::FloorLookup;
use crate::store::{
    ApplyItem, Checkpoint, Effect, ItemOutcome, OutcomeStatus, RetentionStore, SkipReason,
};

/// The identity kind of a retention run, used in the run id and the audit kind.
pub const DEFAULT_KIND: &str = "data_policy_retention";

/// Whether the call starts a run or continues one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    /// Start a new run; an existing run with the same identity is refused.
    Fresh,
    /// Continue the run saved under this plan's identity; attempt + 1.
    Continue,
}

/// How the executor runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionOptions {
    /// Items per batch; the checkpoint is written after each batch.
    pub batch_size: usize,
    /// Stop after this many batches (a test seam for a kill; `None` runs to the end).
    pub max_batches: Option<usize>,
    /// Plan only: the default. A dry run writes no outcome, audit, proof or checkpoint.
    pub dry_run: bool,
    /// The audit actor (`system`, `user:<id>` or `agent:<id>`).
    pub actor: String,
    /// The run kind, qualified like an app manifest kind. It must not contain `:`.
    pub kind: String,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            batch_size: 100,
            max_batches: None,
            dry_run: true,
            actor: "system".to_owned(),
            kind: DEFAULT_KIND.to_owned(),
        }
    }
}

impl ExecutionOptions {
    /// Checks the options are usable.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidOption`] when a size is zero, the kind is empty or contains `:`,
    /// or the actor is not an identity the audit chain accepts.
    pub fn validate(&self) -> Result<(), DataPolicyError> {
        if self.batch_size == 0 {
            return Err(DataPolicyError::InvalidOption {
                option: "batch_size",
                reason: "a batch holds at least one item",
            });
        }
        if self.max_batches == Some(0) {
            return Err(DataPolicyError::InvalidOption {
                option: "max_batches",
                reason: "at least one batch is run",
            });
        }
        if self.kind.is_empty()
            || self.kind.len() > 128
            || self.kind.contains(':')
            || !self
                .kind
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(DataPolicyError::InvalidOption {
                option: "kind",
                reason: "a kind is 1 to 128 letters, digits, '_', '-' or '.' and has no ':'",
            });
        }
        let actor_ok = self.actor == "system"
            || self
                .actor
                .strip_prefix("user:")
                .or_else(|| self.actor.strip_prefix("agent:"))
                .is_some_and(|id| !id.is_empty() && self.actor.len() <= 256);
        if !actor_ok {
            return Err(DataPolicyError::InvalidOption {
                option: "actor",
                reason: "an actor is system, user:<id> or agent:<id>",
            });
        }
        Ok(())
    }
}

/// What an execution attempt did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionReport {
    /// The run id (`{kind}:{plan identity}`).
    pub run_id: String,
    /// The attempt number.
    pub attempt: u32,
    /// The plan identity in hexadecimal.
    pub plan_identity: String,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Whether every item has an outcome.
    pub completed: bool,
    /// The checkpoint as saved (or as it would be, for a dry run).
    pub checkpoint: Checkpoint,
    /// The outcomes produced by this call, in item order.
    pub outcomes: Vec<ItemOutcome>,
}

impl ExecutionReport {
    /// Items read in this run (all attempts, from the checkpoints' counters).
    #[must_use]
    pub fn read(&self) -> u64 {
        self.checkpoint.read
    }

    /// Items applied.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.checkpoint.applied
    }

    /// Items skipped.
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.checkpoint.skipped
    }

    /// Items failed.
    #[must_use]
    pub fn failed(&self) -> u64 {
        self.checkpoint.failed
    }
}

/// Executes `plan`.
///
/// # Errors
/// [`DataPolicyError::RunMismatch`] when a fresh start meets an existing run or a resume has no
/// checkpoint; [`DataPolicyError::PlanChanged`] when a resumed checkpoint came from another plan;
/// [`DataPolicyError::InvalidOption`] for invalid options;
/// [`DataPolicyError::StoreFailed`] when the store fails: a failed **read** (an outcome, a
/// checkpoint, a record, the run's outcomes) stops the run at once, no further item is applied,
/// and the saved checkpoint stays as it was (the outcome rows are the truth, so a later run with a
/// healthy store resumes with no duplicate effect).
pub fn execute(
    plan: &RetentionPlan,
    policy: &PolicySet<'_>,
    store: &mut dyn RetentionStore,
    options: &ExecutionOptions,
    resume: Resume,
    now: u64,
) -> Result<ExecutionReport, DataPolicyError> {
    options.validate()?;
    if options.dry_run && resume == Resume::Continue {
        return Err(DataPolicyError::InvalidOption {
            option: "dry_run",
            reason: "a dry run writes nothing, so it cannot be resumed",
        });
    }
    let run_id = format!("{}:{}", options.kind, plan.identity_hex());
    let identity = plan.identity();

    if options.dry_run {
        return Ok(dry_run_report(plan, &run_id, now));
    }

    let mut checkpoint = match resume {
        Resume::Fresh => {
            if store.checkpoint(&run_id)?.is_some() {
                return Err(DataPolicyError::RunMismatch {
                    run: run_id,
                    operation: "started",
                    reason: "a run with this identity already exists; resume it",
                });
            }
            Checkpoint::new(run_id.clone(), identity)
        }
        Resume::Continue => {
            let Some(mut saved) = store.checkpoint(&run_id)? else {
                return Err(DataPolicyError::RunMismatch {
                    run: run_id,
                    operation: "resumed",
                    reason: "no checkpoint exists for this plan",
                });
            };
            if saved.plan_digest != identity {
                return Err(DataPolicyError::PlanChanged { run: run_id });
            }
            if saved.completed {
                return Err(DataPolicyError::RunMismatch {
                    run: run_id,
                    operation: "resumed",
                    reason: "the run is already complete",
                });
            }
            saved.attempt = saved.attempt.saturating_add(1);
            saved
        }
    };

    let total = plan.items().len();
    let start = checkpoint.next_index.min(total);
    let mut outcomes = Vec::new();
    let mut batches = 0usize;
    let mut index = start;
    while index < total {
        let end = index.saturating_add(options.batch_size).min(total);
        for item in &plan.items()[index..end] {
            if let Some(existing) = store.outcome(&run_id, &item.record_id)? {
                // The outcome row is the truth, not the checkpoint.
                outcomes.push(existing);
                continue;
            }
            let outcome = execute_item(
                policy,
                store,
                options,
                &run_id,
                checkpoint.attempt,
                now,
                item,
            )?;
            outcomes.push(outcome);
        }
        index = end;
        checkpoint.next_index = index;
        checkpoint.completed = index >= total;
        refresh_counters(&mut checkpoint, store)?;
        store.save_checkpoint(checkpoint.clone())?;
        batches += 1;
        if options.max_batches.is_some_and(|max| batches >= max) {
            break;
        }
    }
    if total == 0 && start == 0 {
        checkpoint.completed = true;
        refresh_counters(&mut checkpoint, store)?;
        store.save_checkpoint(checkpoint.clone())?;
    }

    Ok(ExecutionReport {
        run_id,
        attempt: checkpoint.attempt,
        plan_identity: plan.identity_hex(),
        dry_run: false,
        completed: checkpoint.completed,
        checkpoint,
        outcomes,
    })
}

fn refresh_counters(
    checkpoint: &mut Checkpoint,
    store: &dyn RetentionStore,
) -> Result<(), DataPolicyError> {
    let all = store.run_outcomes(&checkpoint.run_id)?;
    checkpoint.read = u64::try_from(all.len()).unwrap_or(u64::MAX);
    checkpoint.applied = count(&all, OutcomeStatus::Applied);
    checkpoint.skipped = count(&all, OutcomeStatus::Skipped);
    checkpoint.failed = count(&all, OutcomeStatus::Failed);
    Ok(())
}

fn count(outcomes: &[ItemOutcome], status: OutcomeStatus) -> u64 {
    u64::try_from(
        outcomes
            .iter()
            .filter(|outcome| outcome.status == status)
            .count(),
    )
    .unwrap_or(u64::MAX)
}

fn dry_run_report(plan: &RetentionPlan, run_id: &str, now: u64) -> ExecutionReport {
    let mut outcomes = Vec::with_capacity(plan.items().len() + plan.skips().len());
    for item in plan.items() {
        outcomes.push(ItemOutcome {
            run_id: run_id.to_owned(),
            record_id: item.record_id.clone(),
            rule_id: item.rule_id.clone(),
            action: Some(item.action),
            status: OutcomeStatus::WouldApply,
            reason: None,
            attempt: 1,
            at: now,
            proof_digest: None,
        });
    }
    for skipped in plan.skips() {
        outcomes.push(ItemOutcome {
            run_id: run_id.to_owned(),
            record_id: skipped.record_id.clone(),
            rule_id: skipped.rule_id.clone(),
            action: Some(skipped.action),
            status: OutcomeStatus::Skipped,
            reason: Some(skipped.reason.clone()),
            attempt: 1,
            at: now,
            proof_digest: None,
        });
    }
    let mut checkpoint = Checkpoint::new(run_id.to_owned(), plan.identity());
    checkpoint.read = u64::try_from(outcomes.len()).unwrap_or(u64::MAX);
    checkpoint.skipped = count(&outcomes, OutcomeStatus::Skipped);
    checkpoint.completed = true;
    ExecutionReport {
        run_id: run_id.to_owned(),
        attempt: 1,
        plan_identity: plan.identity_hex(),
        dry_run: true,
        completed: true,
        checkpoint,
        outcomes,
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_item(
    policy: &PolicySet<'_>,
    store: &mut dyn RetentionStore,
    options: &ExecutionOptions,
    run_id: &str,
    attempt: u32,
    now: u64,
    item: &PlannedAction,
) -> Result<ItemOutcome, DataPolicyError> {
    let Some(record) = store.record(&item.record_id)? else {
        return finish_skip(store, run_id, attempt, now, item, SkipReason::AlreadyGone);
    };
    if let Some(hold) = policy.holds.covering(&record) {
        return finish_skip(
            store,
            run_id,
            attempt,
            now,
            item,
            SkipReason::LegalHold {
                hold: hold.id.clone(),
                reason: hold.reason.clone(),
            },
        );
    }

    if item.action == crate::rule::Action::Delete {
        if let Some(reason) = deletion_blocked(policy, store, item, now)? {
            return finish_skip(store, run_id, attempt, now, item, reason);
        }
        let proof = DeletionProof::compute(&record, &item.rule_id, now);
        let outcome = ItemOutcome {
            run_id: run_id.to_owned(),
            record_id: item.record_id.clone(),
            rule_id: item.rule_id.clone(),
            action: Some(item.action),
            status: OutcomeStatus::Applied,
            reason: None,
            attempt,
            at: now,
            proof_digest: Some(proof.digest_hex()),
        };
        let audit = build_audit(options, item, now, Some(&proof))?;
        store.apply_item(ApplyItem {
            effect: Effect::Delete {
                record_id: item.record_id.clone(),
            },
            outcome: outcome.clone(),
            audit: Some(audit),
            proof: Some(proof),
        })?;
        return Ok(outcome);
    }

    let effect = match item.action {
        crate::rule::Action::Anonymise => Effect::Anonymise {
            record_id: item.record_id.clone(),
            fields: item.anonymise_fields.clone(),
        },
        crate::rule::Action::Archive => Effect::Archive {
            record_id: item.record_id.clone(),
        },
        crate::rule::Action::Delete => {
            return Err(DataPolicyError::StoreFailed {
                reason: "the delete action was not handled before the effect",
            });
        }
    };
    let outcome = ItemOutcome {
        run_id: run_id.to_owned(),
        record_id: item.record_id.clone(),
        rule_id: item.rule_id.clone(),
        action: Some(item.action),
        status: OutcomeStatus::Applied,
        reason: None,
        attempt,
        at: now,
        proof_digest: None,
    };
    let audit = build_audit(options, item, now, None)?;
    store.apply_item(ApplyItem {
        effect,
        outcome: outcome.clone(),
        audit: Some(audit),
        proof: None,
    })?;
    Ok(outcome)
}

fn deletion_blocked(
    policy: &PolicySet<'_>,
    store: &dyn RetentionStore,
    item: &PlannedAction,
    now: u64,
) -> Result<Option<SkipReason>, DataPolicyError> {
    let trigger = CivilDate::from_unix_millis(item.trigger_at)?;
    if let Some(record) = store.record(&item.record_id)? {
        match policy.floors.lookup(&record, trigger, policy.calendar) {
            Ok(FloorLookup::NotSubject) => {}
            Ok(FloorLookup::UnknownJurisdiction) => {
                return Ok(Some(SkipReason::StatutoryFloorUnknown));
            }
            Ok(FloorLookup::Earliest(earliest)) if earliest > now => {
                return Ok(Some(SkipReason::StatutoryFloorPending { earliest }));
            }
            Ok(FloorLookup::Earliest(_)) => {}
            Err(error) => {
                return Ok(Some(SkipReason::CalendarRefused {
                    detail: error.to_string(),
                }));
            }
        }
    }
    let mut referrers: Vec<String> = store
        .records()?
        .into_iter()
        .filter(|other| other.id != item.record_id)
        .filter(|other| other.references_record(&item.record_id))
        .map(|other| other.id)
        .collect();
    if referrers.is_empty() {
        return Ok(None);
    }
    referrers.sort();
    Ok(Some(SkipReason::Referential { referrers }))
}

fn finish_skip(
    store: &mut dyn RetentionStore,
    run_id: &str,
    attempt: u32,
    now: u64,
    item: &PlannedAction,
    reason: SkipReason,
) -> Result<ItemOutcome, DataPolicyError> {
    let outcome = ItemOutcome {
        run_id: run_id.to_owned(),
        record_id: item.record_id.clone(),
        rule_id: item.rule_id.clone(),
        action: Some(item.action),
        status: OutcomeStatus::Skipped,
        reason: Some(reason),
        attempt,
        at: now,
        proof_digest: None,
    };
    store.apply_item(ApplyItem {
        effect: Effect::None,
        outcome: outcome.clone(),
        audit: None,
        proof: None,
    })?;
    Ok(outcome)
}

fn build_audit(
    options: &ExecutionOptions,
    item: &PlannedAction,
    now: u64,
    proof: Option<&DeletionProof>,
) -> Result<AuditRecord, DataPolicyError> {
    let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
    payload.insert("rule".to_owned(), PayloadValue::Text(item.rule_id.clone()));
    payload.insert(
        "action".to_owned(),
        PayloadValue::Text(item.action.as_str().to_owned()),
    );
    payload.insert(
        "status".to_owned(),
        PayloadValue::Text(OutcomeStatus::Applied.as_str().to_owned()),
    );
    if let Some(proof) = proof {
        payload.insert("proof".to_owned(), PayloadValue::Text(proof.digest_hex()));
    }
    let at = now
        .checked_mul(1_000)
        .and_then(|micros| i64::try_from(micros).ok())
        .ok_or(DataPolicyError::StoreFailed {
            reason: "the audit instant is out of range",
        })?;
    let record = AuditRecord {
        kind: format!("data_policy.{}", item.action.as_str()),
        actor: options.actor.clone(),
        on_behalf_of: None,
        subject: Some(Subject {
            doctype: item.doctype.clone(),
            id: item.record_id.clone(),
        }),
        at,
        payload,
        masked: true,
    };
    record
        .validate()
        .map_err(|_| DataPolicyError::StoreFailed {
            reason: "the audit record is not valid",
        })?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{ExecutionOptions, Resume, execute};
    use crate::calendar::{CivilDate, Gregorian};
    use crate::hold::Holds;
    use crate::plan::{PolicySet, plan_run};
    use crate::record::{FieldClassMap, Record};
    use crate::rule::RetentionRules;
    use crate::statutory::StatutoryFloor;
    use crate::store::{MemoryStore, RetentionStore};

    fn at(year: i32, month: u8, day: u8) -> u64 {
        CivilDate::new(year, month, day)
            .unwrap()
            .to_unix_millis()
            .unwrap()
    }

    fn record(id: &str) -> Record {
        Record {
            id: id.to_owned(),
            doctype: "note".to_owned(),
            doc_class: None,
            jurisdiction: None,
            fields: Default::default(),
            created: at(2020, 1, 1),
            closed: None,
            events: Default::default(),
            references: Vec::new(),
        }
    }

    #[test]
    fn a_dry_run_is_the_default_and_writes_nothing() {
        let mut store = MemoryStore::from_records(vec![record("a")]).unwrap();
        let rules = RetentionRules::from_toml(
            "[[rule]]\nid = \"n\"\ndoctype = \"note\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\n",
        )
        .unwrap();
        let holds = Holds::empty();
        let floors = StatutoryFloor::empty();
        let fields = FieldClassMap::default();
        let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
        let plan = plan_run(&policy, &store, at(2026, 1, 1)).unwrap();
        let options = ExecutionOptions::default();
        assert!(options.dry_run);
        let report = execute(
            &plan,
            &policy,
            &mut store,
            &options,
            Resume::Fresh,
            at(2026, 1, 1),
        )
        .unwrap();
        assert!(report.completed);
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(store.record_count(), 1);
        assert!(store.audits().is_empty());
        assert!(store.proofs().unwrap().is_empty());
        assert!(store.checkpoint(&report.run_id).unwrap().is_none());
    }
}
