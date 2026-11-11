//! Undo, rewind and the record file against the fake app.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::prelude::Decimal;
use spot_app_api::{EffectKind, Operation};
use spot_bpm_effects::{
    AlternativeId, Choice, ChoiceOutcome, EffectsError, NoGraph, PartnerWait, PlannedReversal,
    RequestEffects, ReviseHow, RewindPath, UndoContext, UndoOutcome, codes,
};

const WHO: &str = "user-1";

struct Rig {
    svc: Arc<RequestEffects>,
    app: Arc<FakeApp>,
    partner: Partner,
}

impl Rig {
    fn new() -> Self {
        let app = FakeApp::new();
        let partner = Partner::new(&app);
        Self {
            svc: Arc::new(service()),
            app,
            partner,
        }
    }

    fn ctx(&self) -> UndoContext<'_> {
        UndoContext {
            principal: WHO,
            now: NOW,
            port: self.app.as_ref(),
            partner: &self.partner,
        }
    }

    /// An order-like chain: 1 (order) <- 2 (invoice) <- 3 (delivery), and 4 (payment) paying 2.
    fn chain(&self, req: &spot_bpm_engine::CaseId) {
        let (a, b, c, p) = (rec(1), rec(2), rec(3), rec(4));
        self.app.post(&a, 100, None);
        self.app.post(&b, 100, Some(&a));
        self.app.post(&c, 100, Some(&b));
        self.app.post(&p, 40, Some(&b));
        self.svc.record(req, new_entry("n1", &a, &[])).unwrap();
        self.svc.record(req, new_entry("n2", &b, &[&a])).unwrap();
        self.svc.record(req, new_entry("n3", &c, &[&b])).unwrap();
        self.svc.record(req, new_entry("n4", &p, &[&b])).unwrap();
    }
}

fn names(plan: &spot_bpm_effects::UndoPlan) -> Vec<String> {
    plan.steps().iter().map(|s| s.record.name.clone()).collect()
}

#[test]
fn undo_to_start_reverses_in_dependency_order() {
    let rig = Rig::new();
    let req = case("req1");
    rig.chain(&req);
    let before = probe(&rig.app, &[1, 2, 3, 4]);
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    // The payment and the delivery depend on the invoice, the invoice on the order; ties go by
    // reverse time.
    assert_eq!(
        names(&plan),
        vec![rec(4).name, rec(3).name, rec(2).name, rec(1).name]
    );

    let out = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert_eq!(
        out,
        UndoOutcome::Done {
            reversed: 4,
            skipped: 0
        }
    );
    let after = probe(&rig.app, &[1, 2, 3, 4]);
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(a.0, Decimal::from(0), "every store nets to zero");
        assert!(a.1 >= b.1, "append-only counts only grow");
        assert_eq!(a.1, b.1 + 1, "exactly one reversing row each");
    }
    assert_eq!(
        rig.app.state.lock().unwrap().reversed,
        vec![rec(4), rec(3), rec(2), rec(1)]
    );

    let file = rig.svc.file(&req).unwrap();
    assert_eq!(
        file.entries().len(),
        8,
        "four forward entries and four new receipts"
    );
    assert!(!file.is_locked());
    assert!(file.position().is_none());
    let rev = &file.entries()[4];
    assert_eq!(rev.reverses.unwrap().entry, 3);
    assert_eq!(rev.operation, Operation::Cancel);
    // The request is free again.
    rig.svc.record(&req, new_entry("n1", &rec(9), &[])).unwrap();
}

#[test]
fn undo_to_a_node_keeps_what_came_before() {
    let rig = Rig::new();
    let req = case("req2");
    rig.chain(&req);
    let plan = rig
        .svc
        .plan_undo(&req, Some(&node("n2")), &NoGraph)
        .unwrap();
    assert_eq!(names(&plan), vec![rec(4).name, rec(3).name]);
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert_eq!(rig.app.net(&rec(1)), 100);
    assert_eq!(rig.app.net(&rec(2)), 100);
    assert_eq!(rig.app.net(&rec(3)), 0);
    assert_eq!(rig.app.net(&rec(4)), 0);
    assert_eq!(rig.svc.file(&req).unwrap().position(), Some(&node("n2")));
    assert!(matches!(
        rig.svc.plan_undo(&req, Some(&node("zz")), &NoGraph),
        Err(EffectsError::NodeUnknown { .. })
    ));
}

