//! The [`CampaignStore`] contract suite (feature `testing`).
//!
//! Every backend, the memory fake and PostgreSQL, must pass [`run_all`]. The suite is async
//! and runtime-free: the caller drives it with whatever executor it has. Each case uses a
//! tenant of its own, so a backend that shares one database between cases needs no cleanup.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;

use spot_tenancy::TenantId;

use crate::error::{CAMPAIGN_CLOSED, NOT_AUTHORIZED};
use crate::model::{
    AssignmentRef, CampaignSpec, CampaignStatus, DeadlinePolicy, Decision, ReviewItem,
    ReviewItemState,
};
use crate::store::{
    CampaignStore, HistoryEntry, MAX_ID_BYTES, MAX_PAGE, MAX_TEXT_BYTES, StoreError,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A tenant no other case (or earlier run) used.
#[must_use]
pub fn fresh_tenant() -> TenantId {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let mut entropy = [0_u8; 10];
    entropy[..8].copy_from_slice(&n.to_be_bytes());
    entropy[8] = 0x5a;
    TenantId::new_v7(1_700_000_000_000 + n, entropy)
}

/// Polls both futures on the calling task, interleaved, until both are done.
pub async fn join2<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = Box::pin(a);
    let mut b = Box::pin(b);
    let (mut ra, mut rb) = (None, None);
    std::future::poll_fn(|cx| {
        if ra.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            ra = Some(v);
        }
        if rb.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            rb = Some(v);
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (ra.unwrap(), rb.unwrap())
}

/// The spec every case uses (`deadline` 2000, opened at 1000).
#[must_use]
pub fn spec(id: &str, policy: DeadlinePolicy) -> CampaignSpec {
    CampaignSpec {
        id: id.to_string(),
        title: "Quarterly review".to_string(),
        opened_by: "sec-lead".to_string(),
        escalation_contact: "sec-lead".to_string(),
        deadline: 2000,
        stale_after_days: 30,
        on_deadline: policy,
    }
}

/// A fresh (used yesterday) item.
#[must_use]
pub fn item(id: &str, user: &str, reviewer: &str) -> ReviewItem {
    ReviewItem {
        id: id.to_string(),
        user: user.to_string(),
        assignment: AssignmentRef::new(format!("assign-{id}")),
        source: "direct".to_string(),
        last_use: Some(990),
        sod_override: None,
        reviewer: reviewer.to_string(),
    }
}

fn three() -> Vec<ReviewItem> {
    vec![
        item("i1", "alice", "bob"),
        item("i2", "carol", "bob"),
        item("i3", "dave", "erin"),
    ]
}

fn keep() -> Decision {
    Decision::Keep { note: None }
}

/// Runs every case. `make` returns a store over the same backend each call.
pub async fn run_all<S: CampaignStore>(make: impl Fn() -> S) {
    open_and_load_round_trip(&make()).await;
    open_twice_is_refused(&make()).await;
    decision_compare_and_set(&make()).await;
    racing_decisions_have_one_winner(&make()).await;
    self_decision_is_refused(&make()).await;
    refusals_are_recorded(&make()).await;
    tick_is_idempotent(&make()).await;
    tick_removal_actions_recorded_once(&make()).await;
    tenant_isolation(&make()).await;
    bounds_are_enforced(&make()).await;
    listing_and_history_paging(&make()).await;
    completed_campaign_is_closed(&make()).await;
    snapshot_is_never_recomputed(&make()).await;
}

async fn open_and_load_round_trip<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    let out = s
        .open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    assert_eq!(out.events.len(), 1);
    let stored = s.load(&t, "c1").await.unwrap();
    assert_eq!(stored.item_versions, vec![0, 0, 0]);
    assert_eq!(stored.campaign.items(), three().as_slice());
    assert_eq!(stored.campaign.status(), CampaignStatus::Open);
    assert_eq!(stored.campaign.opening_events(), out.events.as_slice());
}

async fn open_twice_is_refused<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    s.open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let again = s
        .open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await;
    assert_eq!(again.unwrap_err(), StoreError::AlreadyExists);
    // A state-machine refusal passes through with its own code.
    let bad = s
        .open(&t, spec("c2", DeadlinePolicy::LeaveOpen), vec![], 1000)
        .await
        .unwrap_err();
    assert_eq!(bad.code(), "review.spec_invalid");
}

async fn decision_compare_and_set<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    s.open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let stale = s.decide(&t, "c1", "i1", "bob", keep(), 7, 1100).await;
    assert_eq!(stale.unwrap_err(), StoreError::VersionConflict);
    let ok = s
        .decide(&t, "c1", "i1", "bob", keep(), 0, 1100)
        .await
        .unwrap();
    assert_eq!(ok.events.len(), 1);
    let stored = s.load(&t, "c1").await.unwrap();
    assert_eq!(stored.item_versions, vec![1, 0, 0]);
    assert!(matches!(
        stored.campaign.item_states()[0],
        ReviewItemState::Kept { .. }
    ));
    // Decided once: a replay with the new version is refused by the state machine.
    let twice = s.decide(&t, "c1", "i1", "bob", keep(), 1, 1200).await;
    assert_eq!(twice.unwrap_err().code(), NOT_AUTHORIZED);
}

