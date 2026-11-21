//! A whitelisted method runs only for a principal who holds the right it declares
//! (WP-DOC-11.T3): the full matrix through the real engine on the fake backend.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "authz/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value as Json, json};
use spot_app_api::context::{DocumentService, HookContext, MethodContext};
use spot_app_api::controller::{Controller, MethodDecl, MethodRight, StepDecl, StepName};
use spot_app_api::prelude::{RecordId, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, Document, ErrorClass};
use spot_core::ManualClock;
use spot_document::engine::{
    AppPermissionCheck, Authorizer, Engine, MemoryAppPermissions, MemoryPrincipals,
    PermissionAuthorizer, V7Ids,
};
use spot_permissions::{ModelTexts, PermissionEngine, Principal, Snapshot, load};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;
use world::{World, n};

const APPROVE: &str = "approve_note";

/// The rights in the matrix, in the order "neighbouring" means.
fn rights() -> Vec<MethodRight> {
    vec![
        MethodRight::Read,
        MethodRight::Write,
        MethodRight::Create,
        MethodRight::Submit,
        MethodRight::Cancel,
        MethodRight::Permission(n(APPROVE)),
    ]
}

fn method_name(right: &MethodRight) -> String {
    match right {
        MethodRight::Permission(_) => "m_permission".to_owned(),
        other => format!("m_{}", other.as_str()),
    }
}

/// The role (and the engine right) that holds `right`; the permission right has no role.
fn grant_of(right: &MethodRight) -> Option<&'static str> {
    match right {
        MethodRight::Read => Some("read"),
        MethodRight::Write => Some("write"),
        MethodRight::Create => Some("create"),
        MethodRight::Submit => Some("submit"),
        MethodRight::Cancel => Some("cancel"),
        _ => None,
    }
}

/// A model where each `has_<right>` role holds `read` plus that one right on `note`, and
/// `has_read` holds `read` only.
fn model() -> ModelTexts {
    let mut grants = String::new();
    for right in ["read", "write", "create", "submit", "cancel"] {
        let rights = if right == "read" {
            "[\"read\"]".to_owned()
        } else {
            format!("[\"read\", \"{right}\"]")
        };
        grants.push_str(&format!(
            "[[entries]]\ndoctype = \"note\"\nrole = \"has_{right}\"\nrights = {rights}\n\n"
        ));
    }
    // A role with a right but no `read`.
    grants.push_str(
        "[[entries]]\ndoctype = \"note\"\nrole = \"write_no_read\"\nrights = [\"write\"]\n",
    );
    ModelTexts {
        grants,
        field_grants: String::new(),
        groups: String::new(),
        shares: String::new(),
    }
}

