//! [`StackHost`]: the L10 [`LivenessHost`] over the customise service, the document engine and a
//! real PostgreSQL.
//!
//! The suite's trait is synchronous, the stack is not: the host owns a runtime and blocks on each
//! call. Run the suite from a plain `#[test]`, never from inside an async test.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_app_api::prelude::SiteId;
use spot_core::ManualClock;
use spot_customise::pg::PgSchema;
use spot_customise::{
    Caller, CustomiseError, CustomiseService, ListenerFuture, MemoryHost, MemoryRecordStore, Parts,
    SchemaBackend, SwapListener,
};
use spot_document::engine::{Engine, V7Ids};
use spot_l10_liveness::change::{
    AddDocTypeDef, Change, ChangeEffect, DocTypeDefInput, FieldSpecInput, HostError, Refusal,
};
use spot_l10_liveness::{HostFactory, LivenessHost, Probe, ProbeOutcome, definition_of};
use spot_meta::{BuildOptions, Customisations, Definition, EffectiveMeta};
use spot_meta_live::{LiveMeta, MemoryVersionStore, PollingBus, Snapshot, Version};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{Collation, StorageBackend};
use tokio::runtime::Runtime;

use crate::probes::{PlanView, SchemaView, ViewView, plan_probe, schema_probe, view_probe};

/// Points the engine at each new snapshot and remembers the version it applied: the plan layer
/// the probe reads.
struct Listener {
    engine: Arc<Engine<PgBackend>>,
    applied: Arc<AtomicU64>,
}

impl SwapListener for Listener {
    fn swapped<'a>(&'a self, snapshot: &'a Arc<Snapshot>) -> ListenerFuture<'a> {
        Box::pin(async move {
            self.engine
                .set_meta(snapshot.meta().clone())
                .await
                .map_err(|e| format!("{} {:?} {:?}", e.code, e.details, e.explanation))?;
            self.applied
                .store(snapshot.meta().version(), Ordering::SeqCst);
            Ok(())
        })
    }
}

/// The two standard DocTypes every scenario starts from (the suite's own `party` and `invoice`).
fn standard_definitions() -> Vec<Definition> {
    let party = DocTypeDefInput::new(
        "party",
        vec![
            FieldSpecInput::text("code").with_required(true),
            FieldSpecInput::select("state", &["Active", "Inactive"]).with_default("Active"),
        ],
    )
    .with_label("Party")
    .with_title_field("code");
    let invoice = DocTypeDefInput::new(
        "invoice",
        vec![
            FieldSpecInput::text("number").with_required(true),
            FieldSpecInput::text("party_code"),
            FieldSpecInput::decimal("total", 2),
            FieldSpecInput::select("state", &["Draft", "Sent", "Paid"]).with_default("Draft"),
        ],
    )
    .with_label("Invoice")
    .posted()
    .with_title_field("number");
    vec![definition_of(&party), definition_of(&invoice)]
}

/// The TOML text of a runtime DocType, as `create_doctype` takes it.
pub fn toml_of(def: &AddDocTypeDef) -> String {
    let mut out = format!("name = {:?}\n", def.name);
    if let Some(l) = &def.label {
        out.push_str(&format!("label = {l:?}\n"));
    }
    if def.is_posted {
        out.push_str("is_posted = true\n");
    }
    if let Some(t) = &def.title_field {
        out.push_str(&format!("title_field = {t:?}\n"));
    }
    for f in &def.fields {
        out.push_str(&format!(
            "[[fields]]\nname = {:?}\ntype = {:?}\n",
            f.name, f.ty
        ));
        if let Some(l) = &f.label {
            out.push_str(&format!("label = {l:?}\n"));
        }
        if let Some(o) = &f.options {
            out.push_str(&format!("options = {:?}\n", o.join("\n")));
        }
        if f.required {
            out.push_str("required = true\n");
        }
        if f.read_only {
            out.push_str("read_only = true\n");
        }
        if f.allow_on_posted {
            out.push_str("allow_on_posted = true\n");
        }
        if let Some(d) = &f.default {
            out.push_str(&format!("default = {d:?}\n"));
        }
        if let Some(s) = f.scale {
            out.push_str(&format!("scale = {s}\n"));
        }
    }
    out
}