#[test]
fn a_dependant_outside_the_request_stops_the_sequence_with_blockers() {
    let rig = Rig::new();
    let req = case("req3");
    rig.chain(&req);
    // A payment made outside the request also pays the invoice.
    rig.app.post(&rec(50), 10, Some(&rec(2)));
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    let out = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    let UndoOutcome::NeedsDecision(decision) = out else {
        panic!("expected a decision")
    };
    assert_eq!(decision.blockers.len(), 1);
    assert_eq!(decision.blockers[0].record, rec(2));
    assert_eq!(decision.blockers[0].reason_code, "document_has_dependants");
    assert_eq!(decision.blockers[0].dependants, vec![rec(50)]);
    assert_eq!(decision.alternatives.len(), 1);
    // State is consistent with the file: the payment and the delivery are reversed, the invoice
    // and the order are not.
    assert_eq!(rig.app.net(&rec(4)), 0);
    assert_eq!(rig.app.net(&rec(3)), 0);
    assert_eq!(rig.app.net(&rec(2)), 100);
    assert_eq!(rig.app.net(&rec(1)), 100);
    let file = rig.svc.file(&req).unwrap();
    assert_eq!(
        file.entries()
            .iter()
            .filter(|e| e.reverses.is_some())
            .count(),
        2
    );
    assert_eq!(file.pending_decision(), Some(&decision));
    // Nothing was forced, and the request stays held.
    assert!(matches!(
        rig.svc.record(&req, new_entry("n9", &rec(9), &[])),
        Err(EffectsError::RequestLocked { .. })
    ));
}

#[test]
fn a_chosen_alternative_is_audited_and_the_undo_resumes() {
    let rig = Rig::new();
    let req = case("req4");
    rig.chain(&req);
    rig.app.post(&rec(50), 10, Some(&rec(2)));
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();

    // An alternative the type never offered is refused.
    let bogus = Choice::Alternative(AlternativeId::new("make_it_so").unwrap());
    assert!(matches!(
        rig.svc.choose(&req, &bogus, &rig.ctx()),
        Err(EffectsError::NoSuchDecision { .. })
    ));

    let pick = Choice::Alternative(AlternativeId::new("reverse_dated_today").unwrap());
    assert_eq!(
        rig.svc.choose(&req, &pick, &rig.ctx()).unwrap(),
        ChoiceOutcome::Applied
    );
    let file = rig.svc.file(&req).unwrap();
    assert_eq!(file.audit().len(), 1);
    assert_eq!(file.audit()[0].by, WHO);
    assert_eq!(
        file.audit()[0].alternative.as_deref(),
        Some("reverse_dated_today")
    );
    assert!(file.pending_decision().is_none());

    let out = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert!(matches!(out, UndoOutcome::Done { .. }));
    for n in [1, 2, 3, 4] {
        assert_eq!(rig.app.net(&rec(n)), 0, "document {n}");
    }
    assert_eq!(
        rig.app.net(&rec(50)),
        10,
        "the outside payment is untouched"
    );
}

#[test]
fn abandoning_releases_the_request_and_is_audited() {
    let rig = Rig::new();
    let req = case("req5");
    rig.chain(&req);
    rig.app.post(&rec(50), 10, Some(&rec(2)));
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert_eq!(
        rig.svc.choose(&req, &Choice::Abandon, &rig.ctx()).unwrap(),
        ChoiceOutcome::Abandoned
    );
    let file = rig.svc.file(&req).unwrap();
    assert!(!file.is_locked());
    assert_eq!(file.audit().len(), 1);
    assert!(file.audit()[0].alternative.is_none());
    assert_eq!(rig.app.net(&rec(1)), 100, "what was not reversed stays");
    rig.svc.record(&req, new_entry("n5", &rec(9), &[])).unwrap();
}