fn permission_engine() -> Arc<PermissionEngine> {
    let model = load(&model()).expect("the model loads");
    let snapshot = Snapshot::build(&world::meta(), &model).expect("the snapshot builds");
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// Principals: `holds_<right>`, `near_<right>` (read plus the next right in the list), `none`
/// (read only) and `blind` (no read at all, a write grant only).
fn principals() -> BTreeMap<String, Principal> {
    let all = rights();
    let mut out = BTreeMap::new();
    for (i, right) in all.iter().enumerate() {
        let name = method_name(right);
        let near = &all[(i + 1) % all.len()];
        let role = |r: &MethodRight| {
            grant_of(r).map_or("has_read", |g| {
                Box::leak(format!("has_{g}").into_boxed_str())
            })
        };
        out.insert(
            format!("holds{name}"),
            Principal::with_roles(&format!("holds{name}"), &[role(right)]),
        );
        out.insert(
            format!("near{name}"),
            Principal::with_roles(&format!("near{name}"), &[role(near)]),
        );
    }
    out.insert(
        "none".to_owned(),
        Principal::with_roles("none", &["has_read"]),
    );
    out.insert(
        "blind".to_owned(),
        Principal::with_roles("blind", &["write_no_read"]),
    );
    out.insert("nobody".to_owned(), Principal::user("nobody", Vec::new()));
    out
}

type Ran = Arc<AtomicUsize>;

/// One method per right; every body counts that it ran.
#[derive(Clone)]
struct Sample {
    methods: Vec<MethodDecl>,
    ran: Ran,
}

impl Sample {
    fn new(ran: Ran) -> Self {
        let methods = rights()
            .into_iter()
            .map(|right| {
                let mutates = right.implies_mutates();
                MethodDecl::new(n(&method_name(&right)), mutates).with_right(right)
            })
            .collect();
        Self { methods, ran }
    }
}

impl Controller for Sample {
    fn doctype(&self) -> &str {
        "note"
    }

    fn steps(&self) -> Vec<StepDecl> {
        Vec::new()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        self.methods.clone()
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

struct Fixture {
    world: World,
    engine: Engine<FakeBackend>,
    ran: Ran,
    id: RecordId,
}

async fn fixture_with(
    methods: Option<Vec<MethodDecl>>,
    app: Option<Arc<dyn AppPermissionCheck>>,
) -> Fixture {
    let mut world = World::new();
    world.principals = principals();
    world.ids = world
        .principals
        .keys()
        .map(|name| (name.clone(), RecordId::new_v7()))
        .collect();
    world.engine = permission_engine();
    let mut memory = MemoryPrincipals::new();
    for (name, principal) in &world.principals {
        memory = memory.with(&world.id(name), principal.clone());
    }
    let authorizer: Arc<dyn Authorizer> = Arc::new(PermissionAuthorizer::new(
        world.engine.clone(),
        Arc::new(memory),
    ));
    let ran: Ran = Arc::default();
    let mut sample = Sample::new(ran.clone());
    if let Some(methods) = methods {
        sample.methods = methods;
    }
    let mut builder = RegistryBuilder::new();
    builder.add_controller("sample", Arc::new(sample.clone()));
    let mut engine = Engine::new(
        FakeBackend::new(),
        world::meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(world::now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_authorizer(authorizer)
    .with_controllers(vec![Arc::new(sample)]);
    if let Some(app) = app {
        engine = engine.with_app_permissions(app);
    }
    engine.prepare().await.unwrap();
    let mut doc = Document::new(n("note"), RecordId::new_v7());
    doc.set(n("text"), Value::Text("hello".into()));
    let id = engine
        .create(&world.system_inv(), doc)
        .await
        .unwrap()
        .value
        .id();
    Fixture {
        world,
        engine,
        ran,
        id,
    }
}

/// Approve is granted to `holds_m_permission` and to nobody else.
fn app_permissions(world_ids: &[&str]) -> Arc<dyn AppPermissionCheck> {
    let mut grants = MemoryAppPermissions::new();
    for user in world_ids {
        grants = grants.grant(user, APPROVE);
    }
    Arc::new(grants)
}

async fn fixture() -> Fixture {
    fixture_with(None, Some(app_permissions(&["holdsm_permission"]))).await
}

impl Fixture {
    async fn call(&self, who: &str, method: &str) -> Result<Json, AppError> {
        self.engine
            .call_method(
                &self.world.inv(who),
                "note",
                self.id,
                method,
                json!({}),
                None,
            )
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
}

#[tokio::test]
async fn the_matrix_holds_neighbour_and_none_for_every_right() {
    let f = fixture().await;
    for right in rights() {
        let method = method_name(&right);
        let is_read = right == MethodRight::Read;

        // Holds it: runs.
        let before = f.ran.load(Ordering::SeqCst);
        let out = f.call(&format!("holds{method}"), &method).await;
        assert!(out.is_ok(), "{right}: the holder is refused: {out:?}");
        assert_eq!(
            f.ran.load(Ordering::SeqCst),
            before + 1,
            "{right}: body ran"
        );

        // Holds only the neighbouring right (and `read`): denied, except that for `read` the
        // neighbour's `read` is exactly what the method needs.
        let before = f.ran.load(Ordering::SeqCst);
        let (version, denials) = (f.version().await, f.engine.denial_count());
        let out = f.call(&format!("near{method}"), &method).await;
        if is_read {
            assert!(out.is_ok(), "read: any reader may call it");
            assert_eq!(f.ran.load(Ordering::SeqCst), before + 1);
        } else {
            let error = out.expect_err(&format!("{right}: the neighbour is allowed"));
            assert_eq!(error.code, "permission.denied", "{right}");
            assert_eq!(f.ran.load(Ordering::SeqCst), before, "{right}: body ran");
            assert_eq!(f.version().await, version, "{right}: a denied call writes");
            assert_eq!(f.engine.denial_count(), denials + 1, "{right}: not audited");
        }

        // Holds none (read only, or nothing at all): `read` alone calls only a read method;
        // a principal without `read` learns nothing, not even that the method exists.
        let before = f.ran.load(Ordering::SeqCst);
        let out = f.call("none", &method).await;
        if is_read {
            assert!(out.is_ok());
        } else {
            let error = out.expect_err(&format!("{right}: `none` is allowed"));
            assert_eq!(error.code, "permission.denied", "{right}");
            assert_eq!(f.ran.load(Ordering::SeqCst), before);
        }
        let error = f.call("nobody", &method).await.expect_err("no role");
        assert_eq!(error.code, "method.not_found", "{right}");
    }
}

#[tokio::test]
async fn a_denial_names_the_right_and_a_missing_read_reveals_nothing() {
    let f = fixture().await;
    let denied = f.call("none", "m_submit").await.unwrap_err();
    assert_eq!(denied.code, "permission.denied");
    assert_eq!(denied.details.get("action"), Some(&json!("post")));

    // Without `read` the answer is the one a missing method gets.
    let blind = f.call("blind", "m_write").await.unwrap_err();
    let missing = f.call("none", "no_such_method").await.unwrap_err();
    assert_eq!(blind.code, "method.not_found");
    assert!(
        !f.engine.masked_denial_log().is_empty(),
        "the masked refusal is audited"
    );
    assert_eq!(missing.code, "method.not_found");
    assert_eq!(blind.class, ErrorClass::MethodNotFound);
    assert_eq!(blind.class, missing.class);
    // A document that does not exist looks the same to the caller.
    let gone = f
        .engine
        .call_method(
            &f.world.inv("blind"),
            "note",
            RecordId::new_v7(),
            "m_write",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(gone.code, "method.not_found");
    assert!(blind.details.is_empty() && gone.details.is_empty());
}

#[tokio::test]
async fn submit_by_a_principal_with_update_but_not_post_is_denied() {
    let f = fixture().await;
    let before = f.version().await;
    let error = f.call("holdsm_write", "m_submit").await.unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert_eq!(f.ran.load(Ordering::SeqCst), 0);
    assert_eq!(f.version().await, before);
}

#[tokio::test]
async fn an_app_permission_denies_when_unknown_and_allows_when_granted() {
    // The default engine holds no app permission at all: everyone is refused.
    let f = fixture_with(None, None).await;
    let error = f
        .call("holdsm_permission", "m_permission")
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert_eq!(f.ran.load(Ordering::SeqCst), 0);

    // A check that knows other permissions only: this one is unknown, so refused.
    let other = Arc::new(MemoryAppPermissions::new().grant("holdsm_permission", "other"));
    let f = fixture_with(None, Some(other)).await;
    let error = f
        .call("holdsm_permission", "m_permission")
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");

    // Granted: allowed, for that user only, and only with `read` on the document.
    let f = fixture().await;
    f.call("holdsm_permission", "m_permission").await.unwrap();
    let error = f.call("none", "m_permission").await.unwrap_err();
    assert_eq!(error.code, "permission.denied");
    let error = f.call("nobody", "m_permission").await.unwrap_err();
    assert_eq!(error.code, "method.not_found");
    // The platform is not held back.
    f.engine
        .call_method(
            &f.world.system_inv(),
            "note",
            f.id,
            "m_permission",
            json!({}),
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unevaluable_declaration_is_refused_never_run_with_a_weaker_right() {
    // `read` on a method that changes documents contradicts itself: the call is refused for a
    // reader instead of running with only `read`.
    let bad = vec![MethodDecl::new(n("m_bad"), true).with_right(MethodRight::Read)];
    let f = fixture_with(Some(bad), None).await;
    let error = f.call("none", "m_bad").await.unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert_eq!(f.ran.load(Ordering::SeqCst), 0);
}
