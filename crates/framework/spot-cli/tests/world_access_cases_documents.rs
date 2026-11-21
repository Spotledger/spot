//! WP-AI-07.T7: the cases part (over an in-memory `CaseDesk`) and the documents part (over an
//! in-memory `DocumentLifecycle`). Both record who acted and what was asked, so the tests prove the
//! actor reaches the subsystem, a refusal is a refusal, the hard floor holds, a change record
//! carries no field value, and a rehearsal never writes. The ops block on the runtime handle, so
//! each runs on a blocking thread, as the remedy host calls them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_cli::services::world_access::cases::{
    CaseDesk, DeskCasesPart, DeskFuture as CaseFuture, from_parts as cases_from_parts,
};
use spot_cli::services::world_access::documents::{
    DeskFuture as DocFuture, DocumentLifecycle, LifecycleDocumentsPart,
    from_parts as documents_from_parts,
};
use spot_cli::services::world_access::parts::{CasesPart, DocumentsPart};
use spot_remedies::{CaseLife, CaseRef, DocOp, DocRef, DocState, DocumentView};
use tokio::runtime::Handle;

async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(work).await.unwrap()
}

// ---------------------------------------------------------------------------------------------
// cases
// ---------------------------------------------------------------------------------------------

/// An in-memory desk: a user named `agent:blocked` is not permitted; every unstick is recorded.
struct Cases {
    lives: Mutex<BTreeMap<String, CaseLife>>,
    calls: Mutex<Vec<String>>,
}

impl Cases {
    fn new() -> Arc<Self> {
        let mut lives = BTreeMap::new();
        lives.insert("c1".to_owned(), CaseLife::Stuck);
        lives.insert("c2".to_owned(), CaseLife::Running);
        Arc::new(Self {
            lives: Mutex::new(lives),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl CaseDesk for Cases {
    fn stuck(&self) -> CaseFuture<'_, Vec<String>> {
        Box::pin(async move {
            let lives = self.lives.lock().unwrap();
            Ok::<Vec<String>, &'static str>(
                lives
                    .iter()
                    .filter(|(_, life)| **life == CaseLife::Stuck)
                    .map(|(id, _)| id.clone())
                    .collect(),
            )
        })
    }

    fn life<'a>(&'a self, case: &'a str) -> CaseFuture<'a, Option<CaseLife>> {
        Box::pin(async move {
            Ok::<Option<CaseLife>, &'static str>(self.lives.lock().unwrap().get(case).copied())
        })
    }

    fn unstick<'a>(&'a self, actor: &'a str, case: &'a str, _now: i64) -> CaseFuture<'a, CaseLife> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("{actor}|{case}"));
            if actor == "agent:blocked" {
                return Err("the actor may not unstick cases");
            }
            self.lives
                .lock()
                .unwrap()
                .insert(case.to_owned(), CaseLife::Running);
            Ok(CaseLife::Running)
        })
    }
}

fn case_part(desk: &Arc<Cases>) -> Arc<DeskCasesPart> {
    Arc::new(DeskCasesPart::new(
        Arc::clone(desk) as Arc<dyn CaseDesk>,
        Handle::current(),
    ))
}

