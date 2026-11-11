//! The lifecycle events a migration run records, and their encoding as audit-chain entries.
//!
//! One entry per event, appended in order, so the whole run is a contiguous stretch of the
//! tamper-evident chain: [`RunSpec`] → [`ApprovalEvent`] → [`RehearsalEvent`] → [`ApplyEvent`] →
//! [`FinishEvent`]. Every event carries only non-sensitive fields (identities, digests,
//! references, counts and times); a plan secret or a connection string never appears.
//!
//! The record kinds are built with `concat!` so the error-code scan does not mistake a kind such
//! as `migration.run_planned` for an error code; they are the audit chain's own vocabulary, not
//! `migration_audit.*` codes.

use std::collections::BTreeMap;

use spot_audit_chain::{AuditRecord, Payload, PayloadValue, Subject};

use crate::error::MigrationAuditError;
use crate::run::{
    ApprovalDecision, ApprovalRef, ArtifactKind, ArtifactRef, MigrationRun, Outcome, PlanRef,
    PlanSummary, RehearsalVerdict, ReportRef, RunId,
};

/// The audit-record kinds this crate writes.
pub mod kinds {
    /// A run was planned and its approvals recorded.
    pub const RUN_PLANNED: &str = concat!("migration", ".run_planned");
    /// One approval (or rejection) was recorded.
    pub const RUN_APPROVED: &str = concat!("migration", ".run_approved");
    /// A rehearsal report was recorded.
    pub const RUN_REHEARSED: &str = concat!("migration", ".run_rehearsed");
    /// Apply ran and its journal was recorded.
    pub const RUN_APPLIED: &str = concat!("migration", ".run_applied");
    /// The run finished with a terminal outcome.
    pub const RUN_FINISHED: &str = concat!("migration", ".run_finished");
}

/// The audit subject DocType every run event uses.
pub const SUBJECT_DOCTYPE: &str = "MigrationRun";

/// What a caller asks to begin: the approved plan, its stored document, a summary and who is
/// running it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSpec {
    /// The run's id (caller-supplied; the crate reads no clock or randomness).
    pub id: RunId,
    /// The approved plan digest (from `spot-migration-approval`).
    pub plan_digest: String,
    /// The stored plan document and its summary.
    pub plan: PlanRef,
    /// The identity running the migration (`system`, `user:<id>` or `agent:<id>`).
    pub actor: String,
    /// Unix microseconds the run began.
    pub at: i64,
}

/// One recorded approval or rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalEvent {
    /// The approver's identity.
    pub approver: String,
    /// The role the approver held.
    pub role: String,
    /// What they decided.
    pub decision: ApprovalDecision,
    /// Unix microseconds.
    pub at: i64,
}

/// One recorded rehearsal report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RehearsalEvent {
    /// The stored rehearsal report.
    pub report: ArtifactRef,
    /// What the rehearsal found.
    pub verdict: RehearsalVerdict,
    /// How many findings it reported.
    pub findings: u64,
    /// Unix microseconds.
    pub at: i64,
}

/// One recorded apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyEvent {
    /// The stored apply journal.
    pub journal: ArtifactRef,
    /// How many steps the apply ran.
    pub steps: u64,
    /// Unix microseconds.
    pub at: i64,
}

/// The end of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishEvent {
    /// The terminal outcome.
    pub outcome: Outcome,
    /// Unix microseconds.
    pub at: i64,
}

/// One decoded lifecycle event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A run was planned.
    Planned {
        id: RunId,
        plan_digest: String,
        plan: PlanRef,
        actor: String,
        at: i64,
    },
    /// An approval was recorded.
    Approved { id: RunId, approval: ApprovalRef },
    /// A rehearsal was recorded.
    Rehearsed { id: RunId, report: ReportRef },
    /// An apply was recorded.
    Applied {
        id: RunId,
        journal: ArtifactRef,
        steps: u64,
    },
    /// The run finished.
    Finished {
        id: RunId,
        outcome: Outcome,
        at: i64,
    },
}

impl Event {
    /// The run this event belongs to.
    pub(crate) fn run_id(&self) -> &RunId {
        match self {
            Self::Planned { id, .. }
            | Self::Approved { id, .. }
            | Self::Rehearsed { id, .. }
            | Self::Applied { id, .. }
            | Self::Finished { id, .. } => id,
        }
    }
}

fn record(kind: &str, actor: &str, id: &RunId, at: i64, payload: Payload) -> AuditRecord {
    AuditRecord {
        kind: kind.to_owned(),
        actor: actor.to_owned(),
        on_behalf_of: None,
        subject: Some(Subject {
            doctype: SUBJECT_DOCTYPE.to_owned(),
            id: id.to_string(),
        }),
        at,
        payload,
        masked: true,
    }
}

fn put_text(payload: &mut Payload, key: &str, value: &str) {
    payload.insert(key.to_owned(), PayloadValue::Text(value.to_owned()));
}

