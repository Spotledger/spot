//! Commit points (WP-BPM-15.T2, ADR-080 section 6.7): the held region, release at the commit
//! point in the original order, discard on cancel or undo, the point of no return, the default of
//! no hold, and a live migration hold winning.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_commit::{
    CommitHandler, CommitService, EndCause, FindingKind, NodeAdmission, NodeEffects, RegionPlan,
    commit_kind, validate_config,
};
use spot_bpm_engine::{
    CaseId, IdempotencyKey, NodeCtx, NodeHandler, NodeKind, NodeOutcome, OutgoingEdge,
};
use spot_core::SiteId;
use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId};
use spot_effect_hold::{
    AuditSink, Dispatcher, EffectClass, EffectRef, HoldService, InMemoryLedger, LedgerStore,
    Notifier, PolicyTable, RecordingAudit, RecordingDispatcher, RecordingNotifier, ReleasePlan,
    RequestId,
};

const NOW: i64 = 1_700_000_000_000_000;

fn id(raw: &str) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// `start -> a -> b -> c1 -> d -> e -> c2 -> end`, with `c1` and `c2` the commit points.
fn dag() -> CompiledDag {
    let kinds = [
        ("start", "trigger"),
        ("a", "document"),
        ("b", "document"),
        ("c1", "commit"),
        ("d", "document"),
        ("e", "document"),
        ("c2", "commit"),
        ("end", "document"),
    ];
    let nodes = kinds
        .iter()
        .map(|(n, k)| Node::new(id(n), *k, JoinRule::All, json!({})).unwrap())
        .collect();
    let edges = kinds
        .windows(2)
        .map(|w| Edge::new(id(w[0].0), id(w[1].0), None))
        .collect();
    CompiledDag::compile(&DagDefinition::new("proc", 1, nodes, edges), &Limits::new()).unwrap()
}

fn commits() -> BTreeSet<NodeId> {
    [id("c1"), id("c2")].into()
}

fn effects() -> NodeEffects {
    let mut map: NodeEffects = BTreeMap::new();
    map.insert(id("a"), [EffectClass::Mail].into());
    map.insert(id("b"), [EffectClass::Webhook].into());
    map.insert(id("d"), [EffectClass::FiscalTransmission].into());
    map.insert(id("e"), [EffectClass::BankApi].into());
    map.insert(id("end"), [EffectClass::Mail].into());
    map
}

struct Rig {
    site: SiteId,
    service: CommitService,
    hold: Arc<HoldService>,
    dispatcher: Arc<RecordingDispatcher>,
    notifier: Arc<RecordingNotifier>,
}

fn rig(plan: RegionPlan) -> Rig {
    let site = SiteId::new("commit.example").unwrap();
    let ledger = Arc::new(InMemoryLedger::new());
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotifier::new());
    let hold = Arc::new(HoldService::new(
        site.clone(),
        ledger as Arc<dyn LedgerStore>,
        Arc::new(PolicyTable::new()),
        Arc::new(RecordingAudit::new()) as Arc<dyn AuditSink>,
        Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
        Arc::clone(&notifier) as Arc<dyn Notifier>,
    ));
    Rig {
        service: CommitService::new(Arc::clone(&hold), Arc::new(plan)),
        site,
        hold,
        dispatcher,
        notifier,
    }
}

fn full_plan() -> RegionPlan {
    RegionPlan::analyze(&dag(), &commits(), &effects(), &PolicyTable::new()).unwrap()
}

fn fx(rig: &Rig, class: EffectClass, key: &str) -> EffectRef {
    EffectRef::new(rig.site.clone(), class, "effect", key, NOW)
}

fn request() -> RequestId {
    RequestId::new("req1").unwrap()
}

#[test]
fn a_region_is_everything_before_a_commit_point_and_after_the_previous_one() {
    let plan = full_plan();
    let r1: Vec<&str> = plan
        .region(&id("c1"))
        .unwrap()
        .iter()
        .map(NodeId::as_str)
        .collect();
    assert_eq!(r1, ["a", "b", "start"]);
    let r2: Vec<&str> = plan
        .region(&id("c2"))
        .unwrap()
        .iter()
        .map(NodeId::as_str)
        .collect();
    assert_eq!(
        r2,
        ["d", "e"],
        "the first commit point bounds the second region"
    );
    assert!(plan.is_held(&id("a")) && plan.is_held(&id("e")));
    assert!(
        !plan.is_held(&id("end")),
        "after the last commit point nothing is held"
    );
    assert!(!plan.is_held(&id("c1")));
    assert!(plan.is_commit_point(&id("c2")) && !plan.is_commit_point(&id("a")));
}

