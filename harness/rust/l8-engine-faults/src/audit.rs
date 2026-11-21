//! The engine durability audit: what the client was told versus what the store holds.
//!
//! | Rule | Checks |
//! | --- | --- |
//! | a | every committed attempt is visible after recovery at exactly the version the run last gave it |
//! | b | every committed operation left the lifecycle events it must, each carrying its command id exactly as often as it should, and no refused or never-sent command left one |
//! | c | a posted document never lacks the number that names it |
//! | d | no two committed creations share a number |
//! | e | every re-send equals the original result (a replay) or is a first run, never a second effect, a conflict with the first attempt's own write or a different result |
//! | f | the version history of every document is continuous |
//!
//! Plus two findings that are not rules but failures: an attempt that was never settled, and an
//! error the classifier could not place. No finding contains a document value.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::DocState;
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::RecordId;
use spot_events_contract::PayloadDocument;
use spot_l7_engine::world::World;
use spot_storage_contract::OutboxRecord;

use crate::classify::{Class, classify};
use crate::log::{Attempt, Done, OpKind, Outcome, Resolution, Seen, Settled};

/// One thing the audit found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// The rule: `a` to `f`, `u` (unresolved) or `x` (unclassified).
    pub rule: char,
    /// What it is about: a command id or a document id.
    pub subject: String,
    /// What is wrong, without values.
    pub detail: String,
}

fn finding(rule: char, subject: impl ToString, detail: impl Into<String>) -> Finding {
    Finding {
        rule,
        subject: subject.to_string(),
        detail: detail.into(),
    }
}

/// Whether the operation had already committed when the re-send was made, given the read-back.
fn committed_before(a: &Attempt, before: &Option<Seen>) -> bool {
    match a.op {
        OpKind::CreateTask | OpKind::CreateTe => before.is_some(),
        OpKind::DeleteTe => before.is_none(),
        OpKind::CorrectTe => before.is_some_and(|b| b.state == Some(DocState::Cancelled)),
        OpKind::DeniedPost => false,
        _ => before.is_some_and(|b| Some(b.version) > a.expected),
    }
}

/// Rule e on one resolved attempt.
fn rule_e(a: &Attempt, r: &Resolution, out: &mut Vec<Finding>) {
    let was = committed_before(a, &r.before);
    match &r.settled {
        Settled::Unresolved => out.push(finding('u', &a.command_id, "never settled")),
        Settled::Refused(code) if was => out.push(finding(
            'e',
            &a.command_id,
            format!("the command had committed, but the re-send was refused: {code}"),
        )),
        Settled::Refused(_) => {}
        Settled::Committed(done) => {
            let first_run_version = match a.op {
                OpKind::CreateTask | OpKind::CreateTe | OpKind::CorrectTe => 1,
                OpKind::DeleteTe => a.expected.unwrap_or(0),
                _ => a.expected.unwrap_or(0) + 1,
            };
            if was {
                // A replay: the same document, and for a write that leaves it, the version the
                // read-back showed (one writer per document, so nothing moved it since).
                let leaves_doc = !matches!(a.op, OpKind::DeleteTe | OpKind::CorrectTe);
                if leaves_doc && r.before.is_some_and(|b| b.version != done.seen.version) {
                    out.push(finding(
                        'e',
                        &a.command_id,
                        "the replay answered another version than the original",
                    ));
                }
                if matches!(a.op, OpKind::CreateTask | OpKind::CreateTe) && done.doc != a.target {
                    out.push(finding(
                        'e',
                        &a.command_id,
                        "the replay answered another document than the original",
                    ));
                }
            } else if done.seen.version != first_run_version {
                out.push(finding(
                    'e',
                    &a.command_id,
                    format!(
                        "a first run gave version {}, expected {first_run_version}",
                        done.seen.version
                    ),
                ));
            }
        }
    }
}

/// Rule f: walks the committed attempts of each worker in order and checks that every write on a
/// document starts from the version the last one left.
fn rule_f(attempts: &[Attempt], out: &mut Vec<Finding>) {
    let mut version: BTreeMap<RecordId, u64> = BTreeMap::new();
    for a in attempts {
        let Some(done) = a.committed() else { continue };
        match a.op {
            OpKind::CreateTask | OpKind::CreateTe => {
                if done.seen.version != 1 {
                    out.push(finding(
                        'f',
                        done.doc,
                        format!("created at version {}", done.seen.version),
                    ));
                }
                version.insert(done.doc, done.seen.version);
            }
            OpKind::DeleteTe => {
                version.remove(&a.target);
            }
            OpKind::CorrectTe => {
                let known = version.get(&a.target).copied();
                if known.is_some() && known != a.expected {
                    out.push(finding(
                        'f',
                        a.target,
                        "a correction started from a version the document did not have",
                    ));
                }
                version.insert(a.target, a.expected.unwrap_or(0) + 1);
                version.insert(done.doc, done.seen.version);
            }
            _ => {
                let known = version.get(&a.target).copied();
                if known.is_some() && known != a.expected {
                    out.push(finding(
                        'f',
                        a.target,
                        "a write started from a version the document did not have",
                    ));
                }
                if Some(done.seen.version) != a.expected.map(|v| v + 1) {
                    out.push(finding(
                        'f',
                        a.target,
                        format!(
                            "a write answered version {} after expecting {:?}",
                            done.seen.version, a.expected
                        ),
                    ));
                }
                version.insert(a.target, done.seen.version);
            }
        }
    }
}