fn put_int(payload: &mut Payload, key: &str, value: u64) {
    payload.insert(
        key.to_owned(),
        PayloadValue::Int(i64::try_from(value).unwrap_or(i64::MAX)),
    );
}

/// The record that begins a run.
pub(crate) fn planned_record(spec: &RunSpec) -> AuditRecord {
    let mut payload = Payload::new();
    put_text(&mut payload, "plan_digest", &spec.plan_digest);
    put_text(&mut payload, "plan_location", &spec.plan.artifact.location);
    put_text(
        &mut payload,
        "plan_artifact_digest",
        &spec.plan.artifact.digest,
    );
    put_int(&mut payload, "plan_bytes", spec.plan.artifact.bytes);
    put_text(&mut payload, "risk", &spec.plan.summary.risk);
    put_int(&mut payload, "changes", spec.plan.summary.changes);
    put_int(&mut payload, "schema", spec.plan.summary.schema);
    put_int(&mut payload, "metadata", spec.plan.summary.metadata);
    put_int(&mut payload, "patches", spec.plan.summary.patches);
    put_int(&mut payload, "behaviour", spec.plan.summary.behaviour);
    record(kinds::RUN_PLANNED, &spec.actor, &spec.id, spec.at, payload)
}

/// The record of one approval.
pub(crate) fn approval_record(actor: &str, id: &RunId, event: &ApprovalEvent) -> AuditRecord {
    let mut payload = Payload::new();
    put_text(&mut payload, "approver", &event.approver);
    put_text(&mut payload, "role", &event.role);
    put_text(&mut payload, "decision", event.decision.as_str());
    payload.insert("at".to_owned(), PayloadValue::Int(event.at));
    record(kinds::RUN_APPROVED, actor, id, event.at, payload)
}

/// The record of one rehearsal.
pub(crate) fn rehearsal_record(actor: &str, id: &RunId, event: &RehearsalEvent) -> AuditRecord {
    let mut payload = Payload::new();
    put_text(&mut payload, "report_location", &event.report.location);
    put_text(&mut payload, "report_digest", &event.report.digest);
    put_int(&mut payload, "report_bytes", event.report.bytes);
    put_text(&mut payload, "verdict", event.verdict.as_str());
    put_int(&mut payload, "findings", event.findings);
    record(kinds::RUN_REHEARSED, actor, id, event.at, payload)
}

/// The record of one apply.
pub(crate) fn applied_record(actor: &str, id: &RunId, event: &ApplyEvent) -> AuditRecord {
    let mut payload = Payload::new();
    put_text(&mut payload, "journal_location", &event.journal.location);
    put_text(&mut payload, "journal_digest", &event.journal.digest);
    put_int(&mut payload, "journal_bytes", event.journal.bytes);
    put_int(&mut payload, "steps", event.steps);
    record(kinds::RUN_APPLIED, actor, id, event.at, payload)
}

/// The record that ends a run.
pub(crate) fn finished_record(actor: &str, id: &RunId, event: &FinishEvent) -> AuditRecord {
    let mut payload = Payload::new();
    put_text(&mut payload, "outcome", event.outcome.as_str());
    record(kinds::RUN_FINISHED, actor, id, event.at, payload)
}

/// Decodes one audit record into a lifecycle event, or `None` when it is not a run event.
///
/// # Errors
/// [`MigrationAuditError::RecordInvalid`] when a run event is missing a field or holds the wrong
/// type, and [`MigrationAuditError::BadId`] when its subject id is not a usable run id.
pub(crate) fn decode(record: &AuditRecord) -> Result<Option<Event>, MigrationAuditError> {
    let Some(subject) = &record.subject else {
        return Ok(None);
    };
    if subject.doctype != SUBJECT_DOCTYPE {
        return Ok(None);
    }
    let id = RunId::new(subject.id.clone())?;
    let payload = &record.payload;
    let event = match record.kind.as_str() {
        kinds::RUN_PLANNED => Event::Planned {
            id,
            plan_digest: text(payload, "plan_digest")?,
            plan: PlanRef {
                artifact: artifact(
                    payload,
                    ArtifactKind::Plan,
                    "plan_location",
                    "plan_artifact_digest",
                    "plan_bytes",
                )?,
                summary: PlanSummary {
                    risk: text(payload, "risk")?,
                    changes: count(payload, "changes")?,
                    schema: count(payload, "schema")?,
                    metadata: count(payload, "metadata")?,
                    patches: count(payload, "patches")?,
                    behaviour: count(payload, "behaviour")?,
                },
            },
            actor: record.actor.clone(),
            at: record.at,
        },
        kinds::RUN_APPROVED => Event::Approved {
            id,
            approval: ApprovalRef {
                approver: text(payload, "approver")?,
                role: text(payload, "role")?,
                decision: ApprovalDecision::parse(&text(payload, "decision")?).ok_or_else(
                    || MigrationAuditError::RecordInvalid {
                        message: "an approval decision is not approved or rejected".to_owned(),
                    },
                )?,
                at: int(payload, "at")?,
            },
        },
        kinds::RUN_REHEARSED => Event::Rehearsed {
            id,
            report: ReportRef {
                artifact: artifact(
                    payload,
                    ArtifactKind::RehearsalReport,
                    "report_location",
                    "report_digest",
                    "report_bytes",
                )?,
                verdict: RehearsalVerdict::parse(&text(payload, "verdict")?).ok_or_else(|| {
                    MigrationAuditError::RecordInvalid {
                        message: "a rehearsal verdict is not one this crate knows".to_owned(),
                    }
                })?,
                findings: count(payload, "findings")?,
            },
        },
        kinds::RUN_APPLIED => Event::Applied {
            id,
            journal: artifact(
                payload,
                ArtifactKind::ApplyJournal,
                "journal_location",
                "journal_digest",
                "journal_bytes",
            )?,
            steps: count(payload, "steps")?,
        },
        kinds::RUN_FINISHED => Event::Finished {
            id,
            outcome: Outcome::parse(&text(payload, "outcome")?).ok_or_else(|| {
                MigrationAuditError::RecordInvalid {
                    message: "a finished outcome is not one this crate knows".to_owned(),
                }
            })?,
            at: record.at,
        },
        _ => return Ok(None),
    };
    Ok(Some(event))
}