async fn racing_decisions_have_one_winner<S: CampaignStore>(s: &S) {
    // Two decisions for one item, both quoting version 0, one remove and one keep.
    let t = fresh_tenant();
    let mut items = three();
    items[0].reviewer = "bob".to_string();
    s.open(&t, spec("c1", DeadlinePolicy::LeaveOpen), items, 1000)
        .await
        .unwrap();
    let remove = Decision::Remove {
        reason: "left the team".to_string(),
    };
    let (a, b) = join2(
        s.decide(&t, "c1", "i1", "bob", keep(), 0, 1100),
        s.decide(&t, "c1", "i1", "bob", remove, 0, 1100),
    )
    .await;
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "exactly one decision wins: {a:?} / {b:?}"
    );
    let stored = s.load(&t, "c1").await.unwrap();
    assert_eq!(stored.item_versions[0], 1, "decided exactly once");
    let history = s.history(&t, "c1", 0, MAX_PAGE).await.unwrap();
    let decided = history
        .iter()
        .filter(|r| {
            matches!(
                &r.entry,
                HistoryEntry::Event(crate::ReviewEvent::Decided { .. })
            )
        })
        .count();
    assert_eq!(decided, 1);
}

async fn self_decision_is_refused<S: CampaignStore>(s: &S) {
    // alice's access is escalated to alice herself (she is the escalation contact).
    let t = fresh_tenant();
    let mut sp = spec("c1", DeadlinePolicy::LeaveOpen);
    sp.escalation_contact = "alice".to_string();
    let items = vec![item("i1", "alice", "alice"), item("i2", "carol", "bob")];
    s.open(&t, sp, items, 1000).await.unwrap();
    let e = s
        .decide(&t, "c1", "i1", "alice", keep(), 0, 1100)
        .await
        .unwrap_err();
    assert_eq!(e.code(), NOT_AUTHORIZED);
    let stored = s.load(&t, "c1").await.unwrap();
    assert!(matches!(
        stored.campaign.item_states()[0],
        ReviewItemState::Pending { .. }
    ));
}

async fn refusals_are_recorded<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    s.open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let e = s.decide(&t, "c1", "i3", "bob", keep(), 0, 1100).await;
    assert_eq!(e.unwrap_err().code(), NOT_AUTHORIZED);
    let history = s.history(&t, "c1", 0, MAX_PAGE).await.unwrap();
    assert!(history.iter().any(|r| matches!(
        &r.entry,
        HistoryEntry::Refused { item, by, code } if item == "i3" && by == "bob" && code == NOT_AUTHORIZED
    )));
    // The refusal changed no state.
    assert_eq!(s.load(&t, "c1").await.unwrap().item_versions, vec![0, 0, 0]);
}

async fn tick_is_idempotent<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    let policy = DeadlinePolicy::Escalate {
        to: "cio".to_string(),
    };
    s.open(&t, spec("c1", policy), three(), 1000).await.unwrap();
    assert!(
        s.tick(&t, "c1", 1500).await.unwrap().is_empty(),
        "before the deadline"
    );
    let first = s.tick(&t, "c1", 2000).await.unwrap();
    assert_eq!(first.events.len(), 3);
    let (a, b) = join2(s.tick(&t, "c1", 2001), s.tick(&t, "c1", 2002)).await;
    assert!(a.unwrap().is_empty() && b.unwrap().is_empty());
    assert_eq!(s.load(&t, "c1").await.unwrap().item_versions, vec![1, 1, 1]);
    // The escalation target may now decide with the new version.
    s.decide(&t, "c1", "i1", "cio", keep(), 1, 2100)
        .await
        .unwrap();
}

async fn tick_removal_actions_recorded_once<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    s.open(
        &t,
        spec("c1", DeadlinePolicy::RemoveUnreviewed),
        three(),
        1000,
    )
    .await
    .unwrap();
    let first = s.tick(&t, "c1", 2000).await.unwrap();
    assert_eq!(first.actions.len(), 3);
    assert!(s.tick(&t, "c1", 2000).await.unwrap().is_empty());
    let history = s.history(&t, "c1", 0, MAX_PAGE).await.unwrap();
    let actions = history
        .iter()
        .filter(|r| matches!(r.entry, HistoryEntry::Action(_)))
        .count();
    assert_eq!(
        actions, 3,
        "a host that crashed can still find what it owes"
    );
    assert_eq!(
        s.load(&t, "c1").await.unwrap().campaign.status(),
        CampaignStatus::Completed
    );
}

