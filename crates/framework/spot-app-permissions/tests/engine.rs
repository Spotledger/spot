//! The adapter through the real document engine on the fake backend (WP-PLT-03.T3): a real
//! `PermissionAuthorizer` and `SnapshotAppPermissions`, no `MemoryAppPermissions`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, MethodContext};
use spot_app_api::controller::{Controller, MethodDecl, MethodRight, StepDecl, StepName};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, Document};
use spot_app_permissions::{DENIAL_LOG_CAPACITY, SnapshotAppPermissions};
use spot_core::ManualClock;
use spot_document::engine::{Authorizer, Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::{
    AppPermissionCatalogue, AppPermissionDecl, AppPermissionGrant, AppPermissionName, ModelTexts,
    PermissionEngine, Principal, Role, Snapshot, load,
};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition};
use spot_storage_fake::FakeBackend;

const APPROVE: &str = "approve_note";
const ROLE_SECRET: &str = "secret_approver_role";

fn n(text: &str) -> Name {
    Name::new(text).unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn meta() -> Arc<EffectiveMeta> {
    let mut note = Definition::new("note");
    note.fields = vec![FieldSpec::new("text", FieldType::Text)];
    Registry::new(vec![note]).unwrap().effective()
}

/// Everyone who may open a note holds `read`; the approver role also holds the permission.
fn model_texts() -> ModelTexts {
    let mut grants = String::new();
    for role in [ROLE_SECRET, "reader"] {
        grants.push_str(&format!(
            "[[entries]]\ndoctype = \"note\"\nrole = \"{role}\"\nrights = [\"read\"]\n\n"
        ));
    }
    ModelTexts {
        grants,
        field_grants: String::new(),
        groups: String::new(),
        shares: String::new(),
    }
}

fn catalogue(declared: bool) -> AppPermissionCatalogue {
    let mut c = AppPermissionCatalogue::new();
    if declared {
        c.insert(
            AppPermissionName::parse(APPROVE).unwrap(),
            AppPermissionDecl {
                app: "notes".into(),
                title: "Approve notes".into(),
                description: "May approve a note".into(),
            },
        )
        .unwrap();
    }
    c
}

/// A snapshot where `ROLE_SECRET` is granted the permission (when `granted`), and the
/// permission is in the catalogue (when `declared`).
fn permission_engine(declared: bool, granted: bool) -> Arc<PermissionEngine> {
    let model = load(&model_texts()).unwrap();
    let grants = if granted {
        vec![AppPermissionGrant {
            role: Role(ROLE_SECRET.into()),
            permission: AppPermissionName::parse(APPROVE).unwrap(),
        }]
    } else {
        vec![]
    };
    let snapshot =
        Snapshot::build_with_apps(&meta(), &model, &catalogue(declared), &grants).unwrap();
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

fn principals() -> BTreeMap<&'static str, (RecordId, Principal)> {
    let mut out = BTreeMap::new();
    for (name, principal) in [
        ("ann", Principal::with_roles("ann", &[ROLE_SECRET])),
        ("bob", Principal::with_roles("bob", &["reader"])),
        ("blind", Principal::with_roles("blind", &["nothing"])),
    ] {
        out.insert(name, (RecordId::new_v7(), principal));
    }
    out
}

#[derive(Clone)]
struct Sample {
    ran: Arc<AtomicUsize>,
}

impl Controller for Sample {
    fn doctype(&self) -> &str {
        "note"
    }
    fn steps(&self) -> Vec<StepDecl> {
        Vec::new()
    }
    fn methods(&self) -> Vec<MethodDecl> {
        vec![MethodDecl::new(n("approve"), true).with_right(MethodRight::Permission(n(APPROVE)))]
    }
    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn call_method<'a>(
        &'a self,
        _method: &'a str,
        _args: Json,
        _ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(json!({"ran": true})) })
    }
}