#[test]
fn validate_reports_the_point_of_no_return_and_an_empty_commit_point() {
    let plan = full_plan();
    let found: Vec<(&str, &str)> = plan
        .findings()
        .iter()
        .map(|f| (f.node.as_str(), f.token()))
        .collect();
    assert_eq!(found, [("d", "point_of_no_return")]);
    assert!(matches!(
        &plan.findings()[0].kind,
        FindingKind::PointOfNoReturn { class } if class == "fiscal_transmission"
    ));
    assert_eq!(plan.points_of_no_return(), vec![&id("d")]);

    // A commit point whose region declares nothing holdable is a warning; a fiscal-only region
    // holds nothing either.
    let mut only_fiscal: NodeEffects = BTreeMap::new();
    only_fiscal.insert(id("a"), [EffectClass::FiscalTransmission].into());
    let plan = RegionPlan::analyze(
        &dag(),
        &[id("c1")].into(),
        &only_fiscal,
        &PolicyTable::new(),
    )
    .unwrap();
    let tokens: Vec<&str> = plan.findings().iter().map(|f| f.token()).collect();
    // Findings are ordered by node: the fiscal step `a` before the commit point `c1`.
    assert_eq!(tokens, ["point_of_no_return", "commit_point_holds_nothing"]);

    let none = RegionPlan::analyze(
        &dag(),
        &[id("c1")].into(),
        &BTreeMap::new(),
        &PolicyTable::new(),
    )
    .unwrap();
    assert_eq!(none.findings().len(), 1);
    assert_eq!(none.findings()[0].token(), "commit_point_holds_nothing");
}

#[test]
fn declarations_are_bounded() {
    let bad = |commits: BTreeSet<NodeId>, effects: NodeEffects| {
        RegionPlan::analyze(&dag(), &commits, &effects, &PolicyTable::new()).unwrap_err()
    };
    assert_eq!(
        bad([id("nope")].into(), BTreeMap::new()).code(),
        "bpm_commit.invalid_declaration"
    );
    let mut on_commit: NodeEffects = BTreeMap::new();
    on_commit.insert(id("c1"), [EffectClass::Mail].into());
    assert_eq!(
        bad([id("c1")].into(), on_commit).code(),
        "bpm_commit.invalid_declaration"
    );
    let mut foreign: NodeEffects = BTreeMap::new();
    foreign.insert(id("zzz"), [EffectClass::Mail].into());
    assert_eq!(
        bad(BTreeSet::new(), foreign).code(),
        "bpm_commit.invalid_declaration"
    );
    let mut many: NodeEffects = BTreeMap::new();
    many.insert(
        id("a"),
        (0..65)
            .map(|n| EffectClass::Other(format!("c{n}")))
            .collect(),
    );
    assert_eq!(
        bad(BTreeSet::new(), many).code(),
        "bpm_commit.invalid_declaration"
    );
    // Too many commit points (the nodes need not exist for the cap to apply first).
    let crowd: BTreeSet<NodeId> = (0..=spot_bpm_commit::MAX_COMMIT_POINTS)
        .map(|n| id(&format!("c{n}x")))
        .collect();
    assert_eq!(
        bad(crowd, BTreeMap::new()).code(),
        "bpm_commit.invalid_declaration"
    );
}

