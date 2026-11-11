//! The world the engine-level L7 suite runs in (WP-TST-07.T2).
//!
//! It is built entirely from the sample app's own artefacts (WP-DX-05.T1): the six DocType
//! definitions, the three-role permission model and the `confidential` field group. Nothing here
//! invents a DocType or a right; the harness only composes what the app ships, plus the two
//! platform fixtures (`user`, `file`) its links name and the one whitelisted method the engine
//! needs to exercise (`l7:touch`).
//!
//! # Principals
//!
//! Six principals, one per [`PrincipalSet`] entry, cover the whole permission surface: the
//! `Project Manager` (everything on the app DocTypes), the `Team Member` (read all, create tasks
//! and time entries), the `Viewer` (read only), `ghost` (no role at all), the platform
//! `Tenant Administrator`, and the platform `system`. The engine resolves each through
//! [`MemoryPrincipals`], exactly as a host resolves a real user.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::context::{Actor, Invocation, MethodContext};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId};
use spot_app_api::{AppError, AppResult, BoxFuture, Controller, DocumentPatch, ErrorClass};
use spot_core::ManualClock;
use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_meta::{Definition, EffectiveMeta, Registry};
use spot_naming::{Namer, StaticPlans};
use spot_permissions::model::{FieldFacts, GroupName, ModelTexts};
use spot_permissions::{Model, PermissionEngine, Principal, Snapshot, TENANT_ADMINISTRATOR, load};
use spot_plan::NamingPlan;
use spot_seams::RegistryBuilder;
use spot_storage_contract::StorageBackend;

/// The tenant every invocation acts in.
pub const SITE: &str = "l7-engine";

/// The sample app's DocTypes, by storage name.
pub const PROJECT: &str = "project";
/// The Task DocType.
pub const TASK: &str = "task";
/// The Time Entry DocType (the app's submittable document).
pub const TIME_ENTRY: &str = "time_entry";
/// The Label DocType (the naming workload's marker documents).
pub const LABEL: &str = "label";
/// The Checklist Item child DocType.
pub const CHECKLIST_ITEM: &str = "checklist_item";
/// The Label Pick child DocType.
pub const LABEL_PICK: &str = "label_pick";
/// The platform User fixture.
pub const USER: &str = "user";

/// The two series the naming workload drives, and the DocType each names.
pub const SERIES_TIME_ENTRY: &str = "TE-.YYYY.-.#####";
/// The project naming pattern (the brief's `PRJ-.YYYY.-.####`).
pub const SERIES_PROJECT: &str = "PRJ-.YYYY.-.####";

/// The step name of the harness's whitelisted method (`l7:touch`).
pub const TOUCH_STEP: &str = "l7:touch";
/// The app name the harness registers its controller under.
pub const HARNESS_APP: &str = "l7";

/// The definitions of the sample app plus the two platform fixtures its links name. The TOML is
/// ours (ADR-03/ADR-32): the same text the app's own detached validation suite uses.
pub fn definitions() -> Vec<Definition> {
    let mut defs: Vec<Definition> = spot_sample_app::DEFINITION_TEXTS
        .iter()
        .map(|(name, text)| {
            Definition::parse(name, text)
                .unwrap_or_else(|e| panic!("the sample DocType {name} parses: {e:?}"))
        })
        .collect();
    defs.push(
        Definition::parse(
            "user",
            r#"
name = "user"
label = "User"
app = "spot_platform"
module = "directory"
title_field = "user_name"

[[fields]]
name = "user_name"
label = "User Name"
type = "text"
max_chars = 140
required = true
unique = true

[[fields]]
name = "enabled"
label = "Enabled"
type = "bool"
default = "true"
"#,
        )
        .unwrap_or_else(|e| panic!("the user fixture parses: {e:?}")),
    );
    defs.push(
        Definition::parse(
            "file",
            r#"
name = "file"
label = "File"
app = "spot_platform"
module = "files"
title_field = "name"

[[fields]]
name = "name"
label = "Name"
type = "text"
max_chars = 140
required = true
"#,
        )
        .unwrap_or_else(|e| panic!("the file fixture parses: {e:?}")),
    );
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    defs
}

/// The app's effective metadata over its definitions and the two fixtures.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions())
        .unwrap_or_else(|e| panic!("the sample definitions build: {e:?}"))
        .effective()
}