#[test]
fn a_crash_mid_undo_resumes_without_reversing_twice() {
    let rig = Rig::new();
    let req = case("req6");
    rig.chain(&req);
    rig.app.state.lock().unwrap().crash_after = Some(2);
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    let err = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap_err();
    assert_eq!(err.code(), codes::PORT_UNAVAILABLE);
    assert!(rig.svc.file(&req).unwrap().is_locked());
    assert_eq!(rig.app.net(&rec(4)), 0);
    assert_eq!(rig.app.net(&rec(3)), 0);
    assert_eq!(rig.app.net(&rec(2)), 100);

    // A new plan is refused while the crashed one holds the request; the interrupted one resumes.
    assert!(matches!(
        rig.svc.plan_undo(&req, None, &NoGraph),
        Err(EffectsError::RequestLocked { .. })
    ));
    let resumed = rig.svc.resume_plan(&req, &NoGraph).unwrap();
    assert_eq!(names(&resumed), vec![rec(2).name, rec(1).name]);
    let out = rig.svc.execute_undo(&resumed, &rig.ctx()).unwrap();
    assert!(matches!(out, UndoOutcome::Done { .. }));
    for n in [1, 2, 3, 4] {
        assert_eq!(rig.app.net(&rec(n)), 0);
    }
    assert_eq!(
        rig.app.row_count(),
        8,
        "each document reversed exactly once"
    );
    // Re-running the finished plan's old object changes nothing (the journal is gone, the file
    // has moved on, so it is refused as stale).
    assert!(matches!(
        rig.svc.execute_undo(&plan, &rig.ctx()),
        Err(EffectsError::StalePlan { .. })
    ));
    assert_eq!(rig.app.row_count(), 8);
}

#[test]
fn a_forward_step_a_rewind_and_a_second_undo_are_refused_during_an_undo() {
    let rig = Rig::new();
    let req = case("req7");
    rig.chain(&req);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let (svc, seen, req) = (Arc::clone(&rig.svc), Arc::clone(&seen), req.clone());
        *rig.app.on_call.lock().unwrap() = Some(Box::new(move || {
            let forward = svc.record(&req, new_entry("n9", &rec(99), &[]));
            let rewind = svc.rewind(&req, &node("n1"), &RewindPath::default());
            let second = svc.plan_undo(&req, Some(&node("n1")), &NoGraph);
            seen.lock().unwrap().push((
                forward.unwrap_err().code(),
                rewind.unwrap_err().code(),
                second.unwrap_err().code(),
            ));
        }));
    }
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    for s in seen.iter() {
        assert_eq!(
            *s,
            (
                codes::REQUEST_LOCKED,
                codes::REQUEST_LOCKED,
                codes::REQUEST_LOCKED
            )
        );
    }
}

#[test]
fn the_same_plan_from_two_callers_reaches_the_port_once_per_step() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let rig = Rig::new();
    let req = case("req7b");
    rig.chain(&req);
    let calls = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let started_tx = std::sync::Mutex::new(started_tx);
    {
        let calls = Arc::clone(&calls);
        *rig.app.on_call.lock().unwrap() = Some(Box::new(move || {
            // The first reversal call waits until the second caller has been refused.
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                started_tx.lock().unwrap().send(()).unwrap();
                go_rx.lock().unwrap().recv().unwrap();
            }
        }));
    }
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| rig.svc.execute_undo(&plan, &rig.ctx()));
        started_rx.recv().unwrap();
        // The first caller is inside the port: the second, with the same plan, is refused and
        // so are a choice and an abandon.
        let second = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap_err();
        assert_eq!(second.code(), codes::REQUEST_LOCKED);
        let choice = rig
            .svc
            .choose(&req, &Choice::Abandon, &rig.ctx())
            .unwrap_err();
        assert_eq!(choice.code(), codes::REQUEST_LOCKED);
        let abandon = rig.svc.abandon(&req, &rig.ctx()).unwrap_err();
        assert_eq!(abandon.code(), codes::REQUEST_LOCKED);
        go_tx.send(()).unwrap();
        let out = first.join().unwrap().unwrap();
        assert_eq!(
            out,
            UndoOutcome::Done {
                reversed: 4,
                skipped: 0
            }
        );
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "each step reached the port once"
    );
    assert_eq!(rig.app.row_count(), 8);
    let file = rig.svc.file(&req).unwrap();
    assert_eq!(
        file.entries()
            .iter()
            .filter(|e| e.reverses.is_some())
            .count(),
        4,
        "one reversing entry per step"
    );
    assert!(!file.is_locked());
}