/// The suite's refusal for a service error. The wording is the service's own, so the mapping reads
/// it; scenarios assert the refusal and the unmoved version, and the specific variant only where
/// an ADR fixes it.
pub fn refusal_of_error(error: &CustomiseError, change: &Change) -> Refusal {
    let text = error.to_string().to_lowercase();
    let doctype = match change {
        Change::AddCustomField { doctype, .. }
        | Change::RemoveCustomField { doctype, .. }
        | Change::SetProperty { doctype, .. } => doctype.clone(),
        Change::AddDocType { definition } => definition.name.clone(),
        Change::ReplaceDefinitions { .. } => String::new(),
    };
    if text.contains("reserved") {
        Refusal::ReservedName
    } else if text.contains("referenced") {
        Refusal::StillReferenced
    } else if text.contains("duplicate") || text.contains("already") || text.contains("collid") {
        match change {
            Change::AddCustomField { spec, .. } => Refusal::FieldCollision {
                doctype,
                field: spec.name.clone(),
            },
            _ => Refusal::NameCollision { name: doctype },
        }
    } else if text.contains("unknown")
        || text.contains("no such")
        || text.contains("not found")
        || text.contains("there is no")
    {
        Refusal::UnknownName
    } else if text.contains("custom_") {
        Refusal::BadCustomFieldName
    } else if matches!(change, Change::SetProperty { .. }) {
        Refusal::BadOverride
    } else {
        Refusal::BadInput {
            why: format!("refused by the service: {text}"),
        }
    }
}

/// What the last change touched, for the probes.
struct Touched {
    doctype: String,
    field: Option<String>,
    present: bool,
}

/// The host over the stack.
pub struct StackHost {
    pub(crate) rt: Arc<Runtime>,
    pub(crate) db: Arc<TestDb>,
    pub(crate) service: Arc<CustomiseService>,
    pub(crate) live: Arc<LiveMeta>,
    schema: Arc<PgSchema>,
    pub(crate) applied: Arc<AtomicU64>,
    pub(crate) engine: Arc<Engine<PgBackend>>,
    pub(crate) shared: Shared,
    touched: Option<Touched>,
}

/// What every node on one database shares: the version store (which also keeps the definitions
/// and announces changes) and the customisation records. A follower built from it converges on
/// what any node applies.
#[derive(Clone)]
pub struct Shared {
    pub(crate) versions: Arc<MemoryVersionStore>,
    pub(crate) records: Arc<MemoryRecordStore>,
}

fn admin() -> Caller {
    Caller::new("l10-stack", &["System Manager"])
}

impl StackHost {
    /// Builds the stack on a scratch database, or `None` when PostgreSQL is not configured.
    ///
    /// # Panics
    ///
    /// When PostgreSQL is configured but the stack cannot be built: that is a failure, not a skip.
    pub fn boot() -> Option<StackHost> {
        let rt = Arc::new(Runtime::new().unwrap_or_else(|e| panic!("a runtime: {e}")));
        let built = rt.block_on(async { build(None).await });
        match built {
            Ok(None) => None,
            Ok(Some(w)) => Some(StackHost::from_wired(rt, w)),
            Err(e) => panic!("the L10 stack could not be built: {e}"),
        }
    }

    fn from_wired(rt: Arc<Runtime>, w: Wired) -> StackHost {
        StackHost {
            rt,
            db: w.db,
            service: w.service,
            live: w.live,
            schema: w.schema,
            applied: w.applied,
            engine: w.engine,
            shared: w.shared,
            touched: None,
        }
    }

    /// A second node on the same database, sharing the version store and the records: it starts
    /// from the base metadata and converges with [`StackHost::recover`].
    ///
    /// # Panics
    ///
    /// When the node cannot be built on a database that exists.
    pub fn follower(&self) -> StackHost {
        let existing = Some((self.db.clone(), self.shared.clone()));
        match self.rt.block_on(async { build(existing).await }) {
            Ok(Some(w)) => StackHost::from_wired(self.rt.clone(), w),
            Ok(None) => panic!("the database of the first node is gone"),
            Err(e) => panic!("the follower could not be built: {e}"),
        }
    }