async fn tenant_isolation<S: CampaignStore>(s: &S) {
    let (a, b) = (fresh_tenant(), fresh_tenant());
    s.open(&a, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let missing = s.load(&b, "c1").await.unwrap_err();
    assert_eq!(missing, StoreError::NotFound);
    assert_eq!(
        missing,
        s.load(&b, "nope").await.unwrap_err(),
        "same answer as a missing id"
    );
    assert_eq!(
        s.decide(&b, "c1", "i1", "bob", keep(), 0, 1100)
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        s.tick(&b, "c1", 2000).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        s.history(&b, "c1", 0, 10).await.unwrap_err(),
        StoreError::NotFound
    );
    assert!(s.list(&b, None, 10).await.unwrap().is_empty());
    // The same id in another tenant is a different campaign, and the first is untouched.
    s.open(&b, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    s.decide(&b, "c1", "i1", "bob", keep(), 0, 1100)
        .await
        .unwrap();
    assert_eq!(s.load(&a, "c1").await.unwrap().item_versions, vec![0, 0, 0]);
}

async fn bounds_are_enforced<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    let long_id = "x".repeat(MAX_ID_BYTES + 1);
    let e = s
        .open(&t, spec(&long_id, DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "access_review_pg.input_invalid");
    let mut items = three();
    items[0].source = "y".repeat(MAX_TEXT_BYTES + 1);
    assert!(matches!(
        s.open(&t, spec("c1", DeadlinePolicy::LeaveOpen), items, 1000)
            .await
            .unwrap_err(),
        StoreError::InputInvalid(_)
    ));
    s.open(&t, spec("c2", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let big = Decision::Remove {
        reason: "z".repeat(MAX_TEXT_BYTES + 1),
    };
    assert!(matches!(
        s.decide(&t, "c2", "i1", "bob", big, 0, 1100)
            .await
            .unwrap_err(),
        StoreError::InputInvalid(_)
    ));
    assert!(matches!(
        s.list(&t, None, 0).await.unwrap_err(),
        StoreError::InputInvalid(_)
    ));
    assert!(matches!(
        s.list(&t, None, MAX_PAGE + 1).await.unwrap_err(),
        StoreError::InputInvalid(_)
    ));
    assert!(matches!(
        s.history(&t, "c2", 0, MAX_PAGE + 1).await.unwrap_err(),
        StoreError::InputInvalid(_)
    ));
    assert!(matches!(
        s.load(&t, &long_id).await.unwrap_err(),
        StoreError::InputInvalid(_)
    ));
}

async fn listing_and_history_paging<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    for id in ["c-b", "c-a", "c-c"] {
        s.open(&t, spec(id, DeadlinePolicy::LeaveOpen), three(), 1000)
            .await
            .unwrap();
    }
    let page1 = s.list(&t, None, 2).await.unwrap();
    let ids: Vec<&str> = page1.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c-a", "c-b"]);
    let page2 = s.list(&t, Some("c-b"), 2).await.unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].id, "c-c");
    assert_eq!(page2[0].items, 3);
    s.decide(&t, "c-a", "i1", "bob", keep(), 0, 1100)
        .await
        .unwrap();
    let all = s.history(&t, "c-a", 0, MAX_PAGE).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].seq < all[1].seq);
    let rest = s.history(&t, "c-a", all[0].seq, MAX_PAGE).await.unwrap();
    assert_eq!(rest, all[1..]);
    let one = s.history(&t, "c-a", 0, 1).await.unwrap();
    assert_eq!(one.len(), 1);
}

async fn completed_campaign_is_closed<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    s.open(
        &t,
        spec("c1", DeadlinePolicy::LeaveOpen),
        vec![item("i1", "alice", "bob")],
        1000,
    )
    .await
    .unwrap();
    s.decide(&t, "c1", "i1", "bob", keep(), 0, 1100)
        .await
        .unwrap();
    assert_eq!(
        s.load(&t, "c1").await.unwrap().campaign.status(),
        CampaignStatus::Completed
    );
    let e = s
        .decide(&t, "c1", "i1", "bob", keep(), 1, 1200)
        .await
        .unwrap_err();
    assert_eq!(e.code(), CAMPAIGN_CLOSED);
    assert!(s.tick(&t, "c1", 3000).await.unwrap().is_empty());
}

async fn snapshot_is_never_recomputed<S: CampaignStore>(s: &S) {
    let t = fresh_tenant();
    let mut items = three();
    items[2].last_use = None;
    s.open(
        &t,
        spec("c1", DeadlinePolicy::LeaveOpen),
        items.clone(),
        1000,
    )
    .await
    .unwrap();
    // Later steps at a very different time leave the snapshot as it was taken.
    let noted = Decision::Keep {
        note: Some("still needed".to_string()),
    };
    s.decide(&t, "c1", "i1", "bob", noted, 0, 9_000_000)
        .await
        .unwrap();
    s.tick(&t, "c1", 9_000_001).await.unwrap();
    assert_eq!(
        s.load(&t, "c1").await.unwrap().campaign.items(),
        items.as_slice()
    );
}

struct ThreadWaker(std::thread::Thread);

impl std::task::Wake for ThreadWaker {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives a future to completion on the calling thread, for tests that have no runtime.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = std::task::Waker::from(std::sync::Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}