#[test]
fn the_running_flag_is_cleared_when_a_port_fails_and_the_same_plan_resumes() {
    let rig = Rig::new();
    let req = case("req7c");
    rig.chain(&req);
    rig.app.state.lock().unwrap().crash_after = Some(1);
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    assert!(rig.svc.execute_undo(&plan, &rig.ctx()).is_err());
    let file = rig.svc.file(&req).unwrap();
    assert!(file.is_locked());
    assert!(!file.undo().unwrap().running);
    assert_eq!(file.undo().unwrap().started_by, WHO);
    assert!(matches!(
        rig.svc.execute_undo(&plan, &rig.ctx()).unwrap(),
        UndoOutcome::Done { .. }
    ));
}

#[test]
fn abandon_releases_a_lock_that_has_no_pending_decision_and_is_audited() {
    let rig = Rig::new();
    let req = case("req7d");
    rig.chain(&req);
    // No undo yet: nothing to abandon.
    assert_eq!(
        rig.svc.abandon(&req, &rig.ctx()).unwrap_err().code(),
        codes::NO_UNDO
    );
    // The port keeps failing: the lock is held with no decision to choose from.
    rig.app.state.lock().unwrap().crash_after = Some(0);
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    assert!(rig.svc.execute_undo(&plan, &rig.ctx()).is_err());
    assert!(rig.svc.file(&req).unwrap().is_locked());
    assert_eq!(
        rig.svc
            .choose(&req, &Choice::Abandon, &rig.ctx())
            .unwrap_err()
            .code(),
        codes::NO_SUCH_DECISION,
        "choose needs a decision"
    );
    rig.svc.abandon(&req, &rig.ctx()).unwrap();
    let file = rig.svc.file(&req).unwrap();
    assert!(!file.is_locked());
    assert_eq!(file.audit().len(), 1);
    assert_eq!(file.audit()[0].by, WHO);
    assert!(file.audit()[0].alternative.is_none());
    rig.svc.record(&req, new_entry("n5", &rec(9), &[])).unwrap();
}

#[test]
fn a_restored_file_never_claims_a_running_caller() {
    let rig = Rig::new();
    let req = case("req7e");
    rig.chain(&req);
    // A snapshot taken while the undo is running (what a host would have stored at a crash).
    let snapshot = Arc::new(std::sync::Mutex::new(None));
    {
        let (svc, snapshot, req) = (Arc::clone(&rig.svc), Arc::clone(&snapshot), req.clone());
        *rig.app.on_call.lock().unwrap() = Some(Box::new(move || {
            let mut slot = snapshot.lock().unwrap();
            if slot.is_none() {
                *slot = Some(svc.file(&req).unwrap());
            }
        }));
    }
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    let file = snapshot.lock().unwrap().take().unwrap();
    assert!(file.undo().unwrap().running);
    rig.svc.restore(&req, file);
    assert!(!rig.svc.file(&req).unwrap().undo().unwrap().running);
    rig.svc.abandon(&req, &rig.ctx()).unwrap();
}

#[test]
fn a_stale_plan_is_refused() {
    let rig = Rig::new();
    let req = case("req8");
    rig.chain(&req);
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    rig.svc.record(&req, new_entry("n5", &rec(9), &[])).unwrap();
    assert!(matches!(
        rig.svc.execute_undo(&plan, &rig.ctx()),
        Err(EffectsError::StalePlan { .. })
    ));
    assert_eq!(rig.app.row_count(), 4, "nothing ran");
}

