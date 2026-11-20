//! The seeded workload driver: every call carries a unique command id, every indeterminate call
//! is re-sent with the same id and the same arguments until it settles.
//!
//! **One writer per document.** A worker only touches the Task and Time Entries it created, so
//! after a re-send the replayed document is the document as that one call left it, and the audit
//! can compare versions exactly. The engine's behaviour under contention is `spot-l7-engine`'s
//! job; this crate attacks durability.

use std::sync::Arc;
use std::time::{Duration, Instant};

use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, Date, Decimal, Month, Name, RecordId, Value, Version};
use spot_app_api::{AppResult, DocState, Document, DocumentPatch};
use spot_l7_engine::world::{TASK, TIME_ENTRY, World};
use spot_l7_invariants::schedule::Rng;

use crate::classify::{Class, classify};
use crate::log::{
    Attempt, AttemptLog, Done, OpKind, Outcome, Resolution, Seen, Settled, command_id,
};

/// What a worker runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The mixed lifecycle workload.
    Mixed,
    /// Create, post, correct, again and again (the cancel-and-amend scenario).
    CorrectCycle,
}

/// Whether the workload cheats on purpose, for the honesty tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Honesty {
    /// The real workload.
    Honest,
    /// An indeterminate answer is treated as refused and never re-sent.
    NeverResend,
}

/// How a run is configured.
#[derive(Clone, Debug)]
pub struct WorkloadConfig {
    /// The seed: the plan of every worker is a pure function of it.
    pub seed: u64,
    /// Concurrent workers.
    pub workers: u32,
    /// Calls per worker.
    pub ops_per_worker: u64,
    /// The mix.
    pub mode: Mode,
    /// Honest or cheating.
    pub honesty: Honesty,
    /// How long one indeterminate call may take to settle.
    pub settle_deadline: Duration,
    /// The pause between two sends of the same command.
    pub backoff: Duration,
    /// The person every Time Entry names.
    pub person: RecordId,
}

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("a name: {e:?}"))
}

fn date() -> Date {
    Date::from_calendar_date(2026, Month::January, 15).unwrap_or_else(|e| panic!("a date: {e:?}"))
}

fn seen(doc: &Document) -> Seen {
    Seen {
        version: doc.version().map_or(1, |v| v.0),
        state: doc.state(),
    }
}

/// One call, fixed before the first send so a re-send is byte for byte the same.
#[derive(Clone)]
pub struct Plan {
    /// The operation.
    pub op: OpKind,
    /// The principal.
    pub actor: &'static str,
    /// The DocType.
    pub doctype: &'static str,
    /// The target (the new id for a create).
    pub target: RecordId,
    /// The expected version of an existing target.
    pub expected: Option<u64>,
    /// The document of a create.
    pub doc: Option<Document>,
    /// The patch of an update.
    pub patch: Option<DocumentPatch>,
}

fn invocation(world: &World, actor: &str, command: &str) -> Invocation {
    let mut inv = world.inv(actor);
    inv.command_id = CommandId::new(command).ok();
    inv
}