    fn catalog(&self) -> Option<spot_storage_contract::Catalog> {
        let collation = Collation::new("und").ok()?;
        self.live.current().meta().catalog(collation).ok()
    }
}

/// One built node.
struct Wired {
    db: Arc<TestDb>,
    service: Arc<CustomiseService>,
    live: Arc<LiveMeta>,
    schema: Arc<PgSchema>,
    applied: Arc<AtomicU64>,
    engine: Arc<Engine<PgBackend>>,
    shared: Shared,
}

/// Builds a node. With `existing`, on that database and sharing that state (a follower); without,
/// on a fresh scratch database (`None` result when PostgreSQL is not configured).
async fn build(existing: Option<(Arc<TestDb>, Shared)>) -> Result<Option<Wired>, String> {
    let first = existing.is_none();
    let (db, shared) = match existing {
        Some(pair) => pair,
        None => {
            let Some(db) = TestDb::create().await else {
                return Ok(None);
            };
            (
                Arc::new(db),
                Shared {
                    versions: Arc::new(MemoryVersionStore::new()),
                    records: Arc::new(MemoryRecordStore::new()),
                },
            )
        }
    };
    let base = standard_definitions();
    let site = SiteId::new("l10-stack").map_err(|e| format!("{e:?}"))?;
    let snapshot = Snapshot::build(
        base.clone(),
        &Customisations::new(),
        &BuildOptions::new(),
        Version::ZERO,
    )
    .map_err(|e| format!("{e:?}"))?;
    let catalog = snapshot
        .meta()
        .catalog(Collation::new("und").map_err(|e| e.to_string())?)
        .map_err(|e| format!("{e:?}"))?;
    let schema_backend = Arc::new(
        PgBackend::connect(db.config())
            .await
            .map_err(|e| e.to_string())?,
    );
    if first {
        schema_backend
            .prepare(&catalog)
            .await
            .map_err(|e| e.to_string())?;
    }
    let engine_backend = PgBackend::connect(db.config())
        .await
        .map_err(|e| e.to_string())?;
    let clock = Arc::new(ManualClock::new(time_now()));
    let live = Arc::new(LiveMeta::new(site.clone(), base.clone()).map_err(|e| format!("{e:?}"))?);
    let engine = Arc::new(
        Engine::new(
            engine_backend,
            live.current().meta().clone(),
            Arc::new(
                RegistryBuilder::new()
                    .build()
                    .map_err(|e| format!("{e:?}"))?,
            ),
            clock,
            Arc::new(V7Ids),
        )
        .map_err(|e| e.code.clone())?,
    );
    let bus = Arc::new(PollingBus::new(shared.versions.clone()));
    let schema = Arc::new(PgSchema::new(schema_backend));
    let applied = Arc::new(AtomicU64::new(live.current().meta().version()));
    let service = Arc::new(
        CustomiseService::new(Parts {
            site,
            base,
            options: BuildOptions::new(),
            live: live.clone(),
            bus,
            versions: shared.versions.clone(),
            definitions: shared.versions.clone(),
            records: shared.records.clone(),
            schema: schema.clone(),
        })
        .with_host(Arc::new(MemoryHost::new()))
        .with_listener(Arc::new(Listener {
            engine: engine.clone(),
            applied: applied.clone(),
        })),
    );
    Ok(Some(Wired {
        db,
        service,
        live,
        schema,
        applied,
        engine,
        shared,
    }))
}

fn time_now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_790_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

