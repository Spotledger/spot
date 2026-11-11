//! [`CustomiseService`]: the one orchestrated, crash-safe path that turns a customisation into a
//! live change.
//!
//! Every call runs the same **change pipeline**:
//!
//! 1. *validate*: build the candidate metadata in a scratch copy (the live snapshot is never
//!    touched) and report every problem at once, with field paths;
//! 2. *plan*: compute the schema plan for the resulting catalogue and **refuse** the change if
//!    it holds a destructive step, naming it;
//! 3. *persist*: write the records as `pending`, atomically (the intent marker);
//! 4. *apply*: run the additive schema plan;
//! 5. *mark and swap*: mark the records `applied`, build the new snapshot, adopt it, publish it
//!    on the invalidation bus, tell the host;
//! 6. *announce*: emit the lifecycle events.
//!
//! A crash between any two steps is recoverable. [`CustomiseService::recover`] (and the start of
//! every later call) finds records still `pending` or `removing`, re-plans from the records
//! (idempotent), and either finishes them or rolls them back to `failed` with the reason; the
//! previous snapshot keeps serving meanwhile. No database lock is held across the pipeline:
//! only the migration engine's per-step locks, and this node's in-process mutex that keeps two
//! callers from interleaving.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use spot_core::SiteId;
use spot_meta::{BuildOptions, Definition, FieldSpec};
use spot_meta_live::{Invalidation, LiveError};
use spot_meta_live::{InvalidationBus, LiveMeta, Snapshot, Version, VersionStore};
use spot_storage_contract::{Catalog, Collation};

use crate::assemble::{View, assemble, build, rebuildable};
use crate::error::{CustomiseError, CustomiseResult, Issue, codes};
use crate::host::{
    AuditEntry, AuditSink, Caller, CustomiseAccess, CustomiseEvent, DefaultGrant, EventSink,
    GrantSink, MemoryHost, ReferrerLookup, Right, RoleAccess, SYSTEM_MANAGER,
};
use crate::node::{DefinitionSink, MetaGate, SwapListener};
use crate::records::{
    DocTypeRecord, FieldRecord, PropertyRecord, RecordKey, RecordOp, RecordState, RecordStore,
    Records,
};
use crate::schema::{Fill, SchemaBackend};

mod prepare;
use prepare::{Change, backfill_value};

/// A test hook: called with the name of each pipeline step as it completes; returning `true`
/// stops the pipeline there with [`CustomiseError::Interrupted`], leaving exactly the state a
/// crash would. The steps are `validated`, `persisted`, `schema_applied`, `marked`, `swapped`
/// and `published`.
pub type FaultHook = dyn Fn(&'static str) -> bool + Send + Sync;

/// What the service is built from.
pub struct Parts {
    /// The site.
    pub site: SiteId,
    /// The definitions the installed apps ship (never changed by a site).
    pub base: Vec<Definition>,
    /// The build options every build uses (known seams, derivations).
    pub options: BuildOptions,
    /// This node's live metadata.
    pub live: Arc<LiveMeta>,
    /// Where a swap is announced.
    pub bus: Arc<dyn InvalidationBus>,
    /// The version store the bus writes; read for the head so new versions rise above it.
    pub versions: Arc<dyn VersionStore>,
    /// Where each published version's effective definitions are kept for followers.
    pub definitions: Arc<dyn DefinitionSink>,
    /// The customisation records.
    pub records: Arc<dyn RecordStore>,
    /// The database side.
    pub schema: Arc<dyn SchemaBackend>,
}

/// The outcome of a successful change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The metadata version this node serves after the change.
    pub version: Version,
}

/// What [`CustomiseService::recover`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovery {
    /// Whether unfinished records were found and resolved (finished or rolled back).
    pub resolved: bool,
    /// The version this node swapped to, when its snapshot was behind the records.
    pub swapped: Option<Version>,
}

/// One row of [`CustomiseService::list`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// `custom_doctype`, `custom_field` or `property_setter`.
    pub kind: &'static str,
    /// The DocType it concerns (for a custom DocType, its own name).
    pub doctype: String,
    /// The field, for a field or field-level property.
    pub field: Option<String>,
    /// The record's own name: the DocType, the field or the property key.
    pub name: String,
    /// Its state.
    pub state: RecordState,
    /// The reason it failed, if it did.
    pub reason: Option<String>,
    /// The change that last wrote it.
    pub intent: u64,
    /// Who made that change.
    pub actor: String,
}

