//! [`StubHost`]: an in-memory host with the app API's semantics, for app tests and WP-TST-10.
//!
//! It runs the ADR-51 order exactly ([`sequence`]), enforces the phase rules on every context
//! call, rolls back everything (including other documents changed by `after_*` steps, jobs and
//! events) when a phase-1–3 step fails, runs committed steps after commit without rolling back,
//! applies step overrides, and records the lifecycle outbox events of ADR-056 with the
//! invocation's command id.
//!
//! What it does **not** do: storage-level behaviour (queries return `platform.unsupported`),
//! permissions, naming series, defaults beyond `amended_from`, time budgets, or concurrency
//! beyond optimistic version checks at commit. The platform's real host (WP-DOC-04, WP-DOC-10)
//! does all of those and passes the same [`contract_tests`](crate::contract_tests).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Value as Json, json};
use spot_events_contract::lifecycle::Operation;
use spot_events_contract::{EventMeta, PayloadDocument, Topic};
use spot_storage_contract::{Name, NodeRef, OutboxEvent, Query, RecordId, Rows, Value, Version};

use crate::BoxFuture;
use crate::context::{
    AMENDED_FROM, AppCache, DocumentService, HookContext, Invocation, JobRequest, MethodContext,
    Outcome, Repository, valid_job_name,
};
use crate::controller::{Controller, StepName};
use crate::document::{DocState, Document, DocumentPatch};
use crate::error::{AppError, AppResult, ErrorClass, FieldPath};
use crate::lifecycle::{HookPoint, Phase, Stage, Target, sequence};
use crate::message::Message;
use crate::meta::{DocTypeDef, FieldKind};
use crate::seams::{OverrideKind, SeamProvider, Seams, StepOverride};

type Store = BTreeMap<(Name, RecordId), Document>;

#[derive(Default)]
struct Registry {
    meta: BTreeMap<Name, Arc<DocTypeDef>>,
    controllers: Vec<Arc<dyn Controller>>,
    overrides: Vec<StepOverride>,
    seams: BTreeMap<(String, u32), Arc<dyn SeamProvider>>,
}

#[derive(Default)]
struct Delivered {
    outbox: Vec<OutboxEvent>,
    jobs: Vec<JobRequest>,
    notices: Vec<(RecordId, Json)>,
    committed_failures: Vec<(String, AppError)>,
    next_id: u64,
}

/// The in-memory host.
#[derive(Clone, Default)]
pub struct StubHost {
    reg: Arc<Mutex<Registry>>,
    store: Arc<Mutex<Store>>,
    delivered: Arc<Mutex<Delivered>>,
    cache: Arc<StubCache>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Guarded data stays consistent: every critical section is a plain read or replace.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn internal(code: &str, msg: impl Into<String>) -> AppError {
    AppError::new(ErrorClass::Internal, code, msg)
}

fn name(s: &str) -> AppResult<Name> {
    Name::new(s).map_err(|e| internal("platform.invalid_name", e.to_string()))
}

impl StubHost {
    /// An empty host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Defines or redefines a DocType, effective for the next operation (ADR-05).
    pub fn define(&self, def: DocTypeDef) {
        lock(&self.reg).meta.insert(def.name.clone(), Arc::new(def));
    }

    /// Registers a controller or controller extension.
    pub fn register(&self, controller: Arc<dyn Controller>) {
        lock(&self.reg).controllers.push(controller);
    }

    /// Installs a step override.
    pub fn add_override(&self, o: StepOverride) {
        lock(&self.reg).overrides.push(o);
    }

    /// Provides a seam.
    pub fn provide_seam(&self, name: &str, version: u32, provider: Arc<dyn SeamProvider>) {
        lock(&self.reg)
            .seams
            .insert((name.to_owned(), version), provider);
    }

    /// Outbox events published so far, in order.
    pub fn outbox(&self) -> Vec<OutboxEvent> {
        lock(&self.delivered).outbox.clone()
    }

    /// Jobs enqueued by committed transactions and committed steps.
    pub fn jobs(&self) -> Vec<JobRequest> {
        lock(&self.delivered).jobs.clone()
    }

    /// Realtime notices delivered.
    pub fn notices(&self) -> Vec<(RecordId, Json)> {
        lock(&self.delivered).notices.clone()
    }

    /// Committed steps that failed (they never roll back; a real host retries them).
    pub fn committed_failures(&self) -> Vec<(String, AppError)> {
        lock(&self.delivered).committed_failures.clone()
    }

    /// Whether a document is stored.
    pub fn stored(&self, doctype: &str, id: RecordId) -> Option<Document> {
        lock(&self.store)
            .iter()
            .find(|((d, i), _)| d.as_str() == doctype && *i == id)
            .map(|(_, v)| v.clone())
    }

    fn new_id(&self) -> RecordId {
        let mut d = lock(&self.delivered);
        d.next_id += 1;
        let n = d.next_id;
        // A deterministic UUIDv7-shaped id: version 7, variant 10, counter in the low bytes.
        let mut b = [0u8; 16];
        b[0] = 0x01;
        b[6] = 0x70;
        b[8] = 0x80;
        b[8..16].copy_from_slice(&(0x8000_0000_0000_0000u64 | n).to_be_bytes());
        RecordId::from_bytes(b).unwrap_or_else(|_| RecordId::new_v7())
    }

