//! Shared fixtures: a small DocType set, an in-memory document engine, and the guard ports.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_app_api::context::Actor;
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, SiteId};
use spot_bpm_governance::{
    GovernanceGuard, MemoryPolicyStore, MemoryPrincipals, MemoryRequests, NoRequests,
    RecordingAudit, RoleOutsideProcess,
};
use spot_core::ManualClock;
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::{AssignmentContext, Principal};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

/// Runs a future to completion on the current thread (every in-memory future is ready).
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A valid storage name.
pub fn n(name: &str) -> Name {
    Name::new(name).unwrap()
}

/// The fixed instant every test runs at.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

/// A later instant, for effective-dating tests.
pub fn later(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000 + seconds).unwrap()
}

/// The test site.
pub fn site() -> SiteId {
    SiteId::new("acme.test").unwrap()
}

/// A user record id, fixed so the audit actor is deterministic.
pub fn user_id() -> RecordId {
    RecordId::from_bytes([
        0x01, 0x9a, 0x00, 0x00, 0x00, 0x00, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x07,
    ])
    .unwrap()
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// The DocType set: a `note` that carries a company and a reason field, and a `customer`.
pub fn definitions() -> Vec<Definition> {
    let mut note = Definition::new("note");
    note.fields = vec![
        text("company"),
        text("title"),
        text("outside_process_reason"),
    ];

    let mut customer = Definition::new("customer");
    customer.fields = vec![text("company"), text("name")];

    vec![note, customer]
}

/// The effective metadata.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// An in-memory document engine over the test metadata, with `controllers` registered.
pub fn engine_with(
    controllers: Vec<(String, Arc<dyn spot_app_api::Controller>)>,
) -> Arc<Engine<FakeBackend>> {
    let mut builder = RegistryBuilder::new();
    for (app, controller) in controllers {
        builder.add_controller(&app, controller);
    }
    let seams = Arc::new(builder.build().unwrap());
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        seams,
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    block_on(engine.prepare()).unwrap();
    Arc::new(engine)
}

/// A user actor.
pub fn user_actor() -> Actor {
    Actor::User {
        id: user_id(),
        via: None,
    }
}

/// An import actor (a platform component, not a user).
pub fn import_actor() -> Actor {
    Actor::System {
        component: n("import"),
    }
}

/// A server-script actor (the triggering user).
pub fn script_actor() -> Actor {
    Actor::User {
        id: user_id(),
        via: None,
    }
}

/// A principal holding the outside-process role.
pub fn outside_principal() -> Principal {
    Principal::user(
        &user_id().to_string(),
        vec![AssignmentContext::unscoped(
            "default",
            &[spot_bpm_governance::OUTSIDE_PROCESS_PERMISSION],
        )],
    )
}

/// A principal holding no outside-process role.
pub fn plain_principal() -> Principal {
    Principal::with_roles(&user_id().to_string(), &["Viewer"])
}

/// The ports a guard is built over, kept together so a test can inspect them.
pub struct Ports {
    pub policy: Arc<MemoryPolicyStore>,
    pub requests: Arc<MemoryRequests>,
    pub audit: Arc<RecordingAudit>,
}

/// Builds a guard over a memory policy store, memory requests and a recording audit.
pub fn guard(ports: &Ports) -> Arc<GovernanceGuard> {
    Arc::new(GovernanceGuard::new(
        ports.policy.clone(),
        ports.requests.clone(),
        Arc::new(RoleOutsideProcess),
        ports.audit.clone(),
    ))
}

/// A guard whose authority verifies no request (the strict default).
pub fn guard_no_requests(ports: &Ports) -> Arc<GovernanceGuard> {
    Arc::new(GovernanceGuard::new(
        ports.policy.clone(),
        Arc::new(NoRequests),
        Arc::new(RoleOutsideProcess),
        ports.audit.clone(),
    ))
}

/// A principal source that maps the test user to `principal` and everyone else to the system.
pub fn principals(principal: Principal) -> Arc<MemoryPrincipals> {
    let source = MemoryPrincipals::new();
    source.insert(user_id(), principal);
    Arc::new(source)
}