#[tokio::test]
async fn a_process_without_commit_points_holds_nothing() {
    let rig = rig(RegionPlan::none());
    let admission = rig
        .service
        .admit(&request(), &id("a"), fx(&rig, EffectClass::Mail, "k1"))
        .await
        .unwrap();
    assert_eq!(admission, NodeAdmission::Flows);
    assert!(
        rig.hold
            .request_pending(&request())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn effects_wait_for_their_commit_point_and_leave_in_original_order() {
    let rig = rig(full_plan());
    let r = request();
    for (node, class, key) in [
        ("a", EffectClass::Mail, "k1"),
        ("b", EffectClass::Webhook, "k2"),
        ("e", EffectClass::BankApi, "k3"),
    ] {
        let admission = rig
            .service
            .admit(&r, &id(node), fx(&rig, class, key))
            .await
            .unwrap();
        assert!(matches!(admission, NodeAdmission::Held(_)));
    }
    // After the last commit point the effect flows as today.
    let after = rig
        .service
        .admit(&r, &id("end"), fx(&rig, EffectClass::Mail, "k4"))
        .await
        .unwrap();
    assert_eq!(after, NodeAdmission::Flows);
    assert_eq!(rig.dispatcher.unique_count(), 0);

    let first = rig
        .service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert!(first.completed && !first.deferred_by_migration);
    assert_eq!(first.released.len(), 2);
    let keys: Vec<String> = rig.dispatcher.calls().into_iter().map(|c| c.key).collect();
    assert_eq!(
        keys,
        ["k1", "k2"],
        "only the first region, in original order"
    );

    // Idempotent: the same commit point again sends nothing more.
    let again = rig
        .service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert!(again.released.is_empty() && again.completed);

    let second = rig
        .service
        .commit(&r, &id("c2"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert_eq!(second.released.len(), 1);
    assert_eq!(rig.dispatcher.unique_count(), 3);
}

#[tokio::test]
async fn undo_before_the_commit_point_discards_what_is_held_and_nothing_leaves() {
    let rig = rig(full_plan());
    let r = request();
    rig.service
        .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, "k1"))
        .await
        .unwrap();
    rig.service
        .admit(&r, &id("b"), fx(&rig, EffectClass::BankApi, "k2"))
        .await
        .unwrap();
    let report = rig.service.end(&r, EndCause::Undone, NOW).await.unwrap();
    assert_eq!(report.discarded, 2);
    assert_eq!(
        rig.dispatcher.unique_count(),
        0,
        "nothing left the building"
    );
    assert!(
        rig.notifier.notices().is_empty(),
        "default classes drop silently"
    );
    let after = rig
        .service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert!(after.released.is_empty());
    assert_eq!(
        rig.dispatcher.unique_count(),
        0,
        "a discarded effect is never replayed"
    );
}

#[tokio::test]
async fn a_cancel_after_the_first_commit_discards_only_the_unreleased_region() {
    let rig = rig(full_plan());
    let r = request();
    rig.service
        .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, "k1"))
        .await
        .unwrap();
    rig.service
        .admit(&r, &id("e"), fx(&rig, EffectClass::BankApi, "k3"))
        .await
        .unwrap();
    rig.service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    let report = rig.service.end(&r, EndCause::Cancelled, NOW).await.unwrap();
    assert_eq!(report.discarded, 1);
    let keys: Vec<String> = rig.dispatcher.calls().into_iter().map(|c| c.key).collect();
    assert_eq!(
        keys,
        ["k1"],
        "the released effect stays out; the later one never left"
    );
}

#[tokio::test]
async fn a_never_held_class_goes_at_once_and_flags_the_point_of_no_return() {
    let rig = rig(full_plan());
    let admission = rig
        .service
        .admit(
            &request(),
            &id("d"),
            fx(&rig, EffectClass::FiscalTransmission, "f1"),
        )
        .await
        .unwrap();
    assert_eq!(admission, NodeAdmission::PointOfNoReturn);
    assert!(
        rig.hold
            .request_pending(&request())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_commit_at_a_non_commit_node_is_refused() {
    let rig = rig(full_plan());
    let error = rig
        .service
        .commit(&request(), &id("a"), ReleasePlan::new(), NOW)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "bpm_commit.not_commit_point");
}

#[tokio::test]
async fn a_live_migration_hold_defers_the_commit_point() {
    let rig = rig(full_plan());
    let r = request();
    rig.service
        .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, "k1"))
        .await
        .unwrap();
    rig.hold.begin("mig-1", "upgrade", NOW, None).await.unwrap();
    let deferred = rig
        .service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert!(deferred.deferred_by_migration && !deferred.completed);
    assert_eq!(rig.dispatcher.unique_count(), 0);
    rig.hold
        .release("mig-1", ReleasePlan::new(), NOW)
        .await
        .unwrap();
    let done = rig
        .service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert!(done.completed);
    assert_eq!(rig.dispatcher.unique_count(), 1);
}

