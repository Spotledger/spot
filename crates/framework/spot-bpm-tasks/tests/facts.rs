//! `DocumentServiceFacts`: the inbox judges a document task on the document the service loads.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use common::{block_on, now, spec};
use spot_app_api::context::{DocumentService, Invocation, Outcome};
use spot_app_api::document::{DocState, Document, DocumentPatch};
use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::{RecordId, SiteId, Value, Version};
use spot_app_api::{AppError, AppResult, BoxFuture, ErrorClass};
use spot_bpm_tasks::{
    DocumentServiceFacts, InboxAccess, MapDirectory, MapPrincipals, PermissionAccess,
    PrincipalSource, Task,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::{AppliesTo, AssignmentContext, Principal, Role, UserPermission};
use spot_permissions::{ModelTexts, PermissionEngine, Snapshot, load};

fn rid(byte: u8) -> RecordId {
    let mut bytes = [byte; 16];
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    RecordId::from_bytes(bytes).unwrap()
}

fn name(text: &str) -> spot_app_api::prelude::Name {
    spot_app_api::prelude::Name::new(text).unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    ReadError,
    NoState,
    OtherDoctype,
    Slow,
}

/// Counts the loads that started and the ones that were dropped before they finished.
#[derive(Default)]
struct Loads {
    started: AtomicUsize,
    finished: AtomicUsize,
    dropped_unfinished: AtomicUsize,
}

/// Returns `Pending` (and asks to be woken) `n` times, then ready.
struct YieldFor(u32);

impl Future for YieldFor {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 == 0 {
            return Poll::Ready(());
        }
        self.0 -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Records, on drop, a load that never finished.
struct LoadGuard<'a>(&'a Loads, bool);

impl Drop for LoadGuard<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.dropped_unfinished.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct FakeService {
    loads: Arc<Loads>,
    order: RecordId,
    customer: RecordId,
    posted: bool,
    mode: Mode,
}

fn refuse<T>() -> BoxFuture<'static, AppResult<T>> {
    Box::pin(async {
        Err(AppError::new(
            ErrorClass::Internal,
            "test.unsupported",
            "unsupported",
        ))
    })
}

impl DocumentService for FakeService {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        let def = DocTypeDef::new(
            name(doctype),
            "Order".to_owned(),
            self.posted,
            false,
            None,
            vec![],
        );
        Box::pin(async move {
            if self.mode == Mode::Slow {
                // Far past the old 64-poll bound.
                YieldFor(1_000).await;
            }
            Ok(Arc::new(def))
        })
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        // The load runs as the platform, never as a user.
        assert!(matches!(
            inv.actor,
            spot_app_api::context::Actor::System { .. }
        ));
        Box::pin(async move {
            self.loads.started.fetch_add(1, Ordering::SeqCst);
            let mut guard = LoadGuard(&self.loads, false);
            if self.mode == Mode::Slow {
                YieldFor(1_000).await;
            }
            guard.1 = true;
            self.loads.finished.fetch_add(1, Ordering::SeqCst);
            if self.mode == Mode::ReadError {
                return Err(AppError::new(ErrorClass::Internal, "test.read", "down"));
            }
            if id != self.order {
                return Err(AppError::new(
                    ErrorClass::NotFound,
                    "document.not_found",
                    "no",
                ));
            }
            let fields = BTreeMap::from([(name("customer"), Value::Id(self.customer))]);
            let state = match self.mode {
                Mode::NoState => None,
                _ => Some(DocState::Draft),
            };
            let kind = if self.mode == Mode::OtherDoctype {
                "invoice"
            } else {
                doctype
            };
            Ok(Outcome {
                value: Document::loaded(
                    name(kind),
                    id,
                    Version::FIRST,
                    state,
                    fields,
                    BTreeMap::new(),
                ),
                messages: vec![],
            })
        })
    }

    fn create<'a>(
        &'a self,
        _: &'a Invocation,
        _: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        refuse()
    }
    fn update<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        _: RecordId,
        _: DocumentPatch,
        _: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        refuse()
    }
    fn post<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        refuse()
    }
    fn cancel<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        refuse()
    }
    fn correct<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        refuse()
    }
    fn delete<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        refuse()
    }
    fn rename<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        _: RecordId,
        _: &'a str,
        _: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        refuse()
    }
    fn call_method<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        _: RecordId,
        _: &'a str,
        _: serde_json::Value,
        _: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
        refuse()
    }
    fn field_changed<'a>(
        &'a self,
        _: &'a Invocation,
        _: Document,
        _: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        refuse()
    }
}

fn engine() -> Arc<PermissionEngine> {
    let mut customer_def = Definition::new("customer");
    customer_def.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut order = Definition::new("order");
    let mut link = FieldSpec::new("customer", FieldType::Link);
    link.options = Some("customer".to_owned());
    order.fields = vec![link];
    let meta = Registry::new(vec![customer_def, order])
        .unwrap()
        .effective();
    let model = load(&ModelTexts {
        grants: "[[entries]]\nrole = \"Reader\"\ndoctype = \"order\"\nrights = [\"read\"]\n"
            .to_owned(),
        ..ModelTexts::default()
    })
    .unwrap();
    Arc::new(PermissionEngine::new(Arc::new(
        Snapshot::build(&meta, &model).unwrap(),
    )))
}

