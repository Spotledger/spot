//! The durability audit: what the client claimed versus what the store holds.
//!
//! After a fault and a recovery, the audit partitions every operation of the run into
//! [`CommitClass`] buckets — acknowledged, refused, failed, indeterminate-resolved-committed,
//! indeterminate-resolved-absent — and then asserts the platform's durability rules against the
//! store:
//!
//! - an **acknowledged** commit's effects are present: for every record it touched, the change
//!   feed holds exactly the one net entry (insert, update or delete) at the version the operation
//!   wrote;
//! - a **refused**, **failed** or **resolved-absent** operation's effects are absent: no such
//!   entry exists;
//! - an operation that was **not** acknowledged is either wholly present or wholly absent,
//!   never partial — checked per transaction over its own touched records;
//! - no commit outcome is left **unresolved**: an `Indeterminate` that was never settled is a
//!   violation;
//! - sequences have no gap or duplicate over the committed allocations;
//! - the outbox holds every committed event exactly once and no event of a transaction that did
//!   not commit.
//!
//! # Why only created records are judged per operation
//!
//! A document's current version cannot say whether *this* operation applied: a later update
//! raises the version and a later delete removes the row, so a version comparison would both miss
//! a lost update and mistake a later write for an earlier one. The change feed is exact per
//! `(entity, id, op, version)`, but that key is **not unique to an operation**: under contention
//! two operations can attempt the same version of the same record (one commits, the other
//! retries or fails), so a loser's attempted version can equal the winner's committed version.
//! Only a record's **creation** carries an id unique to the operation, so the per-operation
//! atomicity check is made over created records; updated and deleted records are judged by the
//! aggregate invariants (document counts, version continuity) and the outbox markers, which the
//! scenarios run next to the audit.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_l7_invariants::history::{History, Outcome, Role};
use spot_l7_invariants::invariant::{CheckContext, Violation};
use spot_l7_invariants::oracle::{Oracle, change_code};
use spot_storage_contract::{ChangeOp, RecordId};

/// The audit's invariant id, for its violations.
pub const DURABILITY_AUDIT: &str = "durability-audit";

/// How the client's view of an operation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitClass {
    /// The commit returned success.
    Acknowledged,
    /// The store refused the operation; nothing happened.
    Refused,
    /// The operation failed after its retries.
    Failed,
    /// The commit outcome was unknown and a read-back resolved it as committed.
    IndeterminateCommitted,
    /// The commit outcome was unknown and a read-back resolved it as absent.
    IndeterminateAbsent,
    /// The commit outcome was unknown and never resolved: a violation.
    Unresolved,
}

impl CommitClass {
    /// The name used in the table.
    pub fn name(self) -> &'static str {
        match self {
            CommitClass::Acknowledged => "acknowledged",
            CommitClass::Refused => "refused",
            CommitClass::Failed => "failed",
            CommitClass::IndeterminateCommitted => "indeterminate_committed",
            CommitClass::IndeterminateAbsent => "indeterminate_absent",
            CommitClass::Unresolved => "unresolved",
        }
    }

    /// Every class, in table order.
    pub fn all() -> [CommitClass; 6] {
        [
            CommitClass::Acknowledged,
            CommitClass::Refused,
            CommitClass::Failed,
            CommitClass::IndeterminateCommitted,
            CommitClass::IndeterminateAbsent,
            CommitClass::Unresolved,
        ]
    }
}

/// The result of the audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    /// How many operations fell into each class.
    pub classification: BTreeMap<CommitClass, u64>,
    /// Whether the audit held.
    pub ok: bool,
    /// Every violation found.
    pub violations: Vec<Violation>,
}

impl AuditReport {
    /// The table of classes and counts.
    pub fn table(&self) -> String {
        let mut out = String::from("durability audit\n");
        for class in CommitClass::all() {
            let count = self.classification.get(&class).copied().unwrap_or(0);
            out.push_str(&format!("  {:<24} {count:>8}\n", class.name()));
        }
        if self.ok {
            out.push_str("  result: OK\n");
        } else {
            out.push_str(&format!(
                "  result: {} VIOLATION(S)\n",
                self.violations.len()
            ));
        }
        out
    }

    /// The ids of the invariants that failed (always [`DURABILITY_AUDIT`] here).
    pub fn failed_ids(&self) -> Vec<&str> {
        if self.ok {
            Vec::new()
        } else {
            vec![DURABILITY_AUDIT]
        }
    }
}

/// The audit.
pub struct DurabilityAudit;

impl DurabilityAudit {
    /// Counts the operations by [`CommitClass`]. Pure: it reads only the history.
    pub fn classify(history: &History) -> BTreeMap<CommitClass, u64> {
        let mut counts: BTreeMap<CommitClass, u64> = BTreeMap::new();
        for entry in &history.entries {
            *counts.entry(class_of(&entry.outcome)).or_insert(0) += 1;
        }
        counts
    }