/// Sends one plan once.
pub async fn send(
    service: &dyn DocumentService,
    world: &World,
    plan: &Plan,
    command: &str,
) -> AppResult<Done> {
    let inv = invocation(world, plan.actor, command);
    let expected = Version(plan.expected.unwrap_or(1));
    let done = |doc: &Document| Done {
        doc: doc.id(),
        seen: seen(doc),
    };
    match plan.op {
        OpKind::CreateTask | OpKind::CreateTe => {
            let doc = plan
                .doc
                .clone()
                .unwrap_or_else(|| Document::new(n(plan.doctype), plan.target));
            Ok(done(&service.create(&inv, doc).await?.value))
        }
        OpKind::UpdateTe => {
            let patch = plan.patch.clone().unwrap_or_default();
            Ok(done(
                &service
                    .update(&inv, plan.doctype, plan.target, patch, expected)
                    .await?
                    .value,
            ))
        }
        OpKind::PostTe | OpKind::DeniedPost => Ok(done(
            &service
                .post(&inv, plan.doctype, plan.target, expected)
                .await?
                .value,
        )),
        OpKind::CancelTe => Ok(done(
            &service
                .cancel(&inv, plan.doctype, plan.target, expected)
                .await?
                .value,
        )),
        OpKind::CorrectTe => Ok(done(
            &service
                .correct(&inv, plan.doctype, plan.target, expected)
                .await?
                .value,
        )),
        OpKind::DeleteTe => {
            service
                .delete(&inv, plan.doctype, plan.target, expected)
                .await?;
            Ok(Done {
                doc: plan.target,
                seen: Seen {
                    version: expected.0,
                    state: None,
                },
            })
        }
        OpKind::TouchTask => {
            let out = service
                .call_method(
                    &inv,
                    plan.doctype,
                    plan.target,
                    "touch",
                    serde_json::Value::Null,
                    Some(expected),
                )
                .await?;
            Ok(done(&out.value.0))
        }
    }
}