struct Principals;

impl PrincipalSource for Principals {
    fn principal(&self, user: &str) -> Principal {
        match user {
            "reader" => MapPrincipals::new()
                .with_roles("reader", &["Reader"])
                .principal(user),
            "east" => Principal::user(
                "east",
                vec![AssignmentContext {
                    id: "a".to_owned(),
                    roles: [Role("Reader".to_owned())].into_iter().collect(),
                    scope: vec![UserPermission {
                        user: String::new(),
                        doctype: None,
                        field_or_link_doctype: "order.customer".to_owned(),
                        allowed: vec![rid(1).to_string()],
                        applies_to: AppliesTo::AllDoctypes,
                        include_descendants: false,
                    }],
                }],
            ),
            _ => Principal::with_roles(user, &[]),
        }
    }
}

fn access(customer: u8, posted: bool, mode: Mode) -> PermissionAccess {
    access_with(customer, posted, mode, Arc::new(Loads::default()))
}

fn access_with(customer: u8, posted: bool, mode: Mode, loads: Arc<Loads>) -> PermissionAccess {
    let service = Arc::new(FakeService {
        loads,
        order: rid(9),
        customer: rid(customer),
        posted,
        mode,
    });
    let facts = DocumentServiceFacts::new(service, SiteId::new("bpm.test").unwrap(), Arc::new(now));
    PermissionAccess::new(
        engine(),
        Arc::new(Principals),
        Arc::new(MapDirectory::new()),
        Arc::new(facts),
    )
}

fn task(subject: &str) -> Task {
    let mut s = spec("t1", "case1", "approve", &["alice"]);
    s.doctype = Some("order".to_owned());
    s.subject = Some(subject.to_owned());
    Task::open(s)
}

#[test]
fn values_and_state_reach_the_engine() {
    let a = access(1, false, Mode::Ok);
    let t = task(&rid(9).to_string());
    assert!(block_on(a.may_see("reader", &t)));
    assert!(
        block_on(a.may_see("east", &t)),
        "customer 1 is inside east's scope"
    );
    assert!(!block_on(a.may_see("nobody", &t)));
    assert!(!a.may_act("reader", &t));
}

#[test]
fn a_value_outside_the_users_scope_denies() {
    let a = access(2, false, Mode::Ok);
    assert!(!block_on(a.may_see("east", &task(&rid(9).to_string()))));
    assert!(block_on(a.may_see("reader", &task(&rid(9).to_string()))));
}

#[test]
fn a_missing_document_or_bad_subject_denies() {
    let a = access(1, false, Mode::Ok);
    assert!(!block_on(a.may_see("reader", &task(&rid(8).to_string()))));
    assert!(!block_on(a.may_see("reader", &task("order_1"))));
}

#[test]
fn a_read_error_denies() {
    let a = access(1, false, Mode::ReadError);
    assert!(!block_on(a.may_see("reader", &task(&rid(9).to_string()))));
}

#[test]
fn a_posted_type_without_state_denies() {
    let a = access(1, true, Mode::NoState);
    assert!(!block_on(a.may_see("reader", &task(&rid(9).to_string()))));
    let draftless = access(1, false, Mode::NoState);
    assert!(block_on(
        draftless.may_see("reader", &task(&rid(9).to_string()))
    ));
}

#[test]
fn a_document_of_another_doctype_denies() {
    let a = access(1, false, Mode::OtherDoctype);
    assert!(!block_on(a.may_see("reader", &task(&rid(9).to_string()))));
}

#[test]
fn a_slow_service_still_resolves_past_the_old_poll_bound() {
    let a = access(1, false, Mode::Slow);
    let t = task(&rid(9).to_string());
    assert!(block_on(a.may_see("reader", &t)), "slow but healthy reads");
    assert!(!block_on(a.may_see("nobody", &t)));
    // A slow read that ends in an error still denies.
    let missing = task(&rid(8).to_string());
    assert!(!block_on(a.may_see("reader", &missing)));
}

#[test]
fn dropping_the_future_mid_load_leaves_nothing_behind() {
    let loads = Arc::new(Loads::default());
    let a = access_with(1, false, Mode::Slow, loads.clone());
    let t = task(&rid(9).to_string());
    {
        let mut fut = Box::pin(a.may_see("reader", &t));
        let mut cx = Context::from_waker(Waker::noop());
        // Poll until the load is under way (meta takes 1,000 polls first), then drop.
        for _ in 0..1_500 {
            assert!(fut.as_mut().poll(&mut cx).is_pending());
        }
    }
    assert_eq!(loads.started.load(Ordering::SeqCst), 1);
    assert_eq!(loads.finished.load(Ordering::SeqCst), 0);
    assert_eq!(loads.dropped_unfinished.load(Ordering::SeqCst), 1);
    // The next call works and answers correctly.
    assert!(block_on(a.may_see("reader", &t)));
    assert_eq!(loads.finished.load(Ordering::SeqCst), 1);
}