/// The service. Cheap to share behind an `Arc`.
pub struct CustomiseService {
    site: SiteId,
    base: Vec<Definition>,
    options: BuildOptions,
    collation: Collation,
    bus: Arc<dyn InvalidationBus>,
    versions: Arc<dyn VersionStore>,
    definitions: Arc<dyn DefinitionSink>,
    records: Arc<dyn RecordStore>,
    schema: Arc<dyn SchemaBackend>,
    access: Arc<dyn CustomiseAccess>,
    grants: Arc<dyn GrantSink>,
    audit: Arc<dyn AuditSink>,
    events: Arc<dyn EventSink>,
    referrers: Arc<dyn ReferrerLookup>,
    listener: Option<Arc<dyn SwapListener>>,
    gate: Arc<MetaGate>,
    fault: Mutex<Option<Arc<FaultHook>>>,
    lock: tokio::sync::Mutex<()>,
}

impl CustomiseService {
    /// A service over `parts`, with `System Manager` as the only role that may customise and an
    /// in-memory host for grants, audit, events and referrers (replace with
    /// [`with_host`](Self::with_host) or the individual `with_*` calls).
    ///
    /// # Panics
    ///
    /// Never: the default collation is a constant locale.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        let host = Arc::new(MemoryHost::new());
        let collation = Collation::new("und").unwrap_or_else(|e| panic!("`und` is a locale: {e}"));
        Self {
            site: parts.site,
            base: parts.base,
            options: parts.options,
            collation,
            bus: parts.bus,
            versions: parts.versions,
            definitions: parts.definitions,
            records: parts.records,
            schema: parts.schema,
            access: Arc::new(RoleAccess::new()),
            grants: host.clone(),
            audit: host.clone(),
            events: host.clone(),
            referrers: host,
            listener: None,
            gate: Arc::new(MetaGate::new(parts.live)),
            fault: Mutex::new(None),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Replaces who may customise.
    #[must_use]
    pub fn with_access(mut self, access: Arc<dyn CustomiseAccess>) -> Self {
        self.access = access;
        self
    }

    /// Sends grants, audit, events and referrer lookups to one host object.
    #[must_use]
    pub fn with_host<H>(mut self, host: Arc<H>) -> Self
    where
        H: GrantSink + AuditSink + EventSink + ReferrerLookup + 'static,
    {
        self.grants = host.clone();
        self.audit = host.clone();
        self.events = host.clone();
        self.referrers = host;
        self
    }

    /// Tells `listener` after every local swap (the host points its engine at the new snapshot).
    #[must_use]
    pub fn with_listener(mut self, listener: Arc<dyn SwapListener>) -> Self {
        self.listener = Some(listener);
        self
    }

    /// Uses `collation` for the catalogue (it must match the engine's).
    #[must_use]
    pub fn with_collation(mut self, collation: Collation) -> Self {
        self.collation = collation;
        self
    }

    /// Installs or removes the test fault hook (see [`FaultHook`]).
    pub fn set_fault_hook(&self, hook: Option<Arc<FaultHook>>) {
        *self.fault.lock().unwrap_or_else(|p| p.into_inner()) = hook;
    }

    /// The gate through which this node serves metadata.
    #[must_use]
    pub fn gate(&self) -> &Arc<MetaGate> {
        &self.gate
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// Creates a DocType from the TOML text of a `spot-meta` definition.
    ///
    /// # Errors
    ///
    /// [`CustomiseError::Forbidden`], [`CustomiseError::Invalid`] (every problem at once),
    /// [`CustomiseError::Destructive`] or a store, schema or swap failure.
    pub async fn create_doctype(
        &self,
        caller: &Caller,
        def_toml: &str,
    ) -> CustomiseResult<Applied> {
        self.create_doctypes(caller, &[def_toml]).await
    }

    /// Creates several DocTypes in **one** change: a parent and the child DocTypes of its table
    /// fields have to appear together, because neither is valid without the other.
    ///
    /// # Errors
    ///
    /// As [`create_doctype`](Self::create_doctype); every definition's problems are reported at
    /// once.
    pub async fn create_doctypes(
        &self,
        caller: &Caller,
        defs: &[&str],
    ) -> CustomiseResult<Applied> {
        self.run(caller, Change::CreateDocTypes { defs }).await
    }

    /// Replaces a custom DocType's definition. Only additive schema changes are applied.
    ///
    /// # Errors
    ///
    /// As [`create_doctype`](Self::create_doctype), plus [`CustomiseError::NotFound`].
    pub async fn update_doctype(
        &self,
        caller: &Caller,
        name: &str,
        def_toml: &str,
    ) -> CustomiseResult<Applied> {
        self.run(caller, Change::UpdateDocType { name, def_toml })
            .await
    }

    /// Adds a custom field to any DocType, standard or custom.
    ///
    /// # Errors
    ///
    /// As [`create_doctype`](Self::create_doctype).
    pub async fn add_custom_field(
        &self,
        caller: &Caller,
        doctype: &str,
        spec: &FieldSpec,
        insert_after: Option<&str>,
    ) -> CustomiseResult<Applied> {
        let change = Change::AddField {
            doctype,
            spec,
            insert_after,
        };
        self.run(caller, change).await
    }

    /// Removes a custom field the site added, if nothing refers to it.
    ///
    /// # Errors
    ///
    /// As [`create_doctype`](Self::create_doctype); `Invalid` lists the referrers.
    pub async fn remove_custom_field(
        &self,
        caller: &Caller,
        doctype: &str,
        name: &str,
    ) -> CustomiseResult<Applied> {
        self.run(caller, Change::RemoveField { doctype, name })
            .await
    }

    /// Overrides a property of a DocType (`field: None`) or of one field. `value` is the text
    /// form `spot-meta`'s `Property::parse` reads.
    ///
    /// # Errors
    ///
    /// As [`create_doctype`](Self::create_doctype).
    pub async fn set_property(
        &self,
        caller: &Caller,
        doctype: &str,
        field: Option<&str>,
        property: &str,
        value: &str,
    ) -> CustomiseResult<Applied> {
        let change = Change::SetProperty {
            doctype,
            field,
            property,
            value,
        };
        self.run(caller, change).await
    }

    /// Removes a site's override, restoring the standard value. Refused for a security property
    /// on a standard field: a site may only tighten.
    ///
    /// # Errors
    ///
    /// As [`create_doctype`](Self::create_doctype), plus [`CustomiseError::NotFound`].
    pub async fn clear_property(
        &self,
        caller: &Caller,
        doctype: &str,
        field: Option<&str>,
        property: &str,
    ) -> CustomiseResult<Applied> {
        let change = Change::ClearProperty {
            doctype,
            field,
            property,
        };
        self.run(caller, change).await
    }

    /// Every customisation record with its state.
    ///
    /// # Errors
    ///
    /// [`CustomiseError::Forbidden`] or [`CustomiseError::Store`].
    pub async fn list(&self, caller: &Caller) -> CustomiseResult<Vec<Entry>> {
        self.authorise(caller)?;
        let records = self.load().await?;
        let mut entries = Vec::new();
        for r in &records.doctypes {
            entries.push(Entry {
                kind: "custom_doctype",
                doctype: r.name.clone(),
                field: None,
                name: r.name.clone(),
                state: r.state,
                reason: r.reason.clone(),
                intent: r.intent,
                actor: r.actor.clone(),
            });
        }
        for r in &records.fields {
            entries.push(Entry {
                kind: "custom_field",
                doctype: r.doctype.clone(),
                field: Some(r.name.clone()),
                name: r.name.clone(),
                state: r.state,
                reason: r.reason.clone(),
                intent: r.intent,
                actor: r.actor.clone(),
            });
        }
        for r in &records.properties {
            entries.push(Entry {
                kind: "property_setter",
                doctype: r.doctype.clone(),
                field: r.field.clone(),
                name: r.property.clone(),
                state: r.state,
                reason: r.reason.clone(),
                intent: r.intent,
                actor: r.actor.clone(),
            });
        }
        Ok(entries)
    }

    /// Start-up recovery: finishes or rolls back changes a crash left `pending`, then brings
    /// this node's snapshot in line with the applied records (publishing if it was behind).
    /// Safe to call at any time and any number of times.
    ///
    /// # Errors
    ///
    /// [`CustomiseError::Store`], [`CustomiseError::Schema`] or a swap failure.
    pub async fn recover(&self) -> CustomiseResult<Recovery> {
        let _guard = self.lock.lock().await;
        let resolved = self.settle().await?;
        let swapped = self.converge().await?;
        Ok(Recovery { resolved, swapped })
    }

    // ---------------------------------------------------------------- the pipeline

    async fn run(&self, caller: &Caller, change: Change<'_>) -> CustomiseResult<Applied> {
        self.authorise(caller)?;
        let _guard = self.lock.lock().await;
        self.settle().await?;
        self.converge().await?;

        let records = self.load().await?;
        let intent = next_intent(&records);
        let prepared = self.prepare(caller, &change, &records, intent).await?;
        let version = self.next_version().await?;

        // (a) validate in a scratch copy: the live snapshot is untouched.
        let scratch = records.with_ops(&prepared.ops);
        let mut issues = prepared.pre;
        let built = build(
            assemble(&self.base, &scratch, View::Wanted),
            &self.options,
            version,
        );
        let candidate = match built {
            Ok(candidate) if issues.is_empty() => candidate,
            Ok(_) => {
                return Err(CustomiseError::Invalid {
                    issues: dedupe(issues),
                });
            }
            Err(more) => {
                issues.extend(more);
                return Err(CustomiseError::Invalid {
                    issues: dedupe(issues),
                });
            }
        };

        // (b) plan the schema; refuse the destructive.
        let catalog = self.schema_catalog(&scratch, version)?;
        let plan = self
            .schema
            .plan(&catalog)
            .await
            .map_err(|message| CustomiseError::Schema { message })?;
        if !plan.refused.is_empty() {
            return Err(CustomiseError::Destructive {
                steps: plan.refused,
            });
        }
        self.fault("validated")?;

        // (c) persist the intent.
        self.records
            .commit(prepared.ops)
            .await
            .map_err(|message| CustomiseError::Store { message })?;
        self.fault("persisted")?;

        // (d) apply the additive plan, then give the rows that were already there the default.
        let held = self.load().await?;
        let fills = self.fills_for(&candidate, &held);
        if let Err(message) = self.schema.apply(&catalog).await {
            self.roll_back(&held, &message).await?;
            return Err(CustomiseError::Schema { message });
        }
        self.fault("schema_applied")?;
        if let Err(message) = self.schema.backfill(&catalog, &fills).await {
            self.roll_back(&held, &message).await?;
            return Err(CustomiseError::Schema { message });
        }
        self.fault("backfilled")?;

        // (e) mark applied, swap, publish.
        let finished = self.finish(&held, version).await?;
        self.fault("marked")?;
        self.converge().await?;
        // Requests that started on the old snapshot may have written a row without the new
        // field since the pass above; one more sweep fills those. Best effort: every row that
        // existed when the change started is already filled.
        let _ = self.schema.backfill(&catalog, &fills).await;

        // (f) announce.
        let live_version = self.gate.live().current().version();
        for event in finished {
            self.events.emit(CustomiseEvent {
                version: live_version.get(),
                ..event
            });
        }
        Ok(Applied {
            version: live_version,
        })
    }

    /// Resolves records a crash left `pending` or `removing`. Returns whether any were found.
    async fn settle(&self) -> CustomiseResult<bool> {
        let records = self.load().await?;
        if !records.has_unfinished() {
            return Ok(false);
        }
        let version = self.next_version().await?;
        let built = build(
            assemble(&self.base, &records, View::Wanted),
            &self.options,
            version,
        );
        match built {
            Err(issues) => {
                let reason = issues
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                self.roll_back(&records, &reason).await?;
            }
            Ok(candidate) => {
                let catalog = self.schema_catalog(&records, version)?;
                let plan = self
                    .schema
                    .plan(&catalog)
                    .await
                    .map_err(|message| CustomiseError::Schema { message })?;
                if !plan.refused.is_empty() {
                    let reason = plan
                        .refused
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ");
                    self.roll_back(&records, &reason).await?;
                } else {
                    let fills = self.fills_for(&candidate, &records);
                    let done = match self.schema.apply(&catalog).await {
                        Ok(()) => self.schema.backfill(&catalog, &fills).await.map(|_| ()),
                        Err(message) => Err(message),
                    };
                    match done {
                        Ok(()) => {
                            self.finish(&records, version).await?;
                        }
                        Err(message) => self.roll_back(&records, &message).await?,
                    }
                }
            }
        }
        Ok(true)
    }

    /// Brings this node's snapshot in line with the applied records. Returns the version
    /// swapped to, or `None` when the node already serves exactly that content.
    async fn converge(&self) -> CustomiseResult<Option<Version>> {
        let records = self.load().await?;
        let version = self.next_version().await?;
        let candidate = build(
            assemble(&self.base, &records, View::Current),
            &self.options,
            version,
        )
        .map_err(|issues| CustomiseError::Swap {
            message: format!(
                "the applied records do not build: {}",
                issues
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        })?;
        let live = self.gate.live();
        let current = live.current();
        if candidate.hash() == current.hash() {
            self.gate.clear();
            return Ok(None);
        }
        let definitions = rebuildable(candidate.meta().definitions());
        let head = self
            .versions
            .read()
            .await
            .map_err(|e| self.gate.observe(&e))?;
        let (target, publish) = match head {
            // The cluster already announced exactly this content: adopt it at that version.
            Some((head_version, head_hash))
                if &head_hash == candidate.hash() && head_version > current.version() =>
            {
                (head_version, false)
            }
            _ => (version, true),
        };
        self.definitions
            .put_definitions(target, definitions.clone());
        live.adopt(target, candidate.hash(), definitions)
            .map_err(|e| self.gate.observe(&e))?;
        self.gate.clear();
        self.fault("swapped")?;
        let snapshot = live.current();
        if publish {
            self.bus
                .publish(
                    &self.site,
                    Invalidation::meta(target, candidate.hash().clone()),
                )
                .await
                .map_err(|e| self.gate.observe(&e))?;
        }
        self.fault("published")?;
        if let Some(listener) = &self.listener {
            listener
                .swapped(&snapshot)
                .await
                .map_err(|message| CustomiseError::Swap { message })?;
        }
        Ok(Some(target))
    }

    // ---------------------------------------------------------------- finishing and rolling back

    /// Marks `pending` records applied and deletes `removing` ones, after the default grants
    /// and the audit entries. Returns the events to emit once the change is live.
    async fn finish(
        &self,
        records: &Records,
        version: Version,
    ) -> CustomiseResult<Vec<CustomiseEvent>> {
        let mut ops = Vec::new();
        let mut events = Vec::new();
        let event = |kind: &'static str, doctype: &str, field: Option<&str>| CustomiseEvent {
            kind,
            doctype: doctype.to_owned(),
            field: field.map(str::to_owned),
            version: version.get(),
        };
        let audit = |actor: &str,
                     action: &'static str,
                     subject: String,
                     before: Option<String>,
                     after: Option<String>| {
            self.audit.record(AuditEntry {
                actor: actor.to_owned(),
                action,
                subject,
                before,
                after,
                version: version.get(),
            });
        };

        for r in records
            .doctypes
            .iter()
            .filter(|r| r.state == RecordState::Pending)
        {
            if r.prior.is_none() {
                let grant = DefaultGrant {
                    doctype: r.name.clone(),
                    role: SYSTEM_MANAGER.to_owned(),
                    rights: vec![Right::Read, Right::Write, Right::Create, Right::Delete],
                };
                self.grants
                    .set_default_grants(&r.name, &[grant])
                    .map_err(|message| CustomiseError::Store { message })?;
            }
            let (action, kind) = if r.prior.is_none() {
                ("create_doctype", "doctype_created")
            } else {
                ("update_doctype", "doctype_updated")
            };
            audit(
                &r.actor,
                action,
                r.name.clone(),
                r.prior.clone(),
                Some(r.definition.clone()),
            );
            events.push(event(kind, &r.name, None));
            ops.push(RecordOp::PutDocType(DocTypeRecord {
                state: RecordState::Applied,
                reason: None,
                prior: None,
                ..r.clone()
            }));
        }
        for r in &records.fields {
            match r.state {
                RecordState::Pending => {
                    audit(
                        &r.actor,
                        "add_custom_field",
                        format!("{}.{}", r.doctype, r.name),
                        None,
                        Some(r.spec.clone()),
                    );
                    events.push(event("field_added", &r.doctype, Some(&r.name)));
                    ops.push(RecordOp::PutField(FieldRecord {
                        state: RecordState::Applied,
                        reason: None,
                        ..r.clone()
                    }));
                }
                RecordState::Removing => {
                    audit(
                        &r.actor,
                        "remove_custom_field",
                        format!("{}.{}", r.doctype, r.name),
                        Some(r.spec.clone()),
                        None,
                    );
                    events.push(event("field_removed", &r.doctype, Some(&r.name)));
                    ops.push(RecordOp::PutField(FieldRecord {
                        state: RecordState::Removed,
                        reason: None,
                        ..r.clone()
                    }));
                }
                _ => {}
            }
        }
        for r in &records.properties {
            let subject = match &r.field {
                Some(field) => format!("{}.{field}.{}", r.doctype, r.property),
                None => format!("{}.{}", r.doctype, r.property),
            };
            match r.state {
                RecordState::Pending => {
                    audit(
                        &r.actor,
                        "set_property",
                        subject,
                        r.prior.clone(),
                        Some(r.value.clone()),
                    );
                    events.push(event("property_set", &r.doctype, r.field.as_deref()));
                    ops.push(RecordOp::PutProperty(PropertyRecord {
                        state: RecordState::Applied,
                        reason: None,
                        prior: None,
                        ..r.clone()
                    }));
                }
                RecordState::Removing => {
                    audit(
                        &r.actor,
                        "clear_property",
                        subject,
                        Some(r.value.clone()),
                        None,
                    );
                    events.push(event("property_cleared", &r.doctype, r.field.as_deref()));
                    ops.push(RecordOp::Remove(RecordKey::Property(
                        r.doctype.clone(),
                        r.field.clone(),
                        r.property.clone(),
                    )));
                }
                _ => {}
            }
        }
        self.records
            .commit(ops)
            .await
            .map_err(|message| CustomiseError::Store { message })?;
        Ok(events)
    }

    /// Rolls every unfinished record back: a new record becomes `failed` with `reason`, a
    /// replaced one returns to its previous value, a removal is undone.
    async fn roll_back(&self, records: &Records, reason: &str) -> CustomiseResult<()> {
        let mut ops = Vec::new();
        for r in &records.doctypes {
            if r.state == RecordState::Pending {
                ops.push(RecordOp::PutDocType(match &r.prior {
                    Some(prior) => DocTypeRecord {
                        definition: prior.clone(),
                        state: RecordState::Applied,
                        reason: Some(reason.to_owned()),
                        prior: None,
                        ..r.clone()
                    },
                    None => DocTypeRecord {
                        state: RecordState::Failed,
                        reason: Some(reason.to_owned()),
                        ..r.clone()
                    },
                }));
            }
        }
        for r in &records.fields {
            match r.state {
                RecordState::Pending => ops.push(RecordOp::PutField(FieldRecord {
                    state: RecordState::Failed,
                    reason: Some(reason.to_owned()),
                    ..r.clone()
                })),
                RecordState::Removing => ops.push(RecordOp::PutField(FieldRecord {
                    state: RecordState::Applied,
                    reason: Some(reason.to_owned()),
                    ..r.clone()
                })),
                _ => {}
            }
        }
        for r in &records.properties {
            match r.state {
                RecordState::Pending => ops.push(RecordOp::PutProperty(match &r.prior {
                    Some(prior) => PropertyRecord {
                        value: prior.clone(),
                        state: RecordState::Applied,
                        reason: Some(reason.to_owned()),
                        prior: None,
                        ..r.clone()
                    },
                    None => PropertyRecord {
                        state: RecordState::Failed,
                        reason: Some(reason.to_owned()),
                        ..r.clone()
                    },
                })),
                RecordState::Removing => ops.push(RecordOp::PutProperty(PropertyRecord {
                    state: RecordState::Applied,
                    reason: Some(reason.to_owned()),
                    ..r.clone()
                })),
                _ => {}
            }
        }
        self.records
            .commit(ops)
            .await
            .map_err(|message| CustomiseError::Store { message })
    }

    // ---------------------------------------------------------------- small helpers

    fn authorise(&self, caller: &Caller) -> CustomiseResult<()> {
        if self.access.may_customise(caller) {
            Ok(())
        } else {
            Err(CustomiseError::Forbidden {
                user: caller.user.clone(),
            })
        }
    }

    async fn load(&self) -> CustomiseResult<Records> {
        self.records
            .load()
            .await
            .map_err(|message| CustomiseError::Store { message })
    }

    /// The next version: above this node's and above the cluster's announced head.
    async fn next_version(&self) -> CustomiseResult<Version> {
        let current = self.gate.live().current().version();
        let head = self
            .versions
            .read()
            .await
            .map_err(|e| self.gate.observe(&e))?
            .map_or(Version::ZERO, |(v, _)| v);
        current
            .max(head)
            .checked_next()
            .ok_or_else(|| self.gate.observe(&LiveError::Exhausted))
    }

    /// The values to write into the rows that were already there when a custom field is added:
    /// the static default of each pending field whose type can be backfilled. A DocType that is
    /// a child has no rows of its own to update (they belong to their parents) and is skipped.
    fn fills_for(&self, candidate: &Snapshot, records: &Records) -> Vec<Fill> {
        records
            .fields
            .iter()
            .filter(|r| r.state == RecordState::Pending)
            .filter(|r| {
                candidate
                    .meta()
                    .definition(&r.doctype)
                    .is_some_and(|d| !d.is_child)
            })
            .filter_map(|r| {
                let spec: FieldSpec = toml::from_str(&r.spec).ok()?;
                Some(Fill {
                    doctype: r.doctype.clone(),
                    field: r.name.clone(),
                    value: backfill_value(&spec)?,
                })
            })
            .collect()
    }

    /// The catalogue the database must hold for `records`: the live metadata's, plus the columns
    /// of removed custom fields (a runtime change never drops a column).
    fn schema_catalog(&self, records: &Records, version: Version) -> CustomiseResult<Catalog> {
        let build_view = |view| build(assemble(&self.base, records, view), &self.options, version);
        let candidate = match build_view(View::Schema) {
            Ok(candidate) => candidate,
            Err(_) => build_view(View::SchemaStrict)
                .map_err(|issues| CustomiseError::Invalid { issues })?,
        };
        self.catalog_of(&candidate, version)
    }

    /// The catalogue of `candidate`, stamped with `version` so the schema backend sees it rise.
    fn catalog_of(&self, candidate: &Snapshot, version: Version) -> CustomiseResult<Catalog> {
        let compiled = candidate
            .meta()
            .catalog(self.collation.clone())
            .map_err(|e| CustomiseError::Invalid {
                issues: vec![Issue::new(
                    codes::METADATA_INVALID,
                    String::new(),
                    e.to_string(),
                )],
            })?;
        Catalog::new(
            version.get(),
            compiled.collation.clone(),
            compiled.entities().cloned().collect(),
        )
        .map_err(|e| CustomiseError::Schema {
            message: e.to_string(),
        })
    }

    fn fault(&self, step: &'static str) -> CustomiseResult<()> {
        let hook = self.fault.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match hook {
            Some(hook) if hook(step) => Err(CustomiseError::Interrupted { step }),
            _ => Ok(()),
        }
    }
}

impl std::fmt::Debug for CustomiseService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomiseService")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}

fn next_intent(records: &Records) -> u64 {
    let docs = records.doctypes.iter().map(|r| r.intent);
    let fields = records.fields.iter().map(|r| r.intent);
    let props = records.properties.iter().map(|r| r.intent);
    docs.chain(fields).chain(props).max().unwrap_or(0) + 1
}

fn dedupe(issues: Vec<Issue>) -> Vec<Issue> {
    let mut seen = BTreeSet::new();
    issues
        .into_iter()
        .filter(|i| seen.insert((i.code, i.path.clone(), i.message.clone())))
        .collect()
}