/// How a call ended, as the call ledger records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    /// The commit is known: it committed (first answer or after the re-send).
    Acked,
    /// The call was refused or denied cleanly, or its re-send was.
    Failed,
    /// The commit state was never settled.
    Unknown,
}

impl CallOutcome {
    /// The word the ledger prints.
    pub fn as_str(self) -> &'static str {
        match self {
            CallOutcome::Acked => "acked",
            CallOutcome::Failed => "failed",
            CallOutcome::Unknown => "unknown",
        }
    }
}

/// One row of the call ledger: the method, the document and how the call ended. Names and ids
/// only, never a field value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRecord {
    /// The command id of the call.
    pub command_id: String,
    /// The engine method (`create`, `post`, `call_method:touch`, ...).
    pub method: &'static str,
    /// The document key the call was made on, as text.
    pub document: String,
    /// How it ended.
    pub outcome: CallOutcome,
}

/// The ledger of every call of the run, method calls included (the command ledger covers writes
/// only; this one covers every call the workload made).
pub fn call_ledger(attempts: &[Attempt]) -> Vec<CallRecord> {
    attempts
        .iter()
        .map(|a| {
            let outcome = if a.committed().is_some() {
                CallOutcome::Acked
            } else if a.unresolved() {
                CallOutcome::Unknown
            } else {
                CallOutcome::Failed
            };
            CallRecord {
                command_id: a.command_id.clone(),
                method: a.op.method(),
                document: a.target.to_string(),
                outcome,
            }
        })
        .collect()
}

/// Names the method (and document) on every finding that is about a call or a document of the
/// log: a finding whose subject is a command id gets that call's method; one whose subject is a
/// document id gets the methods that were called on it. Idempotent.
pub fn annotate(findings: Vec<Finding>, attempts: &[Attempt]) -> Vec<Finding> {
    let ledger = call_ledger(attempts);
    findings
        .into_iter()
        .map(|mut f| {
            if f.detail.contains("(method ") {
                return f;
            }
            if let Some(c) = ledger.iter().find(|c| c.command_id == f.subject) {
                f.detail = format!(
                    "{} (method {}, document {}, call {})",
                    f.detail,
                    c.method,
                    c.document,
                    c.outcome.as_str()
                );
            } else {
                let mut methods: Vec<String> = ledger
                    .iter()
                    .filter(|c| c.document == f.subject)
                    .map(|c| format!("{} {}", c.method, c.outcome.as_str()))
                    .collect();
                methods.dedup();
                if !methods.is_empty() {
                    f.detail = format!("{} (method {})", f.detail, methods.join(", "));
                }
            }
            f
        })
        .collect()
}

/// The rules that need only the log: e, f, and the two failures. Every finding names its method.
pub fn audit_log(attempts: &[Attempt]) -> Vec<Finding> {
    let mut out = Vec::new();
    for a in attempts {
        if let Outcome::Refused(code) = &a.outcome
            && code.starts_with("unclassified:")
        {
            out.push(finding('x', &a.command_id, code.clone()));
        }
        if a.unresolved() {
            out.push(finding(
                'u',
                &a.command_id,
                "an indeterminate commit was never settled",
            ));
        } else if let Some(r) = &a.resolution {
            rule_e(a, r, &mut out);
        }
    }
    rule_f(attempts, &mut out);
    annotate(out, attempts)
}

/// The version and state each document must have in the store after the run.
fn expected_final(attempts: &[Attempt]) -> BTreeMap<RecordId, Option<(u64, Option<DocState>)>> {
    let mut want: BTreeMap<RecordId, Option<(u64, Option<DocState>)>> = BTreeMap::new();
    for a in attempts {
        let Some(done) = a.committed() else { continue };
        match a.op {
            OpKind::DeleteTe => {
                want.insert(a.target, None);
            }
            OpKind::CorrectTe => {
                want.insert(
                    a.target,
                    Some((a.expected.unwrap_or(0) + 1, Some(DocState::Cancelled))),
                );
                want.insert(done.doc, Some((done.seen.version, Some(DocState::Draft))));
            }
            OpKind::CreateTask | OpKind::CreateTe => {
                want.insert(done.doc, Some((done.seen.version, done.seen.state)));
            }
            _ => {
                want.insert(a.target, Some((done.seen.version, done.seen.state)));
            }
        }
    }
    want
}