fn c(id: &str) -> CaseRef {
    CaseRef::new(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stuck_case_is_unstuck_as_the_remedys_actor() {
    let desk = Cases::new();
    let part = case_part(&desk);
    let (stuck, change) = blocking(move || {
        let mut ops = part.live_as("agent:agent-t2");
        let stuck = ops.stuck();
        (stuck, ops.unstick(&c("c1"), 1_000).unwrap())
    })
    .await;
    assert_eq!(stuck, vec![c("c1")]);
    assert_eq!(
        (change.before, change.after),
        (CaseLife::Stuck, CaseLife::Running)
    );
    assert_eq!(desk.calls(), vec!["agent:agent-t2|c1".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_refusal_is_a_refusal_and_a_non_stuck_case_never_reaches_the_desk() {
    let desk = Cases::new();
    let part = case_part(&desk);
    let (blocked, running, missing) = blocking(move || {
        let mut ops = part.live_as("agent:blocked");
        let blocked = ops.unstick(&c("c1"), 1_000);
        let running = ops.unstick(&c("c2"), 1_000);
        (blocked, running, ops.unstick(&c("zz"), 1_000))
    })
    .await;
    assert_eq!(
        blocked.unwrap_err().detail,
        "the actor may not unstick cases"
    );
    assert!(running.unwrap_err().detail.contains("not stuck"));
    assert!(missing.unwrap_err().detail.contains("no case"));
    assert_eq!(desk.calls().len(), 1, "only the stuck case was asked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_actor_a_case_is_not_touched_and_restore_is_not_supported() {
    let desk = Cases::new();
    let part = case_part(&desk);
    let (no_actor, restore, same) = blocking(move || {
        let no_actor = part.live().unstick(&c("c1"), 1_000);
        let mut ops = part.live_as("agent:a");
        let change = ops.unstick(&c("c1"), 1_000).unwrap();
        let mut same = change.clone();
        same.before = CaseLife::Running;
        (no_actor, ops.restore(&change), ops.restore(&same))
    })
    .await;
    assert!(no_actor.unwrap_err().detail.contains("acting identity"));
    assert_eq!(
        restore.unwrap_err().detail,
        "not supported by the cases port"
    );
    assert!(same.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rehearsal_unstick_leaves_the_real_case_alone() {
    let desk = Cases::new();
    let part = case_part(&desk);
    let live = blocking(move || {
        let mut ops = part.rehearsal().unwrap();
        assert_eq!(ops.stuck(), vec![c("c1")]);
        let change = ops.unstick(&c("c1"), 1_000).unwrap();
        assert_eq!(change.after, CaseLife::Running);
        part.live().case(&c("c1")).unwrap().life
    })
    .await;
    assert_eq!(live, CaseLife::Stuck);
    assert!(
        desk.calls().is_empty(),
        "a rehearsal never reaches the desk"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cases_part_with_no_desk_is_none() {
    let desk = Cases::new();
    assert!(cases_from_parts(None, Some(Handle::current())).is_none());
    assert!(cases_from_parts(Some(Arc::clone(&desk) as Arc<dyn CaseDesk>), None).is_none());
    assert!(cases_from_parts(Some(desk as Arc<dyn CaseDesk>), Some(Handle::current())).is_some());
}

// ---------------------------------------------------------------------------------------------
// documents
// ---------------------------------------------------------------------------------------------

/// An in-memory lifecycle: `agent:blocked` is refused; every run is recorded.
struct Lifecycle {
    docs: Mutex<BTreeMap<String, DocumentView>>,
    runs: Mutex<Vec<String>>,
}

impl Lifecycle {
    fn new() -> Arc<Self> {
        let mut docs = BTreeMap::new();
        docs.insert(
            "DRAFT-1".to_owned(),
            DocumentView {
                state: DocState::Draft,
                financial: false,
            },
        );
        docs.insert(
            "SINV-1".to_owned(),
            DocumentView {
                state: DocState::Submitted,
                financial: true,
            },
        );
        Arc::new(Self {
            docs: Mutex::new(docs),
            runs: Mutex::new(Vec::new()),
        })
    }

    fn runs(&self) -> Vec<String> {
        self.runs.lock().unwrap().clone()
    }
}

impl DocumentLifecycle for Lifecycle {
    fn view<'a>(&'a self, doc: &'a str) -> DocFuture<'a, Option<DocumentView>> {
        Box::pin(async move {
            Ok::<Option<DocumentView>, &'static str>(self.docs.lock().unwrap().get(doc).copied())
        })
    }

    fn run<'a>(&'a self, actor: &'a str, doc: &'a str, op: &'a DocOp) -> DocFuture<'a, DocState> {
        Box::pin(async move {
            self.runs
                .lock()
                .unwrap()
                .push(format!("{actor}|{doc}|{}", op.name()));
            if actor == "agent:blocked" {
                return Err("the actor may not change documents");
            }
            let mut docs = self.docs.lock().unwrap();
            match docs.get_mut(doc) {
                Some(view) => {
                    if matches!(op, DocOp::Cancel { .. }) {
                        view.state = DocState::Cancelled;
                    }
                    Ok(view.state)
                }
                None => Err("no such document"),
            }
        })
    }
}

fn doc_part(lifecycle: &Arc<Lifecycle>) -> Arc<LifecycleDocumentsPart> {
    Arc::new(LifecycleDocumentsPart::new(
        Arc::clone(lifecycle) as Arc<dyn DocumentLifecycle>,
        Handle::current(),
    ))
}

fn d(name: &str) -> DocRef {
    DocRef::new(name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_runs_through_the_lifecycle_as_the_actor_and_carries_no_value() {
    let lifecycle = Lifecycle::new();
    let part = doc_part(&lifecycle);
    let change = blocking(move || {
        let mut ops = part.live_as("agent:agent-t2");
        ops.run(
            &d("DRAFT-1"),
            &DocOp::Recompute {
                field: "grand_total".to_owned(),
            },
        )
        .unwrap()
    })
    .await;
    assert_eq!(
        (change.before, change.after),
        (DocState::Draft, DocState::Draft)
    );
    assert_eq!(change.detail, "document_recompute");
    assert!(
        !format!("{change:?}").contains("grand_total"),
        "the change record names no field"
    );
    assert_eq!(
        lifecycle.runs(),
        vec!["agent:agent-t2|DRAFT-1|document_recompute".to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_refusal_is_a_refusal_and_without_an_actor_nothing_runs() {
    let lifecycle = Lifecycle::new();
    let part = doc_part(&lifecycle);
    let (blocked, no_actor, missing) = blocking(move || {
        let blocked = part
            .live_as("agent:blocked")
            .run(&d("DRAFT-1"), &DocOp::Repost);
        let no_actor = part.live().run(&d("DRAFT-1"), &DocOp::Repost);
        let missing = part.live_as("agent:a").run(&d("zz"), &DocOp::Repost);
        (blocked, no_actor, missing)
    })
    .await;
    assert_eq!(
        blocked.unwrap_err().detail,
        "the actor may not change documents"
    );
    assert!(no_actor.unwrap_err().detail.contains("acting identity"));
    assert!(missing.unwrap_err().detail.contains("no document"));
    assert_eq!(
        lifecycle.runs().len(),
        1,
        "only the blocked actor's call ran"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_submitted_financial_record_only_changes_through_an_allowed_route() {
    let lifecycle = Lifecycle::new();
    let part = doc_part(&lifecycle);
    let (direct, recompute, cancel) = blocking(move || {
        let mut ops = part.live_as("agent:a");
        let direct = ops.run(&d("SINV-1"), &DocOp::Repost);
        let recompute = ops.run(
            &d("SINV-1"),
            &DocOp::Recompute {
                field: "x".to_owned(),
            },
        );
        let cancel = ops.run(
            &d("SINV-1"),
            &DocOp::Cancel {
                reason: "duplicate".to_owned(),
            },
        );
        (direct, recompute, cancel)
    })
    .await;
    assert!(direct.unwrap_err().detail.contains("only through cancel"));
    assert!(recompute.is_err());
    let cancelled = cancel.unwrap();
    assert_eq!(cancelled.after, DocState::Cancelled);
    assert!(
        !cancelled.detail.contains("duplicate"),
        "a reason text never reaches the record"
    );
    assert_eq!(
        lifecycle.runs(),
        vec!["agent:a|SINV-1|document_cancel".to_owned()],
        "the refused edits never reached the lifecycle"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rehearsal_changes_an_overlay_and_leaves_the_live_document_untouched() {
    let lifecycle = Lifecycle::new();
    let part = doc_part(&lifecycle);
    let (rehearsed, again, live) = blocking(move || {
        let mut ops = part.rehearsal().unwrap();
        let change = ops
            .run(
                &d("SINV-1"),
                &DocOp::Cancel {
                    reason: "r".to_owned(),
                },
            )
            .unwrap();
        // A second rehearsal starts from the live state again.
        let other = part.rehearsal().unwrap();
        (
            change.after,
            other.view(&d("SINV-1")).unwrap().state,
            part.live().view(&d("SINV-1")).unwrap().state,
        )
    })
    .await;
    assert_eq!(rehearsed, DocState::Cancelled);
    assert_eq!(again, DocState::Submitted);
    assert_eq!(live, DocState::Submitted);
    assert!(lifecycle.runs().is_empty(), "a rehearsal never writes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hard_floor_holds_in_a_rehearsal_too() {
    let lifecycle = Lifecycle::new();
    let part = doc_part(&lifecycle);
    let refused = blocking(move || part.rehearsal().unwrap().run(&d("SINV-1"), &DocOp::Repost))
        .await
        .unwrap_err();
    assert!(refused.detail.contains("only through cancel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_documents_part_with_no_lifecycle_is_none() {
    let lifecycle = Lifecycle::new();
    assert!(documents_from_parts(None, Some(Handle::current())).is_none());
    assert!(
        documents_from_parts(
            Some(Arc::clone(&lifecycle) as Arc<dyn DocumentLifecycle>),
            None
        )
        .is_none()
    );
    assert!(
        documents_from_parts(
            Some(lifecycle as Arc<dyn DocumentLifecycle>),
            Some(Handle::current())
        )
        .is_some()
    );
}