    fn meta_of(&self, doctype: &str) -> AppResult<Arc<DocTypeDef>> {
        lock(&self.reg)
            .meta
            .iter()
            .find(|(k, _)| k.as_str() == doctype)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| {
                AppError::new(
                    ErrorClass::UnknownDocType,
                    "meta.unknown_doctype",
                    format!("no DocType {doctype}"),
                )
            })
    }

    /// The steps to run at `point` for `doctype`, in order, with overrides applied.
    fn plan(&self, doctype: &str, point: &HookPoint) -> Vec<Planned> {
        let reg = lock(&self.reg);
        let mut decls: Vec<(i32, StepName, Arc<dyn Controller>)> = Vec::new();
        let mut all: BTreeMap<StepName, Arc<dyn Controller>> = BTreeMap::new();
        for c in reg.controllers.iter().filter(|c| c.doctype() == doctype) {
            for s in c.steps() {
                all.insert(s.name.clone(), c.clone());
                if &s.point == point {
                    decls.push((s.priority, s.name, c.clone()));
                }
            }
        }
        decls.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut out = Vec::new();
        for (_, step, ctrl) in decls {
            let ovs: Vec<&StepOverride> =
                reg.overrides.iter().filter(|o| o.target == step).collect();
            let by = |o: &StepOverride| {
                o.by.as_ref()
                    .and_then(|b| all.get(b).map(|c| (b.clone(), c.clone())))
            };
            let mut main = Some(Planned {
                step: step.clone(),
                ctrl: ctrl.clone(),
                wrapped: None,
            });
            let mut before = Vec::new();
            let mut after = Vec::new();
            for o in ovs {
                match o.kind {
                    OverrideKind::Disable => main = None,
                    OverrideKind::Replace => {
                        main = by(o).map(|(s, c)| Planned {
                            step: s,
                            ctrl: c,
                            wrapped: None,
                        })
                    }
                    OverrideKind::Wrap => {
                        if let (Some(m), Some((s, c))) = (main.take(), by(o)) {
                            main = Some(Planned {
                                step: s,
                                ctrl: c,
                                wrapped: Some((m.step, m.ctrl)),
                            });
                        }
                    }
                    OverrideKind::Before => before.extend(by(o).map(|(s, c)| Planned {
                        step: s,
                        ctrl: c,
                        wrapped: None,
                    })),
                    OverrideKind::After => after.extend(by(o).map(|(s, c)| Planned {
                        step: s,
                        ctrl: c,
                        wrapped: None,
                    })),
                }
            }
            out.extend(before);
            out.extend(main);
            out.extend(after);
        }
        out
    }
}

struct Planned {
    step: StepName,
    ctrl: Arc<dyn Controller>,
    wrapped: Option<(StepName, Arc<dyn Controller>)>,
}

/// One transaction's working state.
struct Tx {
    work: Store,
    touched: BTreeMap<(Name, RecordId), Option<Version>>,
    events: Vec<OutboxEvent>,
    jobs: Vec<JobRequest>,
    notices: Vec<(RecordId, Json)>,
    committed: Vec<(Document, HookPoint)>,
    messages: Vec<Message>,
}

impl Tx {
    fn get(&self, doctype: &str, id: RecordId) -> Option<&Document> {
        self.work
            .iter()
            .find(|((d, i), _)| d.as_str() == doctype && *i == id)
            .map(|(_, v)| v)
    }
}

impl StubHost {
    fn begin(&self) -> Tx {
        Tx {
            work: lock(&self.store).clone(),
            touched: BTreeMap::new(),
            events: Vec::new(),
            jobs: Vec::new(),
            notices: Vec::new(),
            committed: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Commits: every touched document must still be at the version this transaction read.
    fn commit(&self, tx: &mut Tx) -> AppResult<()> {
        let mut store = lock(&self.store);
        for (key, read) in &tx.touched {
            let now = store.get(key).and_then(|d| d.version());
            if now != *read {
                return Err(AppError::new(
                    ErrorClass::StaleVersion,
                    "document.stale_version",
                    "The document was changed by someone else.",
                )
                .detail("current_version", now.map_or(0, |v| v.0)));
            }
        }
        for key in tx.touched.keys() {
            match tx.work.get(key) {
                Some(d) => store.insert(key.clone(), d.clone()),
                None => store.remove(key),
            };
        }
        drop(store);
        let mut d = lock(&self.delivered);
        d.outbox.append(&mut tx.events);
        d.jobs.append(&mut tx.jobs);
        d.notices.append(&mut tx.notices);
        Ok(())
    }

    /// Runs a top-level operation: one transaction, then committed steps.
    fn top<'a>(&'a self, inv: &'a Invocation, req: Req) -> BoxFuture<'a, AppResult<Outcome<Ret>>> {
        Box::pin(async move {
            let mut tx = self.begin();
            let value = self.exec(&mut tx, inv, req).await?;
            self.commit(&mut tx)?;
            let mut messages = std::mem::take(&mut tx.messages);
            for (doc, point) in std::mem::take(&mut tx.committed) {
                let mut ctx = Ctx::new(self, None, inv, point.clone(), doc);
                if let Err(e) = run_point(&mut ctx).await {
                    lock(&self.delivered)
                        .committed_failures
                        .push((point.name(), e));
                }
                messages.append(&mut ctx.own_messages);
            }
            Ok(Outcome { value, messages })
        })
    }

    /// Executes one request inside `tx`.
    fn exec<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        req: Req,
    ) -> BoxFuture<'t, AppResult<Ret>> {
        Box::pin(async move {
            Ok(match req {
                Req::Create(doc) => Ret::Doc(self.do_create(tx, inv, doc).await?),
                Req::Update(dt, id, patch, v) => {
                    Ret::Doc(self.do_update(tx, inv, &dt, id, patch, v).await?)
                }
                Req::Transition(op, dt, id, v) => {
                    let d = self.do_transition(tx, inv, op, &dt, id, v).await?;
                    if op == Operation::Delete {
                        Ret::Unit
                    } else {
                        Ret::Doc(d)
                    }
                }
                Req::Correct(dt, id, v) => Ret::Doc(self.do_correct(tx, inv, &dt, id, v).await?),
                Req::Rename(dt, id, new_name, v) => {
                    Ret::Doc(self.do_rename(tx, inv, &dt, id, &new_name, v).await?)
                }
                Req::Method(dt, id, m, args, v) => {
                    let (d, j) = self.do_method(tx, inv, &dt, id, &m, args, v).await?;
                    Ret::DocJson(d, j)
                }
            })
        })
    }