#[test]
fn a_point_of_no_return_runs_nothing() {
    let rig = Rig::new();
    let req = case("req9");
    rig.chain(&req);
    let wire = rec(7);
    rig.app.post(&wire, 5, None);
    let mut entry = new_entry("n5", &wire, &[]);
    entry.receipt.effects.push(irreversible_effect());
    rig.svc.record(&req, entry).unwrap();
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    assert!(plan.is_blocked());
    let out = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    let UndoOutcome::NeedsDecision(d) = out else {
        panic!("expected a decision")
    };
    assert_eq!(d.blockers[0].record, wire);
    assert_eq!(d.blockers[0].reason_code, codes::POINT_OF_NO_RETURN);
    assert!(d.alternatives.is_empty());
    assert_eq!(
        rig.app.row_count(),
        5,
        "no reversal ran before the blocker was known"
    );
    assert!(!rig.svc.file(&req).unwrap().is_locked());
    // Undoing only to a later node passes no point of no return.
    let later = rig
        .svc
        .plan_undo(&req, Some(&node("n5")), &NoGraph)
        .unwrap();
    assert!(later.steps().is_empty());
}

fn irreversible_effect() -> spot_app_api::EffectReceipt {
    use spot_app_api::{EffectClassName, Hex32, ReceiptState, StepName};
    spot_app_api::EffectReceipt {
        step: StepName::new("fake:wire").unwrap(),
        class: EffectClassName::new("bank_wire").unwrap(),
        kind: EffectKind::External,
        store_refs: Vec::new(),
        count: 1,
        digest: Hex32::new(&"1".repeat(64)).unwrap(),
        state: ReceiptState::Delivered,
    }
}

#[test]
fn an_undeclared_effect_fails_closed() {
    let rig = Rig::new();
    let req = case("req10");
    let r = rec(8);
    rig.app.post(&r, 5, None);
    let mut entry = new_entry("n1", &r, &[]);
    entry.receipt.effects[0].class = spot_app_api::EffectClassName::new("mystery").unwrap();
    rig.svc.record(&req, entry).unwrap();
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    assert!(
        plan.steps()
            .iter()
            .any(|s| s.action == PlannedReversal::Blocked)
    );
}

#[test]
fn a_compensable_effect_runs_its_named_action() {
    let rig = Rig::new();
    let req = case("req11");
    let r = rec(8);
    rig.app.post(&r, 5, None);
    let mut entry = new_entry("n1", &r, &[]);
    entry.receipt.effects.push({
        let mut e = irreversible_effect();
        e.step = spot_app_api::StepName::new("fake:notify").unwrap();
        e.class = spot_app_api::EffectClassName::new("mail").unwrap();
        e
    });
    rig.svc.record(&req, entry).unwrap();
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    assert_eq!(plan.steps().len(), 2);
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    let log = rig.app.log();
    assert!(log.contains(&format!("reverse {}", r.name)));
    assert!(
        log.iter()
            .any(|l| l.starts_with("compensate fake:send_correction"))
    );
}

#[test]
fn the_post_and_cancel_of_one_request_net_out_without_a_call() {
    let rig = Rig::new();
    let req = case("req12");
    let r = rec(8);
    rig.app.post(&r, 5, None);
    rig.svc.record(&req, new_entry("n1", &r, &[])).unwrap();
    rig.svc
        .record(
            &req,
            spot_bpm_effects::NewEntry {
                node: node("n2"),
                receipt: receipt(Operation::Cancel, &r, 3, Vec::new()),
                links: Default::default(),
                at: NOW,
            },
        )
        .unwrap();
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    assert!(
        plan.steps()
            .iter()
            .all(|s| matches!(s.action, PlannedReversal::Skip(_)))
    );
    let out = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert_eq!(
        out,
        UndoOutcome::Done {
            reversed: 0,
            skipped: 2
        }
    );
}