impl LivenessHost for StackHost {
    fn apply(&mut self, change: &Change) -> Result<ChangeEffect, HostError> {
        let who = admin();
        let service = self.service.clone();
        let outcome = self.rt.block_on(async {
            match change {
                Change::AddCustomField {
                    doctype,
                    spec,
                    insert_after,
                } => {
                    service
                        .add_custom_field(
                            &who,
                            doctype,
                            &spec.clone().into_meta_spec(),
                            insert_after.as_deref(),
                        )
                        .await
                }
                Change::RemoveCustomField { doctype, name } => {
                    service.remove_custom_field(&who, doctype, name).await
                }
                Change::SetProperty {
                    doctype,
                    field,
                    property,
                } => match property {
                    spot_l10_liveness::change::SetProperty::Default(None) => {
                        service
                            .clear_property(&who, doctype, field.as_deref(), property.key())
                            .await
                    }
                    other => {
                        service
                            .set_property(
                                &who,
                                doctype,
                                field.as_deref(),
                                other.key(),
                                &other.value_text(),
                            )
                            .await
                    }
                },
                Change::AddDocType { definition } => {
                    service.create_doctype(&who, &toml_of(definition)).await
                }
                // The service's base definitions are fixed when it is built: an app update is a
                // restart-shaped event of the host, not an operation of this service.
                Change::ReplaceDefinitions { .. } => Err(CustomiseError::Schema {
                    message: "an app update is not an operation of the customise service"
                        .to_owned(),
                }),
            }
        });
        match outcome {
            Ok(applied) => {
                self.touched = match change {
                    Change::AddCustomField { doctype, spec, .. } => Some(Touched {
                        doctype: doctype.clone(),
                        field: Some(spec.name.clone()),
                        present: true,
                    }),
                    Change::RemoveCustomField { doctype, name } => Some(Touched {
                        doctype: doctype.clone(),
                        field: Some(name.clone()),
                        present: false,
                    }),
                    _ => None,
                };
                Ok(ChangeEffect {
                    new_version: self
                        .live
                        .current()
                        .meta()
                        .version()
                        .max(applied.version.get()),
                })
            }
            Err(e) => Err(HostError {
                refusal: refusal_of_error(&e, change),
            }),
        }
    }

    fn meta(&self) -> Arc<EffectiveMeta> {
        self.live.current().meta().clone()
    }

    /// The stack's recorded per-change bound: every change plans the schema, applies it under the
    /// migration engine's lock (which plans again), backfills and swaps the engine, so one change
    /// measured 500 to 570 ms on an idle 24-core host (PostgreSQL 18, 8 changes, rising slowly with
    /// the column count). The shared 500 ms constant is the in-memory registry's gross bound; this
    /// one is 2 s, still a gross liveness bound, not a performance budget (that is L9's).
    fn per_change_budget(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    fn probe(&mut self, layer: Probe) -> ProbeOutcome {
        let meta = self.live.current().meta().clone();
        let version = meta.version();
        let (field, want) = match &self.touched {
            Some(t) => (Some((t.doctype.clone(), t.field.clone())), Some(t.present)),
            None => (None, None),
        };
        let has_field = field.as_ref().is_some_and(|(dt, f)| match f {
            Some(f) => meta.doctype(dt).is_some_and(|d| d.field(f).is_some()),
            None => true,
        });
        match layer {
            Probe::Plan => plan_probe(
                &PlanView {
                    version: self.applied.load(Ordering::SeqCst),
                    has_field,
                },
                version,
                want,
            ),
            Probe::View => view_probe(&ViewView { version, has_field }, version, want),
            Probe::Schema => {
                let Some(catalog) = self.catalog() else {
                    return ProbeOutcome::Stale {
                        observed: 0,
                        seen: version,
                    };
                };
                let schema = self.schema.clone();
                match self.rt.block_on(async move { schema.plan(&catalog).await }) {
                    Ok(plan) => schema_probe(
                        &SchemaView {
                            pending_steps: plan.steps.len(),
                            refused_steps: plan.refused.len(),
                        },
                        version,
                    ),
                    Err(_) => ProbeOutcome::Stale {
                        observed: 0,
                        seen: version,
                    },
                }
            }
        }
    }
}

/// Builds fresh stacks, one per scenario.
pub struct StackFactory;

impl HostFactory for StackFactory {
    fn fresh(&self) -> Box<dyn LivenessHost> {
        match StackHost::boot() {
            Some(host) => Box::new(host),
            None => panic!(
                "no PostgreSQL: the caller must check SPOT_TEST_PG_URL before running the stack"
            ),
        }
    }
}