    /// Runs one operation's phases 1–3 inside `tx`, queueing its committed steps.
    fn run_op<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        op: Operation,
        doc: Document,
    ) -> BoxFuture<'t, AppResult<Document>> {
        Box::pin(async move {
            let meta = self.meta_of(doc.doctype().as_str())?;
            let key = (doc.doctype().clone(), doc.id());
            let stored_version = tx.get(key.0.as_str(), key.1).and_then(|d| d.version());
            tx.touched.entry(key.clone()).or_insert(stored_version);
            let mut doc = doc;
            let before_delete = doc.clone();
            for stage in sequence(op) {
                match stage {
                    Stage::Hook(point) if point.phase() == Phase::Committed => {
                        let snapshot = if op == Operation::Delete {
                            before_delete.clone()
                        } else {
                            doc.clone()
                        };
                        tx.committed.push((snapshot, point));
                    }
                    Stage::Hook(point) => {
                        let mut ctx = Ctx::new(self, Some(&mut *tx), inv, point, doc);
                        run_point(&mut ctx).await?;
                        let own = std::mem::take(&mut ctx.own_messages);
                        doc = ctx.doc;
                        tx.messages.extend(own);
                    }
                    Stage::Write => {
                        doc = self.write(tx, inv, op, &meta, doc)?;
                    }
                    Stage::Commit => {}
                }
            }
            Ok(if op == Operation::Delete {
                before_delete
            } else {
                doc
            })
        })
    }

    /// The write stage: checks, state change, version, store and lifecycle event.
    fn write(
        &self,
        tx: &mut Tx,
        inv: &Invocation,
        op: Operation,
        meta: &DocTypeDef,
        mut doc: Document,
    ) -> AppResult<Document> {
        let key = (doc.doctype().clone(), doc.id());
        let state = match op {
            Operation::Delete => {
                self.check_not_linked(tx, &doc)?;
                tx.work.remove(&key);
                self.event(
                    tx,
                    inv,
                    op,
                    &doc,
                    doc.version(),
                    json!({"name": title(meta, &doc)}),
                )?;
                return Ok(doc);
            }
            Operation::Post => Some(DocState::Posted),
            Operation::Cancel => Some(DocState::Cancelled),
            Operation::Create => meta.is_posted.then_some(DocState::Draft),
            _ => doc.state(),
        };
        self.check_values(tx, meta, &doc)?;
        let changed: Vec<String> = doc
            .changed_fields()
            .iter()
            .map(|n| n.as_str().to_owned())
            .collect();
        let version = doc.version().map_or(Version::FIRST, Version::next);
        doc.mark_saved(version, state);
        tx.work.insert(key, doc.clone());
        let data = match op {
            Operation::Update | Operation::UpdatePosted => {
                json!({"name": title(meta, &doc), "changed_fields": changed})
            }
            Operation::Rename => json!({"old_name": "", "new_name": title(meta, &doc)}),
            _ => json!({"name": title(meta, &doc)}),
        };
        self.event(tx, inv, op, &doc, Some(version), data)?;
        Ok(doc)
    }

    fn event(
        &self,
        tx: &mut Tx,
        inv: &Invocation,
        op: Operation,
        doc: &Document,
        version: Option<Version>,
        mut data: Json,
    ) -> AppResult<()> {
        if let Json::Object(m) = &mut data {
            m.retain(|_, v| !v.is_null());
        }
        let id = self.new_id();
        let meta = EventMeta {
            occurred_at: inv.now,
            actor: inv.actor.clone(),
            command_id: inv.command_id.clone(),
            causation_id: None,
            correlation_id: id,
            trace_id: None,
        };
        let payload = PayloadDocument { meta, data };
        let subject = NodeRef {
            entity: doc.doctype().clone(),
            id: doc.id(),
        };
        let ev = payload
            .to_outbox_event(id, &op.topic(), subject, version, 1)
            .map_err(|e| internal("platform.event", e.to_string()))?;
        tx.events.push(ev);
        Ok(())
    }

    /// The scalar fields of `doc` (not the child rows) against `meta`: unknown fields, values
    /// that do not fit, links to nothing, listed select values and secrets held in a document.
    fn check_field_values<'a>(
        &'a self,
        tx: &'a Tx,
        meta: &'a DocTypeDef,
        doc: &'a Document,
        add: &'a mut impl FnMut(FieldPath, &str, &'static str),
    ) {
        for (k, v) in doc.fields() {
            match meta.field(k.as_str()) {
                None if k.as_str() == AMENDED_FROM => {}
                None => add(
                    FieldPath::field(k.as_str()),
                    "document.unknown_field",
                    "The DocType has no such field.",
                ),
                Some(f) if f.kind.accepts(v) => {
                    if let (FieldKind::Link { doctype }, Value::Id(target)) = (&f.kind, v)
                        && tx.get(doctype.as_str(), *target).is_none()
                    {
                        add(
                            FieldPath::field(k.as_str()),
                            "document.invalid_link",
                            "The linked document does not exist.",
                        );
                    }
                    self.check_select_and_secret(&f.kind, k.as_str(), v, add);
                }
                Some(f) if matches!(f.kind, FieldKind::Secret) && !v.is_null() => {
                    // A secret held in a document is its own error, not a generic one; the
                    // kind accepts no value, but the message must say why.
                    self.check_select_and_secret(&f.kind, k.as_str(), v, add);
                }
                Some(_) => add(
                    FieldPath::field(k.as_str()),
                    "document.invalid_value",
                    "The value does not fit the field.",
                ),
            }
        }
    }

    fn check_values(&self, tx: &Tx, meta: &DocTypeDef, doc: &Document) -> AppResult<()> {
        let mut err: Option<AppError> = None;
        let mut add = |path: FieldPath, code: &str, msg: &str| {
            err = Some(match err.take() {
                None => AppError::field(path, code, msg),
                Some(e) => e.with_field(path, code, msg),
            });
        };
        self.check_field_values(tx, meta, doc, &mut add);
        for f in &meta.fields {
            if f.required
                && matches!(doc.get(f.name.as_str()), Value::Null)
                && !matches!(f.kind, FieldKind::Table { .. })
            {
                add(
                    FieldPath::field(f.name.as_str()),
                    "document.required",
                    "This field is required.",
                );
            }
        }
        for (table, rows) in doc.tables() {
            let child = match meta.field(table.as_str()).map(|f| &f.kind) {
                Some(FieldKind::Table { doctype })
                | Some(FieldKind::TableMultiselect { doctype, .. }) => {
                    self.meta_of(doctype.as_str())?
                }
                _ => {
                    add(
                        FieldPath::field(table.as_str()),
                        "document.unknown_field",
                        "The DocType has no such table.",
                    );
                    continue;
                }
            };
            for (i, row) in rows.iter().enumerate() {
                let idx = u32::try_from(i + 1).unwrap_or(u32::MAX);
                for (k, v) in &row.fields {
                    match child.field(k.as_str()) {
                        None => add(
                            FieldPath::row(table.as_str(), idx, k.as_str()),
                            "document.unknown_field",
                            "The row has no such field.",
                        ),
                        Some(f) if !f.kind.accepts(v) => add(
                            FieldPath::row(table.as_str(), idx, k.as_str()),
                            "document.invalid_value",
                            "The value does not fit the field.",
                        ),
                        _ => {}
                    }
                }
                for f in child.fields.iter().filter(|f| f.required) {
                    if matches!(row.fields.get(&f.name).unwrap_or(&Value::Null), Value::Null) {
                        add(
                            FieldPath::row(table.as_str(), idx, f.name.as_str()),
                            "document.required",
                            "This field is required.",
                        );
                    }
                }
            }
        }
        err.map_or(Ok(()), Err)
    }

    /// ADR-068 §6: the listed values of a `Select` are the engine's check on write, and a
    /// `Secret` field never holds a value in the document.
    fn check_select_and_secret(
        &self,
        kind: &FieldKind,
        field: &str,
        v: &Value,
        add: &mut impl FnMut(FieldPath, &str, &'static str),
    ) {
        let path = FieldPath::field(field);
        match (kind, v) {
            (FieldKind::Select { values }, Value::Text(text)) => {
                if !values.iter().any(|listed| listed == text) {
                    add(
                        path,
                        "document.value_not_listed",
                        "The value is not one of the field's listed values.",
                    );
                }
            }
            (FieldKind::Secret, Value::Null) => {}
            (FieldKind::Secret, _) => {
                add(
                    path,
                    "document.secret_not_writable",
                    "A secret is set through the secrets service, never in a document.",
                );
            }
            _ => {}
        }
    }

    fn check_not_linked(&self, tx: &Tx, doc: &Document) -> AppResult<()> {
        let reg = lock(&self.reg);
        for ((dt, _), other) in &tx.work {
            let Some(m) = reg.meta.get(dt) else { continue };
            for f in &m.fields {
                if let FieldKind::Link { doctype } = &f.kind
                    && doctype == doc.doctype()
                    && other.get(f.name.as_str()) == &Value::Id(doc.id())
                {
                    return Err(AppError::new(
                        ErrorClass::Linked,
                        "document.linked",
                        format!("{dt} links to this document."),
                    ));
                }
            }
        }
        Ok(())
    }

    fn expect_version(
        tx: &Tx,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Document> {
        let doc = tx.get(doctype, id).cloned().ok_or_else(|| {
            AppError::new(
                ErrorClass::NotFound,
                "document.not_found",
                "The document does not exist.",
            )
        })?;
        if doc.version() != Some(expected) {
            return Err(AppError::new(
                ErrorClass::StaleVersion,
                "document.stale_version",
                "The document was changed by someone else.",
            )
            .detail("current_version", doc.version().map_or(0, |v| v.0)));
        }
        Ok(doc)
    }

    fn state_error(what: &str) -> AppError {
        AppError::new(
            ErrorClass::InvalidState,
            "document.invalid_state",
            format!("{what} is not allowed in the document's state."),
        )
    }

    fn do_create<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        doc: Document,
    ) -> BoxFuture<'t, AppResult<Document>> {
        Box::pin(async move {
            if tx.get(doc.doctype().as_str(), doc.id()).is_some() {
                return Err(AppError::new(
                    ErrorClass::Duplicate,
                    "document.duplicate",
                    "A document with this id already exists.",
                ));
            }
            self.run_op(tx, inv, Operation::Create, doc).await
        })
    }

    fn do_update<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        doctype: &'t str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'t, AppResult<Document>> {
        Box::pin(async move {
            let meta = self.meta_of(doctype)?;
            let mut doc = Self::expect_version(tx, doctype, id, expected)?;
            let op = match doc.state() {
                Some(DocState::Posted) => {
                    let bad: Vec<&Name> = patch
                        .fields
                        .keys()
                        .chain(patch.tables.keys())
                        .filter(|k| !meta.field(k.as_str()).is_some_and(|f| f.allow_on_posted))
                        .collect();
                    if let Some((first, rest)) = bad.split_first() {
                        let msg = "The field cannot be changed on a posted document.";
                        let e = rest.iter().fold(
                            AppError::field(
                                FieldPath::field(first.as_str()),
                                "document.read_only_field",
                                msg,
                            ),
                            |e, k| {
                                e.with_field(
                                    FieldPath::field(k.as_str()),
                                    "document.read_only_field",
                                    msg,
                                )
                            },
                        );
                        return Err(e);
                    }
                    Operation::UpdatePosted
                }
                Some(DocState::Cancelled) => {
                    return Err(Self::state_error("Changing a cancelled document"));
                }
                _ => Operation::Update,
            };
            // `create_only` (0.7.0): the value given at creation stays. A patch that carries the
            // value the field already has is not a change.
            let changed: Vec<&Name> = patch
                .fields
                .iter()
                .filter(|(k, v)| {
                    meta.field(k.as_str()).is_some_and(|f| f.create_only)
                        && doc.get(k.as_str()) != *v
                })
                .map(|(k, _)| k)
                .collect();
            if let Some((first, rest)) = changed.split_first() {
                let msg = "The field can be set only when the document is created.";
                let e = rest.iter().fold(
                    AppError::field(
                        FieldPath::field(first.as_str()),
                        "document.read_only_field",
                        msg,
                    ),
                    |e, k| {
                        e.with_field(
                            FieldPath::field(k.as_str()),
                            "document.read_only_field",
                            msg,
                        )
                    },
                );
                return Err(e);
            }
            doc.apply_patch(&patch, || self.new_id())?;
            self.run_op(tx, inv, op, doc).await
        })
    }

    fn do_transition<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        op: Operation,
        doctype: &'t str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'t, AppResult<Document>> {
        Box::pin(async move {
            let meta = self.meta_of(doctype)?;
            let doc = Self::expect_version(tx, doctype, id, expected)?;
            let ok = match op {
                Operation::Post => meta.is_posted && doc.state() == Some(DocState::Draft),
                Operation::Cancel => doc.state() == Some(DocState::Posted),
                Operation::Delete => doc.state().is_none() || doc.state() == Some(DocState::Draft),
                _ => true,
            };
            if !ok {
                return Err(Self::state_error(crate::lifecycle::op_name(op)));
            }
            self.run_op(tx, inv, op, doc).await
        })
    }
}