/// The sample app's permission model, loaded from the app's own embedded texts.
pub fn permission_model() -> Model {
    load(&ModelTexts {
        grants: spot_sample_app::permissions::PERMISSION_TEXTS
            .grants
            .to_owned(),
        field_grants: spot_sample_app::permissions::PERMISSION_TEXTS
            .field_grants
            .to_owned(),
        groups: spot_sample_app::permissions::PERMISSION_TEXTS
            .groups
            .to_owned(),
        shares: spot_sample_app::permissions::PERMISSION_TEXTS
            .shares
            .to_owned(),
    })
    .unwrap_or_else(|e| panic!("the sample permission model loads: {e:?}"))
}

/// The `confidential` field facts the metadata-side group declaration will stamp once
/// WP-MET-01.T6 lands: `task.details` is the one protected field of the sample app.
fn field_facts() -> Vec<(&'static str, Vec<FieldFacts>)> {
    vec![(
        TASK,
        vec![FieldFacts {
            path: "details".to_owned(),
            groups: vec![GroupName("confidential".to_owned())],
            tags: vec![],
            mask: None,
        }],
    )]
}

/// The permission decision core over the app's metadata, model and field facts.
pub fn decision_engine(meta: &Arc<EffectiveMeta>) -> Arc<PermissionEngine> {
    let model = permission_model();
    let mut snapshot = Snapshot::build(meta, &model)
        .unwrap_or_else(|e| panic!("the sample grants fit the metadata: {e:?}"));
    for (doctype, facts) in field_facts() {
        snapshot = snapshot
            .with_field_facts(doctype, facts)
            .unwrap_or_else(|e| panic!("the confidential facts fit: {e:?}"));
    }
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// The six principals and their actor record ids.
pub struct PrincipalSet {
    principals: BTreeMap<String, Principal>,
    ids: BTreeMap<String, RecordId>,
    users: MemoryPrincipals,
}

impl PrincipalSet {
    /// The six principals.
    pub fn new() -> PrincipalSet {
        let mut principals = BTreeMap::new();
        principals.insert(
            "manager".to_owned(),
            Principal::with_roles("manager", &["Project Manager"]),
        );
        principals.insert(
            "member".to_owned(),
            Principal::with_roles("member", &["Team Member"]),
        );
        principals.insert(
            "viewer".to_owned(),
            Principal::with_roles("viewer", &["Viewer"]),
        );
        principals.insert("ghost".to_owned(), Principal::user("ghost", Vec::new()));
        principals.insert(
            "admin".to_owned(),
            Principal::with_roles("admin", &[TENANT_ADMINISTRATOR]),
        );
        principals.insert("system".to_owned(), Principal::system());

        let mut ids = BTreeMap::new();
        let mut users = MemoryPrincipals::new();
        for (name, principal) in &principals {
            let id = RecordId::new_v7();
            ids.insert(name.clone(), id);
            if !principal.is_system {
                users = users.with(&id, principal.clone());
            }
        }
        PrincipalSet {
            principals,
            ids,
            users,
        }
    }

    /// The six principal names, in a stable order.
    pub fn names(&self) -> [&'static str; 6] {
        ["manager", "member", "viewer", "ghost", "admin", "system"]
    }

    /// The principal named `name`.
    pub fn principal(&self, name: &str) -> &Principal {
        self.principals
            .get(name)
            .unwrap_or_else(|| panic!("no principal named `{name}`"))
    }

    /// The actor record id of a user principal.
    pub fn id(&self, name: &str) -> RecordId {
        *self
            .ids
            .get(name)
            .unwrap_or_else(|| panic!("no principal named `{name}`"))
    }

    /// The [`PrincipalSource`](spot_document::engine::PrincipalSource) the authorizer resolves
    /// actors through.
    pub fn source(&self) -> MemoryPrincipals {
        self.users.clone()
    }
}

impl Default for PrincipalSet {
    fn default() -> Self {
        PrincipalSet::new()
    }
}

/// Everything an engine-level run needs, built once.
pub struct World {
    /// The effective metadata.
    pub meta: Arc<EffectiveMeta>,
    /// The compiled storage catalog (the checks read it).
    pub catalog: Arc<spot_storage_contract::Catalog>,
    /// The decision core.
    pub decision: Arc<PermissionEngine>,
    /// The six principals.
    pub principals: PrincipalSet,
    /// The fixed instant every invocation uses.
    pub now: OffsetDateTime,
    /// The naming engine over the two series the workload drives.
    pub namer: Namer,
}

impl World {
    /// Builds the world.
    pub fn new() -> World {
        let meta = meta();
        let catalog = meta
            .catalog(
                spot_storage_contract::Collation::new("und")
                    .unwrap_or_else(|e| panic!("collation: {e:?}")),
            )
            .map(Arc::new)
            .unwrap_or_else(|e| panic!("the sample catalog compiles: {e:?}"));
        let plans = StaticPlans::new()
            .with(
                TIME_ENTRY,
                NamingPlan::parse(SERIES_TIME_ENTRY).unwrap_or_else(|e| panic!("plan: {e:?}")),
            )
            .with(
                PROJECT,
                NamingPlan::parse(SERIES_PROJECT).unwrap_or_else(|e| panic!("plan: {e:?}")),
            );
        let decision = decision_engine(&meta);
        World {
            meta,
            catalog,
            decision,
            principals: PrincipalSet::new(),
            now: OffsetDateTime::from_unix_timestamp(1_790_000_000)
                .unwrap_or_else(|e| panic!("a fixed instant: {e:?}")),
            namer: Namer::new(Arc::new(plans)),
        }
    }

    /// An invocation as the named principal.
    pub fn inv(&self, name: &str) -> Invocation {
        let principal = self.principals.principal(name);
        let actor = if principal.is_system {
            Actor::System {
                component: Name::new("l7_engine").unwrap_or_else(|e| panic!("a name: {e:?}")),
            }
        } else {
            Actor::User {
                id: self.principals.id(name),
                via: None,
            }
        };
        Invocation {
            site: SiteId::new(SITE).unwrap_or_else(|e| panic!("a site: {e:?}")),
            actor,
            command_id: CommandId::new("l7-cmd").ok(),
            language: None,
            now: self.now,
        }
    }

    /// An engine over `backend`, enforcing the app's permissions and dispatching `l7:touch`.
    pub fn engine<B: StorageBackend>(&self, backend: B) -> AppResult<Engine<B>> {
        let seams = RegistryBuilder::new()
            .build()
            .map_err(|e| AppError::new(ErrorClass::Internal, "l7_seams", format!("{e:?}")))?;
        let authorizer = Arc::new(PermissionAuthorizer::new(
            self.decision.clone(),
            Arc::new(self.principals.source()),
        ));
        Ok(Engine::new(
            backend,
            self.meta.clone(),
            Arc::new(seams),
            Arc::new(ManualClock::new(self.now)),
            Arc::new(V7Ids),
        )?
        .with_authorizer(authorizer)
        .with_controllers(vec![Arc::new(TouchController)]))
    }
}

impl Default for World {
    fn default() -> Self {
        World::new()
    }
}

/// The harness's one whitelisted method: `l7:touch` on a Task. It bumps the task's `subject`
/// with a `!` suffix through the method context's repository, so a method call is a real write
/// that runs the update lifecycle inside the method's transaction.
#[derive(Debug, Clone, Copy, Default)]
pub struct TouchController;

impl Controller for TouchController {
    fn doctype(&self) -> &str {
        TASK
    }

    fn steps(&self) -> Vec<spot_app_api::StepDecl> {
        Vec::new()
    }

    fn methods(&self) -> Vec<spot_app_api::MethodDecl> {
        vec![spot_app_api::MethodDecl::new(
            Name::new("touch").unwrap_or_else(|e| panic!("a method name: {e:?}")),
            true,
        )]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a spot_app_api::StepName,
        _ctx: &'a mut dyn spot_app_api::HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        let step = step.clone();
        Box::pin(async move {
            Err(AppError::new(
                ErrorClass::Internal,
                "l7_unknown_step",
                format!("the harness controller holds no step {step}"),
            ))
        })
    }

    fn call_method<'a>(
        &'a self,
        method: &'a str,
        _args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            if method != "touch" {
                return Err(AppError::new(
                    ErrorClass::MethodNotFound,
                    "method.not_found",
                    format!("no method {method}"),
                ));
            }
            let doc = ctx.doc();
            let subject = doc.text("subject")?.unwrap_or_default().to_owned();
            let version = doc.version().ok_or_else(|| {
                AppError::new(
                    ErrorClass::Internal,
                    "l7_touch_no_version",
                    "the task has no version",
                )
            })?;
            let mut patch = DocumentPatch::default();
            patch.fields.insert(
                Name::new("subject").unwrap_or_else(|e| panic!("a field name: {e:?}")),
                spot_storage_contract::Value::Text(format!("{subject}!")),
            );
            let updated = ctx.update(TASK, doc.id(), patch, version).await?;
            let after = updated.text("subject")?.unwrap_or_default().to_owned();
            Ok(Json::String(after))
        })
    }
}