#[test]
fn the_partner_cancellation_is_the_last_step() {
    let rig = Rig::new();
    let req = case("req13");
    rig.chain(&req);
    rig.svc
        .set_waiting(
            &req,
            Some(PartnerWait {
                conversation: "conv1".to_owned(),
                cancellation: "cancel_order".to_owned(),
                before_local: false,
            }),
        )
        .unwrap();
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    let log = rig.app.log();
    assert_eq!(log.last().unwrap(), "cancel cancel_order");
    assert_eq!(log.len(), 5);
    assert!(rig.svc.file(&req).unwrap().waiting().is_none());
}

#[test]
fn the_cancellation_is_not_sent_when_a_reversal_is_refused() {
    let rig = Rig::new();
    let req = case("req14");
    rig.chain(&req);
    rig.app.post(&rec(50), 10, Some(&rec(2)));
    rig.svc
        .set_waiting(
            &req,
            Some(PartnerWait {
                conversation: "conv1".to_owned(),
                cancellation: "cancel_order".to_owned(),
                before_local: false,
            }),
        )
        .unwrap();
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    let out = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert!(matches!(out, UndoOutcome::NeedsDecision(_)));
    assert!(rig.app.log().iter().all(|l| !l.starts_with("cancel ")));
}

#[test]
fn a_contract_may_cancel_before_the_local_reversals() {
    let rig = Rig::new();
    let req = case("req15");
    rig.chain(&req);
    rig.svc
        .set_waiting(
            &req,
            Some(PartnerWait {
                conversation: "conv1".to_owned(),
                cancellation: "cancel_order".to_owned(),
                before_local: true,
            }),
        )
        .unwrap();
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert_eq!(rig.app.log().first().unwrap(), "cancel cancel_order");
}

#[test]
fn a_failed_cancellation_leaves_the_undo_resumable() {
    let rig = Rig::new();
    let req = case("req16");
    rig.chain(&req);
    rig.svc
        .set_waiting(
            &req,
            Some(PartnerWait {
                conversation: "conv1".to_owned(),
                cancellation: "cancel_order".to_owned(),
                before_local: false,
            }),
        )
        .unwrap();
    *rig.partner.fail.lock().unwrap() = true;
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    let err = rig.svc.execute_undo(&plan, &rig.ctx()).unwrap_err();
    assert_eq!(err.code(), codes::CANCELLATION_FAILED);
    *rig.partner.fail.lock().unwrap() = false;
    rig.svc.execute_undo(&plan, &rig.ctx()).unwrap();
    assert_eq!(rig.app.log().last().unwrap(), "cancel cancel_order");
    assert_eq!(rig.app.row_count(), 8, "no document reversed twice");
}

#[test]
fn rewind_keeps_every_effect_and_revises_instead_of_creating() {
    let rig = Rig::new();
    let req = case("req17");
    let (a, b) = (rec(1), rec(2));
    rig.app.post(&a, 1, None);
    rig.app.post(&b, 1, None);
    rig.svc.record(&req, new_entry("n1", &a, &[])).unwrap();
    let mut created = new_entry("n2", &b, &[]);
    created.receipt = receipt(Operation::Create, &b, 1, Vec::new());
    rig.svc.record(&req, created).unwrap();

    let mut path = RewindPath::default();
    path.will_run.insert(node("n2"));
    // The revised path would create b again and the process declares no mapping: refused.
    let err = rig.svc.rewind(&req, &node("n1"), &path).unwrap_err();
    assert_eq!(err.code(), codes::REWIND_WOULD_DUPLICATE);
    assert_eq!(rig.svc.file(&req).unwrap().position(), Some(&node("n2")));

    path.revisions.insert(node("n2"), ReviseHow::Update);
    let out = rig.svc.rewind(&req, &node("n1"), &path).unwrap();
    assert_eq!(out.kept_entries, 2);
    assert_eq!(rig.svc.file(&req).unwrap().entries().len(), 2);
    assert_eq!(rig.svc.file(&req).unwrap().position(), Some(&node("n1")));
    let rev = rig.svc.revise_of(&req, &node("n2")).unwrap();
    assert_eq!(
        (rev.record.clone(), rev.how),
        (b.clone(), ReviseHow::Update)
    );

    // The next pass of n2 records that it revises b.
    let mut again = new_entry("n2", &b, &[]);
    again.receipt = receipt(Operation::Update, &b, 2, Vec::new());
    rig.svc.record(&req, again).unwrap();
    let file = rig.svc.file(&req).unwrap();
    assert_eq!(file.entries()[2].revise_of, Some(b));
    assert!(rig.svc.revise_of(&req, &node("n2")).is_none());
}

