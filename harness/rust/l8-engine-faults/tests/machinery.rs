//! The machinery on any machine: the audit rules on hand-built logs, the resolution logic on the
//! in-memory fake with a decorator that loses replies, and the honesty proofs (a green run must
//! be able to turn red).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::DocState;
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::RecordId;
use spot_l7_engine::world::{TIME_ENTRY, World};
use spot_l8_engine_faults::audit::audit_log;
use spot_l8_engine_faults::core::{Core, build_engine, run_core, seed};
use spot_l8_engine_faults::decorate::{Liar, LostReplies};
use spot_l8_engine_faults::log::{
    Attempt, Done, OpKind, Outcome, Resolution, Seen, Settled, command_id,
};
use spot_l8_engine_faults::workload::{Honesty, Mode, WorkloadConfig};
use spot_storage_fake::FakeBackend;

// ---- the audit rules on hand-built logs ------------------------------------------------------

fn te(
    seq: u64,
    op: OpKind,
    target: RecordId,
    expected: Option<u64>,
    outcome: Outcome,
    resolution: Option<Resolution>,
) -> Attempt {
    Attempt {
        command_id: command_id(1, 0, seq),
        worker: 0,
        seq,
        op,
        actor: "manager",
        doctype: TIME_ENTRY,
        target,
        expected,
        outcome,
        resolution,
    }
}

fn done(doc: RecordId, version: u64, state: DocState) -> Done {
    Done {
        doc,
        seen: Seen {
            version,
            state: Some(state),
        },
    }
}

fn clean_log() -> Vec<Attempt> {
    let id = RecordId::new_v7();
    vec![
        te(
            0,
            OpKind::CreateTe,
            id,
            None,
            Outcome::Committed(done(id, 1, DocState::Draft)),
            None,
        ),
        // A lost reply on the post: the read-back shows it committed, the re-send replays it.
        te(
            1,
            OpKind::PostTe,
            id,
            Some(1),
            Outcome::Indeterminate("storage.commit_outcome_unknown".into()),
            Some(Resolution {
                before: Some(Seen {
                    version: 2,
                    state: Some(DocState::Posted),
                }),
                settled: Settled::Committed(done(id, 2, DocState::Posted)),
                resends: 1,
            }),
        ),
        te(
            2,
            OpKind::CancelTe,
            id,
            Some(2),
            Outcome::Committed(done(id, 3, DocState::Cancelled)),
            None,
        ),
    ]
}

#[test]
fn a_clean_log_has_no_findings() {
    assert_eq!(audit_log(&clean_log()), Vec::new());
}

fn rules(log: &[Attempt]) -> Vec<char> {
    let mut r: Vec<char> = audit_log(log).iter().map(|f| f.rule).collect();
    r.dedup();
    r
}

#[test]
fn rule_e_catches_a_replay_that_answers_another_version() {
    let mut log = clean_log();
    if let Some(Resolution {
        settled: Settled::Committed(d),
        ..
    }) = log[1].resolution.as_mut()
    {
        d.seen.version = 5;
    }
    assert!(rules(&log).contains(&'e'));
}

#[test]
fn rule_e_catches_a_resend_refused_after_the_command_committed() {
    let mut log = clean_log();
    if let Some(r) = log[1].resolution.as_mut() {
        r.settled = Settled::Refused("document.stale_version".into());
    }
    assert!(rules(&log).contains(&'e'));
}

#[test]
fn rule_e_catches_a_first_run_that_gives_the_wrong_version() {
    let mut log = clean_log();
    let target = log[1].target;
    if let Some(r) = log[1].resolution.as_mut() {
        r.before = Some(Seen {
            version: 1,
            state: Some(DocState::Draft),
        });
        r.settled = Settled::Committed(done(target, 9, DocState::Posted));
    }
    assert!(rules(&log).contains(&'e'));
}

#[test]
fn an_unsettled_attempt_is_a_finding() {
    let mut log = clean_log();
    log[1].resolution = None;
    assert!(rules(&log).contains(&'u'));
}

#[test]
fn rule_f_catches_a_gap_in_the_version_history() {
    let mut log = clean_log();
    log[2].outcome = Outcome::Committed(done(log[2].target, 7, DocState::Cancelled));
    assert!(rules(&log).contains(&'f'));
}

// ---- resolution through the ledger, on the fake ----------------------------------------------

