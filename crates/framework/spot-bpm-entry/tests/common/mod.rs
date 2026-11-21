//! Shared fixtures: a small DocType set, an in-memory document engine, and the entry ports.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_app_api::{Controller, Document};
use spot_bpm_entry::{
    EntryProcesses, MemoryEntrySettings, MemoryRequests, MemoryScopes, OwnershipController,
    ProcessOnlyController, ProcessOnlyGuard, RequestAuthority, ScopedPolicyStore,
};
use spot_bpm_governance::RecordingAudit;
use spot_bpm_governance::actor_text;
use spot_core::ManualClock;
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
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

/// A later instant.
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

/// The DocType set: an `employee` (postable) that carries a company, a name and the owner flag.
pub fn definitions() -> Vec<Definition> {
    let mut employee = Definition::new("employee");
    employee.is_posted = true;
    employee.fields = vec![text("company"), text("full_name"), text("owned_by_request")];
    vec![employee]
}

/// The effective metadata.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// An in-memory document engine with `controllers` registered.
pub fn engine_with(controllers: Vec<Arc<dyn Controller>>) -> Arc<Engine<FakeBackend>> {
    let mut builder = RegistryBuilder::new();
    for controller in controllers {
        builder.add_controller("bpm", controller);
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

/// The acting person's name as the request authority and the scope resolver know it.
pub fn text_of(actor: &Actor) -> String {
    actor_text(actor)
}

/// An invocation, optionally carrying a request context as its command id.
pub fn invocation(actor: Actor, command_id: Option<&str>) -> Invocation {
    Invocation {
        site: site(),
        actor,
        command_id: command_id.and_then(|c| CommandId::new(c).ok()),
        language: None,
        now: now(),
    }
}

/// An `employee` document for `scope`, optionally flagged as owned by `owner`.
pub fn employee(scope: &str, owner: Option<&str>) -> Document {
    let mut doc = Document::new(n("employee"), RecordId::new_v7());
    doc.set(n("company"), Value::Text(scope.to_owned()));
    doc.set(n("full_name"), Value::Text("Amina Rauf".to_owned()));
    if let Some(owner) = owner {
        doc.set(n("owned_by_request"), Value::Text(owner.to_owned()));
    }
    doc
}

/// The entry ports, kept together so a test can inspect them.
pub struct Ports {
    pub scopes: Arc<MemoryScopes>,
    pub modes: Arc<ScopedPolicyStore>,
    pub settings: Arc<MemoryEntrySettings>,
    pub requests: Arc<MemoryRequests>,
    pub audit: Arc<RecordingAudit>,
}

impl Ports {
    pub fn new() -> Self {
        let scopes = MemoryScopes::new().shared();
        // Both channels' actors may create in the `acme` company and everything below it; the
        // `other` company exists but nobody here is assigned to it.
        for actor in [user_actor(), import_actor()] {
            scopes.grant(&actor_text(&actor), "acme");
        }
        scopes.define("acme/north");
        scopes.define("acme/north/shop1");
        scopes.define("other");
        Self {
            scopes,
            modes: ScopedPolicyStore::new(8).shared(),
            settings: MemoryEntrySettings::new().shared(),
            requests: MemoryRequests::new().shared(),
            audit: RecordingAudit::new().shared(),
        }
    }

    pub fn guard(&self) -> Arc<ProcessOnlyGuard> {
        Arc::new(ProcessOnlyGuard::new(
            self.modes.clone(),
            Arc::new(EntryProcesses::new(self.settings.clone())),
            self.requests.clone(),
            self.audit.clone(),
        ))
    }

    /// A grant for `actor` at `node` of `request`, attempt `attempt`, as the engine would issue.
    pub fn grant(&self, actor: &Actor, request: &str, node: &str, attempt: u32) -> String {
        self.requests
            .issue(request, node, attempt, &actor_text(actor))
            .unwrap()
    }

    /// The engine with the process_only controller and the ownership controller for `employee`.
    pub fn engine(&self) -> Arc<Engine<FakeBackend>> {
        let guard: Arc<dyn Controller> = Arc::new(ProcessOnlyController::new(
            "employee",
            self.guard(),
            self.scopes.clone(),
        ));
        let ownership: Arc<dyn Controller> =
            Arc::new(OwnershipController::new("employee", self.requests.clone()));
        engine_with(vec![guard, ownership])
    }
}