fn title(meta: &DocTypeDef, doc: &Document) -> Json {
    meta.title_field
        .as_ref()
        .and_then(|f| match doc.get(f.as_str()) {
            Value::Text(t) => Some(Json::String(t.clone())),
            _ => None,
        })
        .unwrap_or(Json::Null)
}

/// Runs every planned step at the context's hook point.
fn run_point<'c>(ctx: &'c mut Ctx<'_>) -> BoxFuture<'c, AppResult<()>> {
    Box::pin(async move {
        let point = ctx.point.clone();
        for p in ctx.host.plan(ctx.doc.doctype().as_str(), &point) {
            ctx.wrapped = p.wrapped.clone();
            p.ctrl.run_step(&p.step, ctx).await?;
        }
        ctx.wrapped = None;
        Ok(())
    })
}

/// The stub's hook and method context.
struct Ctx<'t> {
    host: &'t StubHost,
    /// `Some` in phases 1–3 and in methods; `None` in the committed and read phases, where
    /// writes run as new top-level transactions.
    tx: Option<&'t mut Tx>,
    inv: &'t Invocation,
    point: HookPoint,
    doc: Document,
    own_messages: Vec<Message>,
    wrapped: Option<(StepName, Arc<dyn Controller>)>,
    method: bool,
}