/// One site: its engine, its authorizer, its adapter, and the document it holds.
struct Site {
    engine: Engine<FakeBackend>,
    authorizer: Arc<PermissionAuthorizer>,
    app: Arc<SnapshotAppPermissions>,
    ids: BTreeMap<&'static str, RecordId>,
    id: RecordId,
}

async fn site(ran: Arc<AtomicUsize>, declared: bool, granted: bool) -> Site {
    let people = principals();
    let mut memory = MemoryPrincipals::new();
    for (id, principal) in people.values() {
        memory = memory.with(id, principal.clone());
    }
    let authorizer = Arc::new(PermissionAuthorizer::new(
        permission_engine(declared, granted),
        Arc::new(memory),
    ));
    let app = Arc::new(SnapshotAppPermissions::new(authorizer.clone()));
    let sample = Sample { ran };
    let mut builder = RegistryBuilder::new();
    builder.add_controller("sample", Arc::new(sample.clone()));
    let as_authorizer: Arc<dyn Authorizer> = authorizer.clone();
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_authorizer(as_authorizer)
    .with_controllers(vec![Arc::new(sample)])
    .with_app_permissions(app.clone());
    engine.prepare().await.unwrap();
    let mut doc = Document::new(n("note"), RecordId::new_v7());
    doc.set(n("text"), Value::Text("hello".into()));
    let id = engine.create(&system_inv(), doc).await.unwrap().value.id();
    Site {
        engine,
        authorizer,
        app,
        ids: people.iter().map(|(k, (id, _))| (*k, *id)).collect(),
        id,
    }
}

fn invocation(actor: Actor) -> Invocation {
    Invocation {
        site: SiteId::new("app-permissions.test").unwrap(),
        actor,
        command_id: CommandId::new("cmd-app-permissions").ok(),
        language: None,
        now: now(),
    }
}

fn system_inv() -> Invocation {
    invocation(Actor::System {
        component: n("app_permission_tests"),
    })
}

impl Site {
    fn inv(&self, who: &str) -> Invocation {
        invocation(Actor::User {
            id: self.ids[who],
            via: None,
        })
    }

    async fn call(&self, who: &str) -> Result<Json, AppError> {
        self.engine
            .call_method(&self.inv(who), "note", self.id, "approve", json!({}), None)
            .await
            .map(|o| o.value.1)
    }

    async fn call_as_system(&self) -> Result<Json, AppError> {
        self.engine
            .call_method(&system_inv(), "note", self.id, "approve", json!({}), None)
            .await
            .map(|o| o.value.1)
    }

    async fn version(&self) -> Option<spot_app_api::prelude::Version> {
        self.engine
            .peek("note", self.id)
            .await
            .unwrap()
            .unwrap()
            .version()
    }

    async fn outbox_len(&self) -> usize {
        self.engine
            .backend()
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap()
            .len()
    }
}