/// A read-back as the system principal: `Ok(None)` when the document is absent.
async fn read_back(
    service: &dyn DocumentService,
    world: &World,
    plan: &Plan,
) -> AppResult<Option<Seen>> {
    let inv = world.inv("system");
    match service.load(&inv, plan.doctype, plan.target).await {
        Ok(out) => Ok(Some(seen(&out.value))),
        Err(e) if matches!(classify(&e), Class::Refused(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Settles one indeterminate call: re-sends the same command until the engine answers for good.
async fn settle(
    service: &dyn DocumentService,
    world: &World,
    plan: &Plan,
    command: &str,
    cfg: &WorkloadConfig,
) -> Resolution {
    let start = Instant::now();
    let mut resends = 0;
    let mut before = None;
    while start.elapsed() < cfg.settle_deadline {
        // The read-back comes first, so the audit knows what the store held before the re-send.
        match read_back(service, world, plan).await {
            Ok(b) => before = b,
            Err(_) => {
                tokio::time::sleep(cfg.backoff).await;
                continue;
            }
        }
        if !plan.op.ledger_covered() {
            // A method call has no ledger row: it is settled by the read-back, never re-sent when
            // it may have committed (the worker is the only writer of its Task).
            let committed = before
                .as_ref()
                .is_some_and(|b| Some(b.version) > plan.expected);
            if committed {
                let b = before.unwrap_or(Seen {
                    version: 0,
                    state: None,
                });
                return Resolution {
                    before,
                    settled: Settled::Committed(Done {
                        doc: plan.target,
                        seen: b,
                    }),
                    resends,
                };
            }
        }
        resends += 1;
        match send(service, world, plan, command).await {
            Ok(done) => {
                return Resolution {
                    before,
                    settled: Settled::Committed(done),
                    resends,
                };
            }
            Err(e) => match classify(&e) {
                Class::Indeterminate(_) => tokio::time::sleep(cfg.backoff).await,
                Class::Refused(code) | Class::Denied(code) | Class::Unclassified(code) => {
                    return Resolution {
                        before,
                        settled: Settled::Refused(code),
                        resends,
                    };
                }
                Class::Committed => {}
            },
        }
    }
    Resolution {
        before,
        settled: Settled::Unresolved,
        resends,
    }
}

#[derive(Clone, Copy)]
struct Te {
    id: RecordId,
    version: u64,
    state: DocState,
}

struct Local {
    task: Option<(RecordId, u64)>,
    tes: Vec<Te>,
}

fn new_te(person: RecordId, task: RecordId) -> Document {
    let mut doc = Document::new(n(TIME_ENTRY), RecordId::new_v7());
    doc.set(n("entry_no"), Value::Text(format!("TE-L8-{}", doc.id())));
    doc.set(n("task"), Value::Id(task));
    doc.set(n("person"), Value::Id(person));
    doc.set(n("work_date"), Value::Date(date()));
    doc.set(n("hours"), Value::Decimal(Decimal::new(2, 0)));
    doc.set(n("billable"), Value::Bool(true));
    doc
}

fn pick(rng: &mut Rng, tes: &[Te], state: DocState) -> Option<Te> {
    let matching: Vec<Te> = tes.iter().copied().filter(|t| t.state == state).collect();
    if matching.is_empty() {
        None
    } else {
        matching
            .get(rng.below(matching.len() as u64) as usize)
            .copied()
    }
}

fn plan_for(
    cfg: &WorkloadConfig,
    local: &Local,
    rng: &mut Rng,
    seq: u64,
    project: RecordId,
) -> Plan {
    let create_te = |local: &Local| -> Plan {
        let task = local.task.map_or_else(RecordId::new_v7, |t| t.0);
        let doc = new_te(cfg.person, task);
        Plan {
            op: OpKind::CreateTe,
            actor: "manager",
            doctype: TIME_ENTRY,
            target: doc.id(),
            expected: None,
            doc: Some(doc),
            patch: None,
        }
    };
    if local.task.is_none() {
        let mut doc = Document::new(n(TASK), RecordId::new_v7());
        doc.set(n("subject"), Value::Text(format!("L8 task {}", doc.id())));
        doc.set(n("project"), Value::Id(project));
        doc.set(n("status"), Value::Text("Open".to_owned()));
        doc.set(n("priority"), Value::Text("Medium".to_owned()));
        return Plan {
            op: OpKind::CreateTask,
            actor: "manager",
            doctype: TASK,
            target: doc.id(),
            expected: None,
            doc: Some(doc),
            patch: None,
        };
    }
    let on = |op, actor, te: Te| Plan {
        op,
        actor,
        doctype: TIME_ENTRY,
        target: te.id,
        expected: Some(te.version),
        doc: None,
        patch: None,
    };
    let denied = |local: &Local| -> Option<Plan> {
        local.task.map(|(id, v)| Plan {
            op: OpKind::DeniedPost,
            actor: "viewer",
            doctype: TASK,
            target: id,
            expected: Some(v),
            doc: None,
            patch: None,
        })
    };
    if cfg.mode == Mode::CorrectCycle {
        return match seq % 4 {
            0 => create_te(local),
            1 => match pick(rng, &local.tes, DocState::Draft) {
                Some(te) => on(OpKind::PostTe, "manager", te),
                None => create_te(local),
            },
            2 => match pick(rng, &local.tes, DocState::Posted) {
                Some(te) => on(OpKind::CorrectTe, "manager", te),
                None => create_te(local),
            },
            // One refusal per cycle: the L7 permission invariant needs a denial to judge.
            // A viewer posting a time entry lacks the right (posting a task is a state refusal,
            // which the permission invariant cannot judge).
            _ => pick(rng, &local.tes, DocState::Draft)
                .or_else(|| pick(rng, &local.tes, DocState::Posted))
                .map(|t| on(OpKind::DeniedPost, "viewer", t))
                .or_else(|| denied(local))
                .unwrap_or_else(|| create_te(local)),
        };
    }
    // The third call of every worker is a refusal the permission invariant can judge.
    let roll = if seq == 3 { 92 } else { rng.below(100) };
    let chosen = match roll {
        0..=29 => None,
        30..=44 => pick(rng, &local.tes, DocState::Draft).map(|t| {
            let mut p = on(OpKind::UpdateTe, "manager", t);
            let mut patch = DocumentPatch::default();
            patch
                .fields
                .insert(n("hours"), Value::Decimal(Decimal::new(3, 0)));
            p.patch = Some(patch);
            p
        }),
        45..=64 => pick(rng, &local.tes, DocState::Draft).map(|t| on(OpKind::PostTe, "manager", t)),
        65..=69 => {
            pick(rng, &local.tes, DocState::Posted).map(|t| on(OpKind::CancelTe, "manager", t))
        }
        70..=79 => {
            pick(rng, &local.tes, DocState::Posted).map(|t| on(OpKind::CorrectTe, "manager", t))
        }
        80..=84 => {
            pick(rng, &local.tes, DocState::Draft).map(|t| on(OpKind::DeleteTe, "manager", t))
        }
        85..=89 => local.task.map(|(id, v)| Plan {
            op: OpKind::TouchTask,
            actor: "manager",
            doctype: TASK,
            target: id,
            expected: Some(v),
            doc: None,
            patch: None,
        }),
        90..=94 => {
            pick(rng, &local.tes, DocState::Draft).map(|t| on(OpKind::DeniedPost, "viewer", t))
        }
        // A call in the wrong state: refused cleanly, never committed.
        _ => pick(rng, &local.tes, DocState::Draft).map(|t| on(OpKind::CancelTe, "manager", t)),
    };
    chosen.unwrap_or_else(|| create_te(local))
}

fn apply(local: &mut Local, plan: &Plan, done: &Done) {
    match plan.op {
        OpKind::CreateTask => local.task = Some((done.doc, done.seen.version)),
        OpKind::CreateTe => local.tes.push(Te {
            id: done.doc,
            version: done.seen.version,
            state: DocState::Draft,
        }),
        OpKind::UpdateTe | OpKind::PostTe | OpKind::CancelTe => {
            if let Some(t) = local.tes.iter_mut().find(|t| t.id == plan.target) {
                t.version = done.seen.version;
                t.state = done.seen.state.unwrap_or(t.state);
            }
        }
        OpKind::CorrectTe => {
            if let Some(t) = local.tes.iter_mut().find(|t| t.id == plan.target) {
                t.version = plan.expected.unwrap_or(0) + 1;
                t.state = DocState::Cancelled;
            }
            local.tes.push(Te {
                id: done.doc,
                version: done.seen.version,
                state: DocState::Draft,
            });
        }
        OpKind::DeleteTe => local.tes.retain(|t| t.id != plan.target),
        OpKind::TouchTask => {
            if let Some(t) = local.task.as_mut() {
                t.1 = done.seen.version;
            }
        }
        OpKind::DeniedPost => {}
    }
}

/// Runs one worker to the end and logs every call.
pub async fn run_worker(
    service: Arc<dyn DocumentService>,
    world: Arc<World>,
    cfg: Arc<WorkloadConfig>,
    project: RecordId,
    worker: u32,
    log: Arc<AttemptLog>,
) {
    let mut rng = Rng::new(cfg.seed ^ (u64::from(worker) + 1).wrapping_mul(0xA076_1D64_78BD_642F));
    let mut local = Local {
        task: None,
        tes: Vec::new(),
    };
    for seq in 0..cfg.ops_per_worker {
        let plan = plan_for(&cfg, &local, &mut rng, seq, project);
        let command = command_id(cfg.seed, worker, seq);
        let (outcome, resolution) = match send(service.as_ref(), &world, &plan, &command).await {
            Ok(done) => (Outcome::Committed(done), None),
            Err(e) => match classify(&e) {
                Class::Committed => (Outcome::Refused("unexpected".into()), None),
                Class::Refused(code) => (Outcome::Refused(code), None),
                Class::Denied(code) => (Outcome::Denied(code), None),
                Class::Unclassified(code) => {
                    (Outcome::Refused(format!("unclassified:{code}")), None)
                }
                Class::Indeterminate(code) => {
                    let resolution = if cfg.honesty == Honesty::NeverResend {
                        None
                    } else {
                        Some(settle(service.as_ref(), &world, &plan, &command, &cfg).await)
                    };
                    (Outcome::Indeterminate(code), resolution)
                }
            },
        };
        let attempt = Attempt {
            command_id: command,
            worker,
            seq,
            op: plan.op,
            actor: plan.actor,
            doctype: plan.doctype,
            target: plan.target,
            expected: plan.expected,
            outcome,
            resolution,
        };
        if let Some(done) = attempt.committed() {
            apply(&mut local, &plan, done);
        } else if attempt.unresolved() {
            // Nothing more is known about this document: leave it alone.
            local.tes.retain(|t| t.id != plan.target);
        }
        log.push(attempt);
    }
}