impl<'t> Ctx<'t> {
    fn new(
        host: &'t StubHost,
        tx: Option<&'t mut Tx>,
        inv: &'t Invocation,
        point: HookPoint,
        doc: Document,
    ) -> Self {
        Self {
            host,
            tx,
            inv,
            point,
            doc,
            own_messages: Vec::new(),
            wrapped: None,
            method: false,
        }
    }

    fn phase(&self) -> Phase {
        if self.method {
            Phase::After
        } else {
            self.point.phase()
        }
    }

    fn deny(&self, what: &str) -> AppError {
        AppError::phase_violation(what, &self.point.name())
    }

    fn check_write(&self, doctype: &str, id: RecordId) -> AppResult<()> {
        if !self.phase().rules().write_others {
            return Err(self.deny("Writing documents"));
        }
        if !self.method && doctype == self.doc.doctype().as_str() && id == self.doc.id() {
            return Err(self.deny("Writing the step's own document through the repository"));
        }
        Ok(())
    }

    /// Runs a write in this transaction (phase 3, methods) or as its own transaction (phase 4).
    async fn write(&mut self, req: Req) -> AppResult<Ret> {
        let (host, inv) = (self.host, self.inv);
        match self.tx.as_deref_mut() {
            Some(tx) => host.exec(tx, inv, req).await,
            None => {
                let o = host.top(inv, req).await?;
                self.own_messages.extend(o.messages);
                Ok(o.value)
            }
        }
    }
}