async fn offline(
    ledger: bool,
    lose_every: u64,
    liar: Option<u64>,
    honesty: Honesty,
    mode: Mode,
) -> Core {
    let world = Arc::new(World::new());
    let engine = build_engine(&world, FakeBackend::new(), ledger, false).unwrap();
    let seeded = seed(&world, &engine).await.unwrap();
    let lost = Arc::new(LostReplies::new(engine.clone(), lose_every));
    let service: Arc<dyn DocumentService> = match liar {
        Some(every) => Arc::new(Liar::new(lost, every)),
        None => lost,
    };
    let cfg = WorkloadConfig {
        seed: 20_261_113,
        workers: 3,
        ops_per_worker: 40,
        mode,
        honesty,
        settle_deadline: Duration::from_secs(10),
        backoff: Duration::from_millis(2),
        person: seeded.user,
    };
    run_core(world, engine, service, cfg, seeded, "fake", async {
        Vec::new()
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_replies_are_settled_by_resending_and_every_check_holds() {
    let core = offline(true, 5, None, Honesty::Honest, Mode::Mixed).await;
    let settled = core
        .attempts
        .iter()
        .filter(|a| a.resolution.is_some())
        .count();
    assert!(settled > 0, "the decorator must lose some replies");
    assert!(core.ok(), "{}\n{}", core.table, core.l7_text);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cancel_and_amend_cycle_holds_under_lost_replies() {
    let core = offline(true, 4, None, Honesty::Honest, Mode::CorrectCycle).await;
    assert!(core.ok(), "{}\n{}", core.table, core.l7_text);
}

// ---- honesty: a green run can turn red -------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_ledger_a_resend_is_a_second_effect_and_rule_e_fails() {
    let core = offline(false, 5, None, Honesty::Honest, Mode::Mixed).await;
    assert!(
        core.findings.iter().any(|f| f.rule == 'e'),
        "{}",
        core.table
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_answer_for_a_call_never_sent_fails_rule_a() {
    let core = offline(true, 1_000_000, Some(5), Honesty::Honest, Mode::Mixed).await;
    assert!(
        core.findings.iter().any(|f| f.rule == 'a'),
        "{}",
        core.table
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_resending_leaves_unresolved_attempts_that_fail_the_run() {
    let core = offline(true, 5, None, Honesty::NeverResend, Mode::Mixed).await;
    assert!(
        core.findings.iter().any(|f| f.rule == 'u'),
        "{}",
        core.table
    );
    assert!(!core.ok());
}

// ---- the call ledger names the method ---------------------------------------------------------

#[test]
fn the_call_ledger_records_every_call_with_method_document_and_outcome() {
    use spot_l8_engine_faults::audit::{CallOutcome, call_ledger};
    let mut log = clean_log();
    let target = log[0].target;
    log.push(te(
        3,
        OpKind::TouchTask,
        target,
        Some(3),
        Outcome::Indeterminate("storage.commit_outcome_unknown".into()),
        None,
    ));
    log.push(te(
        4,
        OpKind::DeniedPost,
        target,
        Some(3),
        Outcome::Denied("permission.denied".into()),
        None,
    ));
    let ledger = call_ledger(&log);
    assert_eq!(ledger.len(), log.len());
    let methods: Vec<&str> = ledger.iter().map(|c| c.method).collect();
    assert_eq!(
        methods,
        ["create", "post", "cancel", "call_method:touch", "post"]
    );
    let outcomes: Vec<CallOutcome> = ledger.iter().map(|c| c.outcome).collect();
    assert_eq!(
        outcomes,
        [
            CallOutcome::Acked,
            CallOutcome::Acked,
            CallOutcome::Acked,
            CallOutcome::Unknown,
            CallOutcome::Failed
        ]
    );
    assert!(ledger.iter().all(|c| c.document == target.to_string()));
}

#[test]
fn a_seeded_lost_commit_names_its_method_in_the_failure_text() {
    let mut log = clean_log();
    // The post committed (the read-back showed it) but the re-send was refused: a lost commit.
    if let Some(r) = log[1].resolution.as_mut() {
        r.settled = Settled::Refused("document.stale_version".into());
    }
    let findings = audit_log(&log);
    let lost = findings
        .iter()
        .find(|f| f.rule == 'e')
        .expect("rule e fires");
    assert!(lost.detail.contains("(method post"), "{}", lost.detail);
    assert!(lost.detail.contains("call failed"), "{}", lost.detail);
    // Names only: no field value appears.
    assert!(!lost.detail.contains("Secret"));
    // And the printed table carries it too.
    let table = spot_l8_engine_faults::audit::table(&log, &findings);
    assert!(table.contains("(method post"), "{table}");
}

#[test]
fn an_unsettled_method_call_names_call_method_touch() {
    let mut log = clean_log();
    let target = log[0].target;
    log.push(te(
        3,
        OpKind::TouchTask,
        target,
        Some(3),
        Outcome::Indeterminate("storage.commit_outcome_unknown".into()),
        None,
    ));
    let findings = audit_log(&log);
    let unsettled = findings
        .iter()
        .find(|f| f.rule == 'u')
        .expect("rule u fires");
    assert!(
        unsettled.detail.contains("(method call_method:touch"),
        "{}",
        unsettled.detail
    );
}