#[test]
fn rewinding_past_a_node_that_does_not_run_again_needs_no_mapping() {
    let rig = Rig::new();
    let req = case("req18");
    let b = rec(2);
    rig.svc.record(&req, new_entry("n1", &rec(1), &[])).unwrap();
    let mut created = new_entry("n2", &b, &[]);
    created.receipt = receipt(Operation::Create, &b, 1, Vec::new());
    rig.svc.record(&req, created).unwrap();
    rig.svc
        .rewind(&req, &node("n1"), &RewindPath::default())
        .unwrap();
    assert!(matches!(
        rig.svc
            .rewind(&case("nope"), &node("n1"), &RewindPath::default()),
        Err(EffectsError::RequestUnknown { .. })
    ));
}

#[test]
fn queries_by_record_dependants_and_effect() {
    let rig = Rig::new();
    let req = case("req19");
    rig.chain(&req);
    let file = rig.svc.file(&req).unwrap();
    assert_eq!(file.entries_by_record(&rec(2)).len(), 1);
    let deps = file.dependants(&rec(2), &NoGraph).unwrap();
    assert_eq!(deps, vec![rec(3), rec(4)]);
    struct G;
    impl spot_bpm_effects::GraphDependants for G {
        fn dependants(
            &self,
            r: &spot_bpm_effects::RecordRef,
        ) -> Result<Vec<spot_bpm_effects::RecordRef>, String> {
            Ok(if *r == rec(1) {
                vec![rec(77)]
            } else {
                Vec::new()
            })
        }
    }
    assert_eq!(file.dependants(&rec(1), &G).unwrap(), vec![rec(2), rec(77)]);
    let class = spot_app_api::EffectClassName::new("balance_movement").unwrap();
    assert_eq!(
        file.effects_by(Some(&class), Some(EffectKind::Internal))
            .len(),
        4
    );
    assert!(
        file.effects_by(Some(&class), Some(EffectKind::External))
            .is_empty()
    );
    assert_eq!(file.effects_by(None, None).len(), 4);
}

#[test]
fn outside_dependants_in_the_graph_are_listed_on_the_plan() {
    struct G;
    impl spot_bpm_effects::GraphDependants for G {
        fn dependants(
            &self,
            r: &spot_bpm_effects::RecordRef,
        ) -> Result<Vec<spot_bpm_effects::RecordRef>, String> {
            Ok(if *r == rec(2) {
                vec![rec(77)]
            } else {
                Vec::new()
            })
        }
    }
    let rig = Rig::new();
    let req = case("req20");
    rig.chain(&req);
    let plan = rig.svc.plan_undo(&req, None, &G).unwrap();
    assert_eq!(plan.outside_dependants().get(&1), Some(&vec![rec(77)]));
}

#[test]
fn a_cycle_or_a_graph_failure_fails_closed() {
    let rig = Rig::new();
    let req = case("req21");
    let (a, b) = (rec(1), rec(2));
    rig.svc.record(&req, new_entry("n1", &a, &[&b])).unwrap();
    rig.svc.record(&req, new_entry("n2", &b, &[&a])).unwrap();
    assert!(matches!(
        rig.svc.plan_undo(&req, None, &NoGraph),
        Err(EffectsError::DependencyCycle { .. })
    ));
    struct Down;
    impl spot_bpm_effects::GraphDependants for Down {
        fn dependants(
            &self,
            _: &spot_bpm_effects::RecordRef,
        ) -> Result<Vec<spot_bpm_effects::RecordRef>, String> {
            Err("Graph says: <secret>".to_owned())
        }
    }
    let err = rig.svc.plan_undo(&req, None, &Down).unwrap_err();
    assert!(matches!(&err, EffectsError::GraphUnavailable { code, .. } if code == "graph_error"));
}