impl Repository for Ctx<'_> {
    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async move { self.host.meta_of(doctype) })
    }

    fn load<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            let found = match self.tx.as_deref() {
                Some(tx) => tx.get(doctype, id).cloned(),
                None => self.host.stored(doctype, id),
            };
            found.ok_or_else(|| {
                AppError::new(
                    ErrorClass::NotFound,
                    "document.not_found",
                    "The document does not exist.",
                )
            })
        })
    }

    fn query<'a>(&'a mut self, _query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async move {
            Err(internal(
                "platform.unsupported",
                "the stub host does not run queries",
            ))
        })
    }

    fn create(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async move {
            self.check_write(doc.doctype().as_str(), doc.id())?;
            self.write(Req::Create(doc)).await?.doc()
        })
    }

    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            self.check_write(doctype, id)?;
            self.write(Req::Update(doctype.to_owned(), id, patch, expected))
                .await?
                .doc()
        })
    }

    fn post<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            self.check_write(doctype, id)?;
            self.write(Req::Transition(
                Operation::Post,
                doctype.to_owned(),
                id,
                expected,
            ))
            .await?
            .doc()
        })
    }

    fn cancel<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            self.check_write(doctype, id)?;
            self.write(Req::Transition(
                Operation::Cancel,
                doctype.to_owned(),
                id,
                expected,
            ))
            .await?
            .doc()
        })
    }

    fn delete<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.check_write(doctype, id)?;
            self.write(Req::Transition(
                Operation::Delete,
                doctype.to_owned(),
                id,
                expected,
            ))
            .await
            .map(|_| ())
        })
    }
}

impl HookContext for Ctx<'_> {
    fn point(&self) -> &HookPoint {
        &self.point
    }

    fn invocation(&self) -> &Invocation {
        self.inv
    }

    fn doc(&self) -> &Document {
        &self.doc
    }

    fn doc_mut(&mut self) -> AppResult<&mut Document> {
        if self.phase().rules().modify_own || matches!(self.point, HookPoint::FieldChanged(_)) {
            Ok(&mut self.doc)
        } else {
            Err(self.deny("Changing the document"))
        }
    }

    fn message(&mut self, message: Message) {
        self.own_messages.push(message);
    }

    fn enqueue_job(&mut self, job: JobRequest) -> AppResult<()> {
        if !self.phase().rules().enqueue {
            return Err(self.deny("Enqueuing a job"));
        }
        if !valid_job_name(&job.job) {
            return Err(internal(
                "platform.invalid_job_name",
                format!("job names are <app>:<job>, got {:?}", job.job),
            ));
        }
        match self.tx.as_deref_mut() {
            Some(tx) => tx.jobs.push(job),
            None => lock(&self.host.delivered).jobs.push(job),
        }
        Ok(())
    }

    fn publish(&mut self, topic: &Topic, schema_version: u32, data: Json) -> AppResult<()> {
        if !self.phase().rules().enqueue {
            return Err(self.deny("Publishing an event"));
        }
        if topic.is_platform() {
            return Err(internal(
                "platform.reserved_topic",
                format!("{topic} is a platform topic"),
            ));
        }
        let id = self.host.new_id();
        let meta = EventMeta {
            occurred_at: self.inv.now,
            actor: self.inv.actor.clone(),
            command_id: self.inv.command_id.clone(),
            causation_id: None,
            correlation_id: id,
            trace_id: None,
        };
        let subject = NodeRef {
            entity: self.doc.doctype().clone(),
            id: self.doc.id(),
        };
        let ev = PayloadDocument { meta, data }
            .to_outbox_event(id, topic, subject, self.doc.version(), schema_version)
            .map_err(|e| internal("platform.event", e.to_string()))?;
        match self.tx.as_deref_mut() {
            Some(tx) => tx.events.push(ev),
            None => lock(&self.host.delivered).outbox.push(ev),
        }
        Ok(())
    }

    fn notify_user(&mut self, user: RecordId, payload: Json) -> AppResult<()> {
        if !self.phase().rules().enqueue {
            return Err(self.deny("Sending a realtime notice"));
        }
        match self.tx.as_deref_mut() {
            Some(tx) => tx.notices.push((user, payload)),
            None => lock(&self.host.delivered).notices.push((user, payload)),
        }
        Ok(())
    }

    fn external_allowed(&self) -> AppResult<()> {
        if self.phase().rules().external && !self.method {
            Ok(())
        } else {
            Err(self.deny("Calling an outside system"))
        }
    }

    fn seams(&self) -> &dyn Seams {
        self.host
    }

    fn cache(&self) -> &dyn AppCache {
        &*self.host.cache
    }

    fn run_wrapped(&mut self) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            let Some((step, ctrl)) = self.wrapped.take() else {
                return Err(self.deny("run_wrapped outside a wrap override"));
            };
            ctrl.run_step(&step, self).await
        })
    }
}