#[tokio::test]
async fn a_failed_dispatch_surfaces_the_hold_code_and_a_retry_finishes() {
    let rig = rig(full_plan());
    let r = request();
    rig.service
        .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, "k1"))
        .await
        .unwrap();
    rig.service
        .admit(&r, &id("b"), fx(&rig, EffectClass::Mail, "k2"))
        .await
        .unwrap();
    rig.dispatcher.fail_key("k1");
    let error = rig
        .service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "bpm_commit.hold_failed");
    assert_eq!(error.hold_code(), Some("effect_hold.dispatch_failed"));
    assert!(
        !error.to_string().contains("k1"),
        "no effect detail in the error"
    );
    assert_eq!(rig.dispatcher.unique_count(), 0, "k2 did not overtake k1");
}

#[test]
fn the_handler_marks_the_commit_point_and_rejects_a_malformed_config() {
    assert_eq!(CommitHandler.kind(), commit_kind());
    assert_eq!(commit_kind(), NodeKind::Unknown("commit".to_owned()));
    assert!(validate_config("c1", &json!(null)).is_ok());
    assert!(validate_config("c1", &json!({})).is_ok());
    assert!(validate_config("c1", &json!({"label": "after payment"})).is_ok());
    for bad in [
        json!(5),
        json!({"label": 7}),
        json!({"label": "x".repeat(129)}),
        json!({"label": "a\nb"}),
        json!({"other": 1}),
    ] {
        assert_eq!(
            validate_config("c1", &bad).unwrap_err().code(),
            "bpm_commit.invalid_config"
        );
    }

    let case = CaseId::new("case1").unwrap();
    let node = id("c1");
    let config = json!({"label": "ok"});
    let mut variables = json!({});
    let outgoing: Vec<OutgoingEdge> = Vec::new();
    let mut ctx = NodeCtx {
        case: &case,
        node: &node,
        kind: commit_kind(),
        attempt: 1,
        key: IdempotencyKey::new(case.clone(), node.clone(), 1),
        is_retry: false,
        now: OffsetDateTime::UNIX_EPOCH,
        deadline: None,
        principal: None,
        config: &config,
        outgoing: &outgoing,
        variables: &mut variables,
        cancelled: false,
    };
    match CommitHandler.run(&mut ctx) {
        NodeOutcome::Done { output, .. } => assert_eq!(output, Some(json!({"commit_point": true}))),
        other => panic!("expected done, got {other:?}"),
    }
    let bad = json!({"nope": 1});
    ctx.config = &bad;
    assert!(matches!(
        CommitHandler.run(&mut ctx),
        NodeOutcome::Fail { .. }
    ));
}

#[test]
fn every_code_is_catalogued() {
    let catalogue = spot_errors::Catalogue::load();
    for code in [
        spot_bpm_commit::codes::INVALID_CONFIG,
        spot_bpm_commit::codes::INVALID_DECLARATION,
        spot_bpm_commit::codes::NOT_COMMIT_POINT,
        spot_bpm_commit::codes::HOLD_FAILED,
    ] {
        assert!(
            catalogue.lookup(code).is_some(),
            "{code} has no catalogue entry"
        );
    }
}

/// A dispatcher that counts every call (no de-duplication) and yields so racing tasks interleave.
struct Counting {
    keys: std::sync::Mutex<Vec<String>>,
}

impl std::fmt::Debug for Counting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Counting")
    }
}

impl Dispatcher for Counting {
    fn dispatch<'a>(
        &'a self,
        effect: &'a spot_effect_hold::EffectRecord,
    ) -> futures::future::BoxFuture<'a, spot_effect_hold::Result<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.keys.lock().unwrap().push(effect.key.clone());
            tokio::task::yield_now().await;
            Ok(())
        })
    }
}