    /// Checks the committed sequence allocations for gaps and duplicates. Pure: it reads only the
    /// oracle.
    pub fn sequence_gaps(oracle: &Oracle) -> Vec<Violation> {
        let mut out = Vec::new();
        for (series, number) in oracle.duplicate_numbers() {
            out.push(Violation::new(
                DURABILITY_AUDIT,
                "sequence",
                series,
                format!("duplicate_allocation number={number}"),
            ));
        }
        for (series, numbers) in oracle.series() {
            let sorted: std::collections::BTreeSet<u64> = numbers.iter().copied().collect();
            let count = sorted.len() as u64;
            for expected in 1..=count {
                if !sorted.contains(&expected) {
                    out.push(Violation::new(
                        DURABILITY_AUDIT,
                        "sequence",
                        series,
                        format!("gap_in_committed_allocations missing={expected} count={count}"),
                    ));
                    break;
                }
            }
        }
        out
    }

    /// Runs the full audit against the store.
    pub async fn run(ctx: &CheckContext) -> AuditReport {
        let mut violations = Vec::new();
        let history = ctx.history();
        let oracle = ctx.oracle();

        // Unresolved commits are always a violation.
        for entry in &history.entries {
            if matches!(entry.outcome, Outcome::Indeterminate { resolved: None }) {
                violations.push(Violation::new(
                    DURABILITY_AUDIT,
                    "operation",
                    format!("w{}#{}", entry.worker, entry.seq),
                    "unresolved_indeterminate_commit",
                ));
            }
        }

        violations.extend(Self::sequence_gaps(oracle));

        // The change feed: one net entry per committed document write, keyed by op and version.
        let mut actual: BTreeMap<(String, String, u8, u64), u64> = BTreeMap::new();
        match ctx.changes().await {
            Ok(changes) => {
                for c in &changes {
                    *actual
                        .entry((
                            c.document.entity.to_string(),
                            c.document.id.to_string(),
                            change_code(c.op),
                            c.version.0,
                        ))
                        .or_insert(0) += 1;
                }
            }
            Err(e) => violations.push(Violation::read_failed(DURABILITY_AUDIT, "changes", &e)),
        }

        // Per-operation atomicity over the change feed.
        for entry in &history.entries {
            let class = class_of(&entry.outcome);
            let committed = matches!(
                class,
                CommitClass::Acknowledged | CommitClass::IndeterminateCommitted
            );
            let label = format!("w{}#{}", entry.worker, entry.seq);
            let mut present = 0u32;
            let mut absent = 0u32;
            for t in &entry.touched {
                // Only a creation is judged per operation (see the module docs).
                if t.role != Role::Created {
                    continue;
                }
                let Some(version) = t.version else {
                    continue;
                };
                let key = (
                    t.entity.clone(),
                    t.id.clone(),
                    change_code(ChangeOp::Insert),
                    version,
                );
                let effect = actual.get(&key).copied().unwrap_or(0) > 0;
                if effect {
                    present += 1;
                } else {
                    absent += 1;
                }
                if committed && !effect {
                    violations.push(Violation::new(
                        DURABILITY_AUDIT,
                        &t.entity,
                        &t.id,
                        format!("acknowledged_effect_absent operation={label}"),
                    ));
                }
                if !committed && effect {
                    violations.push(Violation::new(
                        DURABILITY_AUDIT,
                        &t.entity,
                        &t.id,
                        format!("unacknowledged_effect_present operation={label}"),
                    ));
                }
            }
            if !committed && present > 0 && absent > 0 {
                violations.push(Violation::new(
                    DURABILITY_AUDIT,
                    "operation",
                    label,
                    format!("partial_transaction present={present} absent={absent}"),
                ));
            }
        }

        // Outbox: every committed event exactly once, no orphan.
        match ctx.outbox().await {
            Ok(records) => {
                let mut seen: BTreeMap<RecordId, u64> = BTreeMap::new();
                for r in &records {
                    *seen.entry(r.event.id).or_insert(0) += 1;
                }
                for (id, count) in &seen {
                    if *count > 1 {
                        violations.push(Violation::new(
                            DURABILITY_AUDIT,
                            "outbox",
                            id,
                            format!("duplicate_event copies={count}"),
                        ));
                    }
                }
                for id in oracle.events().keys() {
                    if !seen.contains_key(id) {
                        violations.push(Violation::new(
                            DURABILITY_AUDIT,
                            "outbox",
                            id,
                            "missing_committed_event",
                        ));
                    }
                }
                for id in seen.keys() {
                    if !oracle.events().contains_key(id) {
                        let what = if oracle.is_aborted_event(id) {
                            "orphan_event"
                        } else {
                            "unknown_event"
                        };
                        violations.push(Violation::new(DURABILITY_AUDIT, "outbox", id, what));
                    }
                }
            }
            Err(e) => violations.push(Violation::read_failed(DURABILITY_AUDIT, "outbox", &e)),
        }

        let classification = Self::classify(history);
        AuditReport {
            ok: violations.is_empty(),
            classification,
            violations,
        }
    }
}

/// The class of one outcome.
fn class_of(outcome: &Outcome) -> CommitClass {
    match outcome {
        Outcome::Committed => CommitClass::Acknowledged,
        Outcome::RolledBack | Outcome::Rejected { .. } => CommitClass::Refused,
        Outcome::Failed { .. } => CommitClass::Failed,
        Outcome::Indeterminate { resolved } => match resolved {
            Some(true) => CommitClass::IndeterminateCommitted,
            Some(false) => CommitClass::IndeterminateAbsent,
            None => CommitClass::Unresolved,
        },
    }
}
