//! Builds the L7 engine oracle ([`EngineRun`]) from the resolved attempt log, so the L7 engine
//! invariants judge exactly what the run says committed.

use std::collections::BTreeSet;

use spot_app_api::DocState;
use spot_app_api::prelude::RecordId;
use spot_l7_engine::driver::{
    DeniedOp, EngineRun, ExpectedDoc, ExpectedEvent, Op, OpOutcome, OpRecord,
};
use spot_l7_engine::world::{PROJECT, TIME_ENTRY, USER};

use crate::log::{Attempt, Done, OpKind, Outcome, Resolution, Settled};

/// The seeded documents the run starts from, created by the system principal.
#[derive(Clone, Copy, Debug)]
pub struct Seeded {
    /// The person every Time Entry names.
    pub user: RecordId,
    /// The Project every Task belongs to.
    pub project: RecordId,
}

fn op_of(op: OpKind) -> Op {
    match op {
        OpKind::CreateTask => Op::CreateTask,
        OpKind::CreateTe => Op::CreateTimeEntry,
        OpKind::UpdateTe => Op::UpdateTimeEntry,
        OpKind::PostTe => Op::PostTimeEntry,
        OpKind::DeniedPost => Op::UpdateTask,
        OpKind::CancelTe | OpKind::CorrectTe => Op::CancelTimeEntry,
        OpKind::DeleteTe => Op::DeleteTimeEntry,
        OpKind::TouchTask => Op::CallTaskMethod,
    }
}

struct Builder {
    run: EngineRun,
}

impl Builder {
    fn fold(
        &mut self,
        actor: &str,
        op: Op,
        entity: &str,
        id: RecordId,
        version: u64,
        state: Option<DocState>,
    ) {
        self.run.events.push(ExpectedEvent {
            op,
            topic: op.topic(),
            entity: entity.to_owned(),
            id,
            version,
            actor: actor.to_owned(),
        });
        let key = (entity.to_owned(), id);
        match op {
            Op::DeleteTask | Op::DeleteTimeEntry => {
                if let Some(doc) = self.run.docs.get_mut(&key) {
                    doc.live = false;
                    doc.transitions.push(op);
                }
            }
            _ => {
                let doc = self.run.docs.entry(key).or_insert_with(|| ExpectedDoc {
                    entity: entity.to_owned(),
                    id,
                    version,
                    versions: BTreeSet::new(),
                    state,
                    live: true,
                    name: String::new(),
                    transitions: Vec::new(),
                });
                doc.version = version;
                doc.versions.insert(version);
                doc.state = state;
                doc.live = true;
                doc.transitions.push(op);
            }
        }
    }

    fn record(&mut self, a: &Attempt, outcome: OpOutcome) {
        self.run.ops.push(OpRecord {
            worker: a.worker,
            seq: a.seq,
            op: op_of(a.op),
            actor: a.actor.to_owned(),
            entity: a.doctype.to_owned(),
            id: a.target,
            outcome,
        });
    }

    fn committed(&mut self, a: &Attempt, done: &Done) {
        let actor = a.actor;
        match a.op {
            OpKind::CorrectTe => {
                let original = a.expected.unwrap_or(0) + 1;
                self.fold(
                    actor,
                    Op::CancelTimeEntry,
                    TIME_ENTRY,
                    a.target,
                    original,
                    Some(DocState::Cancelled),
                );
                self.fold(
                    actor,
                    Op::CreateTimeEntry,
                    TIME_ENTRY,
                    done.doc,
                    done.seen.version,
                    Some(DocState::Draft),
                );
            }
            OpKind::DeleteTe => self.fold(
                actor,
                Op::DeleteTimeEntry,
                TIME_ENTRY,
                a.target,
                a.expected.unwrap_or(0),
                None,
            ),
            other => {
                let state = if a.doctype == TIME_ENTRY {
                    done.seen.state
                } else {
                    None
                };
                self.fold(
                    actor,
                    op_of(other),
                    a.doctype,
                    done.doc,
                    done.seen.version,
                    state,
                );
            }
        }
        self.record(a, OpOutcome::Committed);
    }
}

/// The oracle of a run. `denial_count` is the engine's own count, read after the workers stopped.
pub fn build(
    seed: u64,
    workers: u32,
    profile: &str,
    backend: &str,
    seeded: Seeded,
    attempts: &[Attempt],
    denial_count: usize,
) -> EngineRun {
    let mut b = Builder {
        run: EngineRun {
            seed,
            workers,
            profile: profile.to_owned(),
            backend: backend.to_owned(),
            docs: Default::default(),
            events: Vec::new(),
            denials: Vec::new(),
            ops: Vec::new(),
            secrets: Vec::new(),
            links: Vec::new(),
            denial_count,
        },
    };
    b.fold("system", Op::CreateTask, USER, seeded.user, 1, None);
    b.fold(
        "system",
        Op::CreateProject,
        PROJECT,
        seeded.project,
        1,
        None,
    );
    for a in attempts {
        if let Some(done) = a.committed() {
            b.committed(a, done);
            continue;
        }
        let outcome = match (&a.outcome, &a.resolution) {
            (Outcome::Denied(code), _) => {
                b.run.denials.push(DeniedOp {
                    op: op_of(a.op),
                    entity: a.doctype.to_owned(),
                    id: a.target,
                    actor: a.actor.to_owned(),
                    code: code.clone(),
                });
                OpOutcome::Denied(code.clone())
            }
            (Outcome::Refused(code), _) => OpOutcome::Refused(code.clone()),
            (
                Outcome::Indeterminate(_),
                Some(Resolution {
                    settled: Settled::Refused(code),
                    ..
                }),
            ) => OpOutcome::Refused(code.clone()),
            (Outcome::Indeterminate(code), _) => OpOutcome::Failed(code.clone()),
            (Outcome::Committed(_), _) => OpOutcome::Failed("unreachable".to_owned()),
        };
        b.record(a, outcome);
    }
    b.run
}