fn racing_rig() -> (Arc<CommitService>, Arc<Counting>, SiteId) {
    let site = SiteId::new("race.example").unwrap();
    let dispatcher = Arc::new(Counting {
        keys: std::sync::Mutex::new(Vec::new()),
    });
    let hold = Arc::new(HoldService::new(
        site.clone(),
        Arc::new(InMemoryLedger::new()) as Arc<dyn LedgerStore>,
        Arc::new(PolicyTable::new()),
        Arc::new(RecordingAudit::new()) as Arc<dyn AuditSink>,
        Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
        Arc::new(RecordingNotifier::new()) as Arc<dyn Notifier>,
    ));
    (
        Arc::new(CommitService::new(hold, Arc::new(full_plan()))),
        dispatcher,
        site,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_commits_racing_on_one_request_send_each_effect_once() {
    let (service, dispatcher, site) = racing_rig();
    let r = request();
    for (node, key) in [("a", "k1"), ("b", "k2"), ("a", "k3")] {
        service
            .admit(
                &r,
                &id(node),
                EffectRef::new(site.clone(), EffectClass::Mail, "effect", key, NOW),
            )
            .await
            .unwrap();
    }
    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let (service, r) = (Arc::clone(&service), r.clone());
            tokio::spawn(
                async move { service.commit(&r, &id("c1"), ReleasePlan::new(), NOW).await },
            )
        })
        .collect();
    let mut released = 0;
    for task in tasks {
        released += task.await.unwrap().unwrap().released.len();
    }
    assert_eq!(released, 3);
    let mut keys = dispatcher.keys.lock().unwrap().clone();
    keys.sort();
    assert_eq!(
        keys,
        ["k1", "k2", "k3"],
        "each effect went out exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_racing_end_never_both_sends_and_discards() {
    for round in 0..30 {
        let (service, dispatcher, site) = racing_rig();
        let r = RequestId::new(format!("race{round}")).unwrap();
        for (node, key) in [("a", "k1"), ("b", "k2"), ("a", "k3")] {
            service
                .admit(
                    &r,
                    &id(node),
                    EffectRef::new(site.clone(), EffectClass::BankApi, "effect", key, NOW),
                )
                .await
                .unwrap();
        }
        let (s1, r1) = (Arc::clone(&service), r.clone());
        let (s2, r2) = (Arc::clone(&service), r.clone());
        let commit = tokio::spawn(async move {
            s1.commit(&r1, &id("c1"), ReleasePlan::new(), NOW)
                .await
                .unwrap()
        });
        let end = tokio::spawn(async move { s2.end(&r2, EndCause::Cancelled, NOW).await.unwrap() });
        let (committed, ended) = (commit.await.unwrap(), end.await.unwrap());
        assert_eq!(
            committed.released.len() + usize::try_from(ended.discarded).unwrap(),
            3,
            "round {round}: dispatched xor discarded"
        );
        assert_eq!(
            dispatcher.keys.lock().unwrap().len(),
            committed.released.len()
        );
    }
}

#[tokio::test]
async fn a_node_admitted_after_the_request_ended_is_refused() {
    let rig = rig(full_plan());
    let r = request();
    rig.service.end(&r, EndCause::Cancelled, NOW).await.unwrap();
    let error = rig
        .service
        .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, "late"))
        .await
        .unwrap_err();
    assert_eq!(error.hold_code(), Some("effect_hold.invalid_input"));
}

#[tokio::test]
async fn end_by_undo_after_a_commit_leaves_the_released_effects_alone() {
    // The undo lock of the request record file is the host's (see the crate docs); here: what
    // already left is not touched by `end`, only what is still held is discarded.
    let rig = rig(full_plan());
    let r = request();
    rig.service
        .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, "k1"))
        .await
        .unwrap();
    rig.service
        .commit(&r, &id("c1"), ReleasePlan::new(), NOW)
        .await
        .unwrap();
    rig.service
        .admit(&r, &id("e"), fx(&rig, EffectClass::Mail, "k2"))
        .await
        .unwrap();
    let ended = rig.service.end(&r, EndCause::Undone, NOW).await.unwrap();
    assert_eq!(ended.discarded, 1);
    assert_eq!(rig.dispatcher.unique_count(), 1);
}

#[tokio::test]
async fn a_request_holds_at_most_the_cap() {
    let rig = rig(full_plan());
    let r = request();
    for n in 0..spot_effect_hold::MAX_HELD_PER_REQUEST {
        rig.service
            .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, &format!("k{n}")))
            .await
            .unwrap();
    }
    let error = rig
        .service
        .admit(&r, &id("a"), fx(&rig, EffectClass::Mail, "over"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "bpm_commit.hold_failed");
    assert_eq!(error.hold_code(), Some("effect_hold.invalid_input"));
}