impl MethodContext for Ctx<'_> {
    fn invocation(&self) -> &Invocation {
        self.inv
    }

    fn doc(&self) -> &Document {
        &self.doc
    }

    fn message(&mut self, message: Message) {
        self.own_messages.push(message);
    }

    fn enqueue_job(&mut self, job: JobRequest) -> AppResult<()> {
        HookContext::enqueue_job(self, job)
    }

    fn seams(&self) -> &dyn Seams {
        self.host
    }
}

impl Seams for StubHost {
    fn call_json<'a>(
        &'a self,
        name: &'a str,
        version: u32,
        input: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let provider = lock(&self.reg)
                .seams
                .get(&(name.to_owned(), version))
                .cloned();
            match provider {
                Some(p) => p.call(input).await,
                None => Err(AppError::new(
                    ErrorClass::MethodNotFound,
                    "method.not_found",
                    format!("no provider for seam {name} v{version}"),
                )),
            }
        })
    }
}

/// The stub's cache: a plain map, no TTL enforcement.
#[derive(Default)]
struct StubCache(Mutex<BTreeMap<String, Vec<u8>>>);

impl AppCache for StubCache {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move { lock(&self.0).get(key).cloned() })
    }
    fn set<'a>(
        &'a self,
        key: &'a str,
        value: Vec<u8>,
        _ttl: Option<std::time::Duration>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.0).insert(key.to_owned(), value);
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.0).remove(key);
        })
    }
}

/// A write request, owned so it can cross transaction boundaries.
enum Req {
    Create(Document),
    Update(String, RecordId, DocumentPatch, Version),
    Transition(Operation, String, RecordId, Version),
    Correct(String, RecordId, Version),
    Rename(String, RecordId, String, Version),
    Method(String, RecordId, String, Json, Option<Version>),
}

/// The result of a [`Req`].
enum Ret {
    Doc(Document),
    Unit,
    DocJson(Document, Json),
}

impl Ret {
    fn doc(self) -> AppResult<Document> {
        match self {
            Ret::Doc(d) | Ret::DocJson(d, _) => Ok(d),
            Ret::Unit => Err(internal("platform.stub", "operation returned no document")),
        }
    }
}

fn outcome_doc(o: Outcome<Ret>) -> AppResult<Outcome<Document>> {
    Ok(Outcome {
        value: o.value.doc()?,
        messages: o.messages,
    })
}

impl StubHost {
    fn do_correct<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        doctype: &'t str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'t, AppResult<Document>> {
        Box::pin(async move {
            let original = self
                .do_transition(tx, inv, Operation::Cancel, doctype, id, expected)
                .await?;
            let mut draft = Document::new(original.doctype().clone(), self.new_id());
            for (k, v) in original.fields() {
                draft.set(k.clone(), v.clone());
            }
            for (t, rows) in original.tables() {
                let fresh = rows
                    .iter()
                    .map(|r| crate::document::ChildRow {
                        id: self.new_id(),
                        fields: r.fields.clone(),
                    })
                    .collect();
                *draft.rows_mut(t.clone()) = fresh;
            }
            draft.set(name(AMENDED_FROM)?, Value::Id(original.id()));
            self.do_create(tx, inv, draft).await
        })
    }