#[tokio::test]
async fn the_grant_decides_and_a_swap_that_revokes_it_takes_effect_on_the_next_call() {
    let ran = Arc::new(AtomicUsize::new(0));
    let s = site(ran.clone(), true, true).await;

    // The holder's call runs.
    s.call("ann").await.unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // A user without the grant is refused: body not run, nothing written, nothing emitted.
    let (version, outbox) = (s.version().await, s.outbox_len().await);
    let error = s.call("bob").await.unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the body ran");
    assert_eq!(s.version().await, version, "a denied call wrote");
    assert_eq!(s.outbox_len().await, outbox, "a denied call emitted");

    // A swap that revokes the grant: the very next call is refused, with no restart.
    s.authorizer.swap(permission_engine(true, false));
    let error = s.call("ann").await.unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // And a swap that grants it again reopens the method.
    s.authorizer.swap(permission_engine(true, true));
    s.call("ann").await.unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_undeclared_permission_is_refused_for_everyone_but_the_system() {
    let ran = Arc::new(AtomicUsize::new(0));
    // The role is granted the permission, but no installed app declares it: an orphan grant.
    let s = site(ran.clone(), false, true).await;
    for who in ["ann", "bob"] {
        let error = s.call(who).await.unwrap_err();
        assert_eq!(error.code, "permission.denied", "{who}");
    }
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    // The platform acting as itself is not held back, and is not a denial.
    s.call_as_system().await.unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_log_tells_the_reasons_apart_and_the_caller_cannot() {
    let ran = Arc::new(AtomicUsize::new(0));
    let s = site(ran, true, true).await;

    let not_granted = s.call("bob").await.unwrap_err();
    s.authorizer.swap(permission_engine(false, true));
    let unknown = s.call("ann").await.unwrap_err();

    let log = s.app.denial_log();
    let reasons: Vec<&str> = log.iter().map(|d| d.reason).collect();
    assert_eq!(reasons, ["perm.app.not_granted", "perm.app.unknown"]);
    assert_eq!(log[0].permission, APPROVE);
    assert_eq!(log[0].user, "bob");
    assert_eq!(log[1].user, "ann");

    // What the caller sees is identical: no reason, no role, no holder.
    assert_eq!(not_granted.code, unknown.code);
    assert_eq!(not_granted.class, unknown.class);
    assert_eq!(not_granted.message, unknown.message);
    assert_eq!(not_granted.details, unknown.details);
    let seen = format!("{not_granted:?}{unknown:?}");
    assert!(!seen.contains(ROLE_SECRET), "{seen}");

    // The log holds no role name either.
    assert!(!format!("{log:?}").contains(ROLE_SECRET));
    // The engine's own denial log recorded both refusals.
    assert_eq!(s.engine.denial_count(), 2);
}

#[tokio::test]
async fn two_sites_over_one_controller_registry_do_not_share_grants() {
    let ran = Arc::new(AtomicUsize::new(0));
    let a = site(ran.clone(), true, true).await;
    // Site B: the same role and user, the permission neither declared nor granted.
    let b = site(ran.clone(), false, false).await;

    a.call("ann").await.unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    let error = b.call("ann").await.unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(b.app.denial_log()[0].reason, "perm.app.unknown");
    assert!(
        a.app.denial_log().is_empty(),
        "site B's denial is not site A's"
    );

    // Revoking in A does not touch B, and granting in B does not open A.
    b.authorizer.swap(permission_engine(true, true));
    b.call("ann").await.unwrap();
    a.authorizer.swap(permission_engine(true, false));
    assert!(a.call("ann").await.is_err());
    b.call("ann").await.unwrap();
}

#[tokio::test]
async fn reading_the_document_is_still_required_before_the_permission_is_even_asked() {
    let ran = Arc::new(AtomicUsize::new(0));
    let s = site(ran.clone(), true, true).await;
    // Can read the document, lacks the permission: permission.denied.
    assert_eq!(s.call("bob").await.unwrap_err().code, "permission.denied");
    // Cannot read the document: the answer a missing method gets.
    assert_eq!(s.call("blind").await.unwrap_err().code, "method.not_found");
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    // The blind user's attempt is not in the adapter's log (it was never asked), bob's is.
    let users: Vec<String> = s.app.denial_log().into_iter().map(|d| d.user).collect();
    assert_eq!(users, ["bob"]);
}

#[tokio::test]
async fn the_denial_log_is_bounded_and_oldest_first() {
    let ran = Arc::new(AtomicUsize::new(0));
    let s = site(ran, true, true).await;
    for _ in 0..(DENIAL_LOG_CAPACITY + 44) {
        s.call("bob").await.unwrap_err();
    }
    assert_eq!(s.app.denial_log().len(), DENIAL_LOG_CAPACITY);
    // The log is of the adapter's own decisions: a long name is cut, not stored whole.
    let long = "a".repeat(500);
    let bob = Principal::with_roles(&long, &["reader"]);
    use spot_document::engine::AppPermissionCheck;
    assert!(!s.app.holds(&bob, APPROVE).await);
    let last = s.app.denial_log().pop().unwrap();
    assert_eq!(last.user.len(), 128);
}