/// Rules a, c and d against the store, through the engine's own read path.
pub async fn audit_documents(
    service: &dyn DocumentService,
    world: &World,
    attempts: &[Attempt],
) -> Vec<Finding> {
    let mut out = Vec::new();
    let inv = world.inv("system");
    let tasks: BTreeSet<RecordId> = attempts
        .iter()
        .filter(|a| a.op == OpKind::CreateTask)
        .map(|a| a.target)
        .collect();
    let mut numbers: BTreeMap<String, RecordId> = BTreeMap::new();
    for (id, want) in expected_final(attempts) {
        let doctype = if tasks.contains(&id) {
            spot_l7_engine::world::TASK
        } else {
            spot_l7_engine::world::TIME_ENTRY
        };
        let found = service.load(&inv, doctype, id).await;
        match (want, found) {
            (None, Err(e)) if matches!(classify(&e), Class::Refused(_)) => {}
            (None, Ok(_)) => out.push(finding('a', id, "a committed delete left the document")),
            (Some(_), Err(e)) => out.push(finding(
                'a',
                id,
                format!("a committed document is missing: {}", e.code),
            )),
            (None, Err(e)) => out.push(finding('a', id, format!("read failed: {}", e.code))),
            (Some((version, state)), Ok(out_doc)) => {
                let doc = out_doc.value;
                if doc.version().map(|v| v.0) != Some(version) {
                    out.push(finding(
                        'a',
                        id,
                        format!(
                            "version {:?}, expected {version}",
                            doc.version().map(|v| v.0)
                        ),
                    ));
                }
                if doctype == spot_l7_engine::world::TIME_ENTRY {
                    if state.is_some() && doc.state() != state {
                        out.push(finding(
                            'a',
                            id,
                            format!("state {:?}, expected {state:?}", doc.state()),
                        ));
                    }
                    let number = doc
                        .text("entry_no")
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                        .to_owned();
                    if doc.state() == Some(DocState::Posted) && number.is_empty() {
                        out.push(finding('c', id, "posted without its number"));
                    }
                    // A corrected entry is cancelled and its amended copy carries the same
                    // number (the sample names entries by a plain field, not a series): a
                    // cancelled entry no longer holds its number.
                    if !number.is_empty()
                        && doc.state() != Some(DocState::Cancelled)
                        && let Some(first) = numbers.insert(number, id)
                    {
                        out.push(finding('d', id, format!("shares its number with {first}")));
                    }
                }
            }
        }
    }
    out
}

/// Rule b: the lifecycle events of the run, counted per command id.
pub fn audit_events(outbox: &[OutboxRecord], attempts: &[Attempt]) -> Vec<Finding> {
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    // Only the lifecycle topics count (the same set the L7 events invariant uses): the engine's
    // other events (change capture, job and audit topics) carry a command id too.
    let lifecycle = |topic: &str| {
        spot_events_contract::lifecycle::Operation::ALL
            .iter()
            .any(|o| o.topic_name() == topic)
    };
    for rec in outbox {
        if !lifecycle(rec.event.topic.as_str()) {
            continue;
        }
        if let Ok(doc) = PayloadDocument::from_outbox_event(&rec.event)
            && let Some(id) = doc.meta.command_id
        {
            *seen.entry(id.as_str().to_owned()).or_insert(0) += 1;
        }
    }
    let mut out = Vec::new();
    for a in attempts {
        let want: u32 = match (a.committed(), a.op) {
            (Some(_), OpKind::CorrectTe) => 2,
            (Some(_), _) => 1,
            (None, _) => 0,
        };
        let got = seen.get(&a.command_id).copied().unwrap_or(0);
        if got != want {
            out.push(finding(
                'b',
                &a.command_id,
                format!("{got} events, expected {want}"),
            ));
        }
    }
    out
}

/// A count per way an attempt ended, for the table.
pub fn classification(attempts: &[Attempt]) -> BTreeMap<&'static str, u64> {
    let mut m: BTreeMap<&'static str, u64> = BTreeMap::new();
    for a in attempts {
        let key = match (&a.outcome, &a.resolution) {
            (Outcome::Committed(_), _) => "committed",
            (Outcome::Refused(_), _) => "refused",
            (Outcome::Denied(_), _) => "denied",
            (
                Outcome::Indeterminate(_),
                Some(Resolution {
                    settled: Settled::Committed(_),
                    ..
                }),
            ) => "indeterminate_committed",
            (
                Outcome::Indeterminate(_),
                Some(Resolution {
                    settled: Settled::Refused(_),
                    ..
                }),
            ) => "indeterminate_refused",
            (Outcome::Indeterminate(_), _) => "unresolved",
        };
        *m.entry(key).or_insert(0) += 1;
    }
    m
}

/// The table of classes and findings.
pub fn table(attempts: &[Attempt], findings: &[Finding]) -> String {
    let mut out = String::from("engine durability audit\n");
    for (k, v) in classification(attempts) {
        out.push_str(&format!("  {k:<26} {v:>8}\n"));
    }
    if findings.is_empty() {
        out.push_str("  result: OK\n");
    } else {
        out.push_str(&format!("  result: {} FINDING(S)\n", findings.len()));
        for f in annotate(findings.to_vec(), attempts).iter().take(20) {
            out.push_str(&format!("    {} {} {}\n", f.rule, f.subject, f.detail));
        }
    }
    out
}

/// Whether a committed attempt's result exists (used by the scenarios' own checks).
pub fn committed_done(a: &Attempt) -> Option<&Done> {
    a.committed()
}