#[test]
fn a_peer_text_refusal_code_is_replaced_by_a_stable_code() {
    let rig = Rig::new();
    let req = case("req22");
    rig.chain(&req);
    // The fake refuses with whatever code is in this table; use a hostile one.
    struct Hostile(Arc<FakeApp>);
    impl spot_bpm_effects::ReversalPort for Hostile {
        fn reverse(
            &self,
            _: &spot_bpm_effects::ReversalCall<'_>,
        ) -> Result<spot_app_api::OperationReceipt, spot_bpm_effects::PortError> {
            Err(spot_bpm_effects::PortError::Refused(
                spot_bpm_effects::Refusal {
                    code: "Refused: password=hunter2".to_owned(),
                    dependants: Vec::new(),
                    alternatives: Vec::new(),
                },
            ))
        }
        fn compensate(
            &self,
            c: &spot_bpm_effects::ReversalCall<'_>,
        ) -> Result<spot_app_api::OperationReceipt, spot_bpm_effects::PortError> {
            self.0.compensate(c)
        }
    }
    let hostile = Hostile(Arc::clone(&rig.app));
    let ctx = UndoContext {
        principal: WHO,
        now: NOW,
        port: &hostile,
        partner: &rig.partner,
    };
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    let UndoOutcome::NeedsDecision(d) = rig.svc.execute_undo(&plan, &ctx).unwrap() else {
        panic!()
    };
    assert_eq!(d.blockers[0].reason_code, codes::REVERSAL_REFUSED);
}

#[test]
fn an_empty_principal_is_refused() {
    let rig = Rig::new();
    let req = case("req23");
    rig.chain(&req);
    let plan = rig.svc.plan_undo(&req, None, &NoGraph).unwrap();
    let ctx = UndoContext {
        principal: "",
        now: NOW,
        port: rig.app.as_ref(),
        partner: &rig.partner,
    };
    assert!(matches!(
        rig.svc.execute_undo(&plan, &ctx),
        Err(EffectsError::EntryInvalid { .. })
    ));
}

#[test]
fn every_code_is_catalogued() {
    let catalogue = spot_errors::Catalogue::load();
    for code in [
        codes::REQUEST_LOCKED,
        codes::NODE_UNKNOWN,
        codes::REQUEST_UNKNOWN,
        codes::REWIND_WOULD_DUPLICATE,
        codes::STALE_PLAN,
        codes::DEPENDENCY_CYCLE,
        codes::LIMIT_EXCEEDED,
        codes::ENTRY_INVALID,
        codes::PORT_UNAVAILABLE,
        codes::CANCELLATION_FAILED,
        codes::NO_SUCH_DECISION,
        codes::GRAPH_UNAVAILABLE,
        codes::NO_UNDO,
        codes::POINT_OF_NO_RETURN,
        codes::REVERSAL_REFUSED,
    ] {
        assert!(
            catalogue.lookup(code).is_some(),
            "{code} has no catalogue entry"
        );
    }
}

#[test]
fn the_crate_has_no_storage_dependency_and_no_clock() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let deps = manifest.split("[dev-dependencies]").next().unwrap();
    assert!(
        !deps.contains("storage") && !deps.contains("sqlx"),
        "no store dependency"
    );
    let src = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).unwrap();
    for f in src {
        let text = std::fs::read_to_string(f.unwrap().path()).unwrap();
        for banned in [
            "SystemTime",
            "Instant::now",
            "now_utc",
            ".unwrap()",
            ".expect(",
            "unsafe {",
        ] {
            assert!(!text.contains(banned), "{banned} found");
        }
    }
}