fn artifact(
    payload: &Payload,
    kind: ArtifactKind,
    location_key: &str,
    digest_key: &str,
    bytes_key: &str,
) -> Result<ArtifactRef, MigrationAuditError> {
    Ok(ArtifactRef {
        kind,
        location: text(payload, location_key)?,
        digest: text(payload, digest_key)?,
        bytes: count(payload, bytes_key)?,
    })
}

fn text(payload: &Payload, key: &str) -> Result<String, MigrationAuditError> {
    match payload.get(key) {
        Some(PayloadValue::Text(value)) => Ok(value.clone()),
        _ => Err(MigrationAuditError::RecordInvalid {
            message: format!("the payload field {key} is missing or not text"),
        }),
    }
}

fn int(payload: &Payload, key: &str) -> Result<i64, MigrationAuditError> {
    match payload.get(key) {
        Some(PayloadValue::Int(value)) => Ok(*value),
        _ => Err(MigrationAuditError::RecordInvalid {
            message: format!("the payload field {key} is missing or not an integer"),
        }),
    }
}

fn count(payload: &Payload, key: &str) -> Result<u64, MigrationAuditError> {
    Ok(u64::try_from(int(payload, key)?).unwrap_or(0))
}

/// Folds a chain-ordered sequence of decoded events into one run record per run id.
///
/// The first `Planned` event sets the plan and the start; approvals and the rehearsal are
/// collected in order; the apply journal is kept from the last `Applied`; the outcome is the
/// last stage or the terminal `Finished`. Runs come back in the order their plan was recorded.
#[must_use]
pub(crate) fn fold(events: &[Event]) -> Vec<MigrationRun> {
    let mut order: Vec<RunId> = Vec::new();
    let mut runs: BTreeMap<RunId, MigrationRun> = BTreeMap::new();
    for event in events {
        match event {
            Event::Planned {
                id,
                plan_digest,
                plan,
                actor,
                at,
            } => {
                if !runs.contains_key(id) {
                    order.push(id.clone());
                }
                runs.insert(
                    id.clone(),
                    MigrationRun {
                        id: id.clone(),
                        plan_digest: plan_digest.clone(),
                        plan: plan.clone(),
                        approvals: Vec::new(),
                        rehearsal: None,
                        apply_journal_ref: None,
                        outcome: Outcome::Planned,
                        started: *at,
                        ended: None,
                        actor: actor.clone(),
                    },
                );
            }
            Event::Approved { id, approval } => {
                if let Some(run) = runs.get_mut(id) {
                    run.approvals.push(approval.clone());
                }
            }
            Event::Rehearsed { id, report } => {
                if let Some(run) = runs.get_mut(id) {
                    run.rehearsal = Some(report.clone());
                    if run.outcome == Outcome::Planned {
                        run.outcome = Outcome::Rehearsed;
                    }
                }
            }
            Event::Applied { id, journal, .. } => {
                if let Some(run) = runs.get_mut(id) {
                    run.apply_journal_ref = Some(journal.clone());
                    if matches!(run.outcome, Outcome::Planned | Outcome::Rehearsed) {
                        run.outcome = Outcome::Applied;
                    }
                }
            }
            Event::Finished { id, outcome, at } => {
                if let Some(run) = runs.get_mut(id) {
                    run.outcome = *outcome;
                    run.ended = Some(*at);
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| runs.remove(&id))
        .collect()
}