    fn do_rename<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        doctype: &'t str,
        id: RecordId,
        new_name: &'t str,
        expected: Version,
    ) -> BoxFuture<'t, AppResult<Document>> {
        Box::pin(async move {
            let meta = self.meta_of(doctype)?;
            let field = meta
                .title_field
                .clone()
                .ok_or_else(|| Self::state_error("Renaming a DocType without a title field"))?;
            let mut doc = Self::expect_version(tx, doctype, id, expected)?;
            let old = title(&meta, &doc);
            doc.set(field, Value::Text(new_name.to_owned()));
            let out = self.run_op(tx, inv, Operation::Rename, doc).await?;
            // Fill the rename event's old name, which only this operation knows.
            if let Some(ev) = tx
                .events
                .iter_mut()
                .rev()
                .find(|e| e.topic.as_str() == "doc_renamed")
                && let Ok(mut p) = PayloadDocument::from_outbox_event(ev)
            {
                p.data = json!({"old_name": old.as_str().unwrap_or(""), "new_name": new_name});
                if let Ok(t) = serde_json::to_string(&p) {
                    ev.payload = t;
                }
            }
            Ok(out)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn do_method<'t>(
        &'t self,
        tx: &'t mut Tx,
        inv: &'t Invocation,
        doctype: &'t str,
        id: RecordId,
        method: &'t str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'t, AppResult<(Document, Json)>> {
        Box::pin(async move {
            let ctrl = lock(&self.reg)
                .controllers
                .iter()
                .find(|c| {
                    c.doctype() == doctype && c.methods().iter().any(|m| m.name.as_str() == method)
                })
                .cloned()
                .ok_or_else(|| {
                    AppError::new(
                        ErrorClass::MethodNotFound,
                        "method.not_found",
                        format!("no method {method} on {doctype}"),
                    )
                })?;
            let doc = match expected {
                Some(v) => Self::expect_version(tx, doctype, id, v)?,
                None => tx.get(doctype, id).cloned().ok_or_else(|| {
                    AppError::new(
                        ErrorClass::NotFound,
                        "document.not_found",
                        "The document does not exist.",
                    )
                })?,
            };
            let point = HookPoint::of(Phase::After, Target::Op(Operation::Update));
            let mut ctx = Ctx::new(self, Some(&mut *tx), inv, point, doc);
            ctx.method = true;
            let result = ctrl.call_method(method, args, &mut ctx).await?;
            let own = std::mem::take(&mut ctx.own_messages);
            drop(ctx);
            tx.messages.extend(own);
            let after = tx.get(doctype, id).cloned().ok_or_else(|| {
                AppError::new(
                    ErrorClass::NotFound,
                    "document.not_found",
                    "The document was deleted by the method.",
                )
            })?;
            Ok((after, result))
        })
    }
}

impl DocumentService for StubHost {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async move { self.meta_of(doctype) })
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let doc = self.stored(doctype, id).ok_or_else(|| {
                AppError::new(
                    ErrorClass::NotFound,
                    "document.not_found",
                    "The document does not exist.",
                )
            })?;
            let mut ctx = Ctx::new(self, None, inv, HookPoint::Load, doc);
            run_point(&mut ctx).await?;
            Ok(Outcome {
                value: ctx.doc,
                messages: ctx.own_messages,
            })
        })
    }

    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { outcome_doc(self.top(inv, Req::Create(doc)).await?) })
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            outcome_doc(
                self.top(inv, Req::Update(doctype.to_owned(), id, patch, expected))
                    .await?,
            )
        })
    }

    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            outcome_doc(
                self.top(
                    inv,
                    Req::Transition(Operation::Post, doctype.to_owned(), id, expected),
                )
                .await?,
            )
        })
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            outcome_doc(
                self.top(
                    inv,
                    Req::Transition(Operation::Cancel, doctype.to_owned(), id, expected),
                )
                .await?,
            )
        })
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            outcome_doc(
                self.top(inv, Req::Correct(doctype.to_owned(), id, expected))
                    .await?,
            )
        })
    }

    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        Box::pin(async move {
            let o = self
                .top(
                    inv,
                    Req::Transition(Operation::Delete, doctype.to_owned(), id, expected),
                )
                .await?;
            Ok(Outcome {
                value: (),
                messages: o.messages,
            })
        })
    }

    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            outcome_doc(
                self.top(
                    inv,
                    Req::Rename(doctype.to_owned(), id, new_name.to_owned(), expected),
                )
                .await?,
            )
        })
    }

    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, Json)>>> {
        Box::pin(async move {
            let o = self
                .top(
                    inv,
                    Req::Method(doctype.to_owned(), id, method.to_owned(), args, expected),
                )
                .await?;
            match o.value {
                Ret::DocJson(d, j) => Ok(Outcome {
                    value: (d, j),
                    messages: o.messages,
                }),
                _ => Err(internal("platform.stub", "method returned no result")),
            }
        })
    }

    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let mut ctx = Ctx::new(
                self,
                None,
                inv,
                HookPoint::FieldChanged(field.to_owned()),
                draft,
            );
            run_point(&mut ctx).await?;
            Ok(Outcome {
                value: ctx.doc,
                messages: ctx.own_messages,
            })
        })
    }
}

impl crate::contract_tests::TestHost for StubHost {
    fn define(&self, def: DocTypeDef) {
        StubHost::define(self, def);
    }
    fn register(&self, controller: Arc<dyn Controller>) {
        StubHost::register(self, controller);
    }
    fn add_override(&self, o: StepOverride) {
        StubHost::add_override(self, o);
    }
    fn provide_seam(&self, name: &str, version: u32, provider: Arc<dyn SeamProvider>) {
        StubHost::provide_seam(self, name, version, provider);
    }
    fn outbox(&self) -> Vec<OutboxEvent> {
        StubHost::outbox(self)
    }
    fn jobs(&self) -> Vec<JobRequest> {
        StubHost::jobs(self)
    }
    fn committed_failures(&self) -> Vec<(String, AppError)> {
        StubHost::committed_failures(self)
    }
    fn stored(&self, doctype: &str, id: RecordId) -> Option<Document> {
        StubHost::stored(self, doctype, id)
    }
    fn fresh_id(&self) -> RecordId {
        self.new_id()
    }
}
