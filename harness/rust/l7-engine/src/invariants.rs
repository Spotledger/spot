//! The engine-level invariants (WP-TST-07.T2), registered on T1's registry.
//!
//! Each invariant holds the run's oracle ([`EngineRun`](crate::driver::EngineRun),
//! [`NamingRun`](crate::naming::NamingRun)) and reads the store through T1's
//! [`CheckContext`](spot_l7_invariants::CheckContext). The T1 catalog invariants
//! (`child-integrity`, `link-integrity`, `unique-keys`) are registered alongside them, so the
//! framework promises any catalog makes are covered too.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::DocState;
use spot_events_contract::PayloadDocument;
use spot_l7_invariants::builtin::{ChildIntegrity, LinkIntegrity, UniqueKeys};
use spot_l7_invariants::invariant::{
    BoxFuture, CheckContext, Invariant, InvariantRegistry, Violation,
};
use spot_l7_invariants::schedule::Profile;
use spot_storage_contract::{
    Expr, Name, OutboxRecord, Query, QueryPath, RecordId, Selection, StorageError, StorageResult,
    Value,
};

use crate::driver::{EngineRun, Op, OpOutcome};
use crate::naming::NamingRun;
use crate::world::{TIME_ENTRY, World};

/// Id of [`NamingGapless`].
pub const NAMING_GAPLESS: &str = "naming-gapless";
/// Id of [`NamingUnique`].
pub const NAMING_UNIQUE: &str = "naming-unique";
/// Id of [`LifecycleStateMachine`].
pub const LIFECYCLE_STATE_MACHINE: &str = "lifecycle-state_machine";
/// Id of [`EventsPerOperation`].
pub const EVENTS_PER_OPERATION: &str = "events-per_operation";
/// Id of [`NoSecretInEvents`].
pub const AUDIT_NO_SECRET: &str = "audit-no_secret_in_events";
/// Id of [`LinksChecked`].
pub const LINKS_CHECKED: &str = "links-checked";
/// Id of [`PermNoEffectOnDenial`].
pub const PERM_NO_EFFECT: &str = "perm-no_effect_on_denial";

/// The most denials the engine's log keeps.
const DENIAL_LOG: usize = 256;

macro_rules! engine_invariant {
    ($ty:ty, $id:expr, $desc:expr) => {
        impl Invariant for $ty {
            fn id(&self) -> &str {
                $id
            }
            fn describe(&self) -> &str {
                $desc
            }
            fn check<'a>(&'a self, ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
                Box::pin(async move {
                    let mut out = Vec::new();
                    if let Err(e) = self.run(ctx, &mut out).await {
                        out.push(Violation::read_failed($id, "store", &e));
                    }
                    out
                })
            }
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------------------------

fn n(s: &str) -> StorageResult<Name> {
    Name::new(s)
}

fn field(alias: &Name, f: &str) -> StorageResult<Expr> {
    Ok(Expr::field(alias, &n(f)?))
}

fn select(expr: Expr, alias: &str) -> StorageResult<Selection> {
    Ok(Selection {
        expr,
        alias: n(alias)?,
    })
}

fn bad_row(what: &str) -> StorageError {
    StorageError::new(
        spot_storage_contract::ErrorKind::Internal,
        format!("unexpected row shape: {what}"),
    )
}

fn as_int(v: &Value) -> StorageResult<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        _ => Err(bad_row("int")),
    }
}

fn as_text(v: &Value) -> StorageResult<String> {
    match v {
        Value::Text(t) => Ok(t.clone()),
        _ => Err(bad_row("text")),
    }
}

fn cell(row: &[Value], i: usize) -> StorageResult<&Value> {
    row.get(i).ok_or_else(|| bad_row("column count"))
}

fn parse_state(text: &str) -> Option<DocState> {
    match text {
        "draft" => Some(DocState::Draft),
        "posted" => Some(DocState::Posted),
        "cancelled" => Some(DocState::Cancelled),
        _ => None,
    }
}

/// `(version, state)` of one document, or `None` if it is absent. `posted` selects the engine's
/// `doc_state` column, which only a posted DocType carries.
async fn doc_row(
    ctx: &CheckContext,
    entity: &str,
    id: RecordId,
    posted: bool,
) -> StorageResult<Option<(u64, Option<DocState>)>> {
    let a = n("d")?;
    let mut q = Query::from(&n(entity)?, &a, QueryPath::Transactional);
    q.select = vec![select(field(&a, "version")?, "doc_version")?];
    if posted {
        q.select.push(select(field(&a, "doc_state")?, "doc_state")?);
    }
    q.filter = Some(Expr::field(&a, &n("id")?).eq(Expr::Value(Value::Id(id))));
    let rows = ctx.query(&q).await?;
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let version = u64::try_from(as_int(cell(row, 0)?)?).unwrap_or(0);
    let state = if posted {
        parse_state(&as_text(cell(row, 1)?)?)
    } else {
        None
    };
    Ok(Some((version, state)))
}

/// Whether a `label` with this exact name exists.
async fn label_exists(ctx: &CheckContext, name: &str) -> StorageResult<bool> {
    let a = n("l")?;
    let mut q = Query::from(&n("label")?, &a, QueryPath::Transactional);
    q.select = vec![select(field(&a, "id")?, "doc_id")?];
    q.filter = Some(Expr::field(&a, &n("name")?).eq(Expr::Value(Value::Text(name.to_owned()))));
    Ok(!ctx.query(&q).await?.rows.is_empty())
}

// ---------------------------------------------------------------------------------------------
// naming-gapless
// ---------------------------------------------------------------------------------------------

/// `naming-gapless`.
pub struct NamingGapless {
    naming: Arc<NamingRun>,
}

impl NamingGapless {
    /// The invariant over a naming run.
    pub fn new(naming: Arc<NamingRun>) -> NamingGapless {
        NamingGapless { naming }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for (series, numbers) in &self.naming.series {
            let top = numbers.iter().next_back().copied().unwrap_or(0);
            for k in 1..=top {
                if !numbers.contains(&k) {
                    out.push(Violation::new(
                        NAMING_GAPLESS,
                        "sequence",
                        series,
                        format!("gap number={k}"),
                    ));
                }
            }
            let next = ctx.next_number(series).await?;
            if next != top + 1 {
                out.push(Violation::new(
                    NAMING_GAPLESS,
                    "sequence",
                    series,
                    format!("next_number expected={} actual={next}", top + 1),
                ));
            }
        }
        // Every committed allocation left its marker row in the same transaction.
        for alloc in &self.naming.allocations {
            if !label_exists(ctx, &alloc.name).await? {
                out.push(Violation::new(
                    NAMING_GAPLESS,
                    "label",
                    &alloc.name,
                    format!(
                        "missing_marker series={} number={}",
                        alloc.series, alloc.number
                    ),
                ));
            }
        }
        Ok(())
    }
}

engine_invariant!(
    NamingGapless,
    NAMING_GAPLESS,
    "For every series the naming workload drove, the numbers handed out by committed \
     transactions are exactly 1..=n with no duplicate and no gap (a transaction that rolled \
     back after allocating returned its number), the series' next allocation is n+1, and the \
     marker row written in the same transaction as each committed allocation exists. It proves \
     nothing about a series the workload never used."
);

// ---------------------------------------------------------------------------------------------
// naming-unique
// ---------------------------------------------------------------------------------------------

/// `naming-unique`.
pub struct NamingUnique {
    run: Arc<EngineRun>,
}

impl NamingUnique {
    /// The invariant over an engine run.
    pub fn new(run: Arc<EngineRun>) -> NamingUnique {
        NamingUnique { run }
    }

    async fn run(&self, _ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        let mut seen: BTreeMap<(String, String), RecordId> = BTreeMap::new();
        for doc in self
            .run
            .docs
            .values()
            .filter(|d| d.live && !d.name.is_empty())
        {
            let key = (doc.entity.clone(), doc.name.clone());
            if let Some(first) = seen.get(&key) {
                out.push(Violation::new(
                    NAMING_UNIQUE,
                    &doc.entity,
                    doc.id,
                    format!("duplicate_name first={first}"),
                ));
            } else {
                seen.insert(key, doc.id);
            }
        }
        Ok(())
    }
}

engine_invariant!(
    NamingUnique,
    NAMING_UNIQUE,
    "No two live committed documents share a human-readable name: the name each committed \
     create or rename produced is unique within its DocType. The catalog's own unique indexes \
     (for example `project.code`) are covered by `unique-keys` as well."
);

// ---------------------------------------------------------------------------------------------
// lifecycle-state_machine
// ---------------------------------------------------------------------------------------------

/// `lifecycle-state_machine`.
pub struct LifecycleStateMachine {
    run: Arc<EngineRun>,
}

impl LifecycleStateMachine {
    /// The invariant over an engine run.
    pub fn new(run: Arc<EngineRun>) -> LifecycleStateMachine {
        LifecycleStateMachine { run }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for ((entity, id), doc) in &self.run.docs {
            let posted = entity == TIME_ENTRY;
            let stored = doc_row(ctx, entity, *id, posted).await?;
            match (doc.live, stored) {
                (true, None) => out.push(Violation::new(
                    LIFECYCLE_STATE_MACHINE,
                    entity,
                    id,
                    "missing_document",
                )),
                (true, Some((version, state))) => {
                    if version != doc.version {
                        out.push(Violation::new(
                            LIFECYCLE_STATE_MACHINE,
                            entity,
                            id,
                            format!("version expected={} actual={version}", doc.version),
                        ));
                    }
                    if state != doc.state {
                        out.push(Violation::new(
                            LIFECYCLE_STATE_MACHINE,
                            entity,
                            id,
                            format!(
                                "state expected={} actual={}",
                                state_text(doc.state),
                                state_text(state)
                            ),
                        ));
                    }
                }
                (false, Some(_)) => out.push(Violation::new(
                    LIFECYCLE_STATE_MACHINE,
                    entity,
                    id,
                    "deleted_document_present",
                )),
                (false, None) => {}
            }
            self.legality(entity, *id, doc, out);
        }
        Ok(())
    }

    /// Checks that the committed transitions of one document are reachable in the ADR-42
    /// machine.
    fn legality(
        &self,
        entity: &str,
        id: RecordId,
        doc: &crate::driver::ExpectedDoc,
        out: &mut Vec<Violation>,
    ) {
        let posted = entity == TIME_ENTRY;
        let mut state = None::<Sim>;
        for op in &doc.transitions {
            let next = match (*op, state) {
                (Op::CreateProject | Op::CreateTask | Op::CreateTimeEntry, None) => {
                    Some(if posted { Sim::Draft } else { Sim::Alive })
                }
                (
                    Op::UpdateTask | Op::UpdateTimeEntry | Op::RenameTask | Op::CallTaskMethod,
                    Some(Sim::Alive | Sim::Draft),
                ) => state,
                (Op::PostTimeEntry, Some(Sim::Draft)) => Some(Sim::Posted),
                (Op::CancelTimeEntry, Some(Sim::Posted)) => Some(Sim::Cancelled),
                (Op::DeleteTask | Op::DeleteTimeEntry, Some(Sim::Alive | Sim::Draft)) => {
                    Some(Sim::Deleted)
                }
                (op, _) => {
                    out.push(Violation::new(
                        LIFECYCLE_STATE_MACHINE,
                        entity,
                        id,
                        format!(
                            "illegal_transition op={} from={}",
                            op.label(),
                            state.map_or("none", Sim::text)
                        ),
                    ));
                    return;
                }
            };
            state = next;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sim {
    Alive,
    Draft,
    Posted,
    Cancelled,
    Deleted,
}

impl Sim {
    fn text(self) -> &'static str {
        match self {
            Sim::Alive => "alive",
            Sim::Draft => "draft",
            Sim::Posted => "posted",
            Sim::Cancelled => "cancelled",
            Sim::Deleted => "deleted",
        }
    }
}

fn state_text(state: Option<DocState>) -> &'static str {
    match state {
        Some(DocState::Draft) => "draft",
        Some(DocState::Posted) => "posted",
        Some(DocState::Cancelled) => "cancelled",
        None => "none",
    }
}

engine_invariant!(
    LifecycleStateMachine,
    LIFECYCLE_STATE_MACHINE,
    "Every committed document's stored version and state equal the oracle's (a document a \
     committed create left is present, a committed delete removed it, a post made it posted and \
     a cancel made it cancelled); and every committed transition is reachable in the ADR-42 \
     machine (a posted document is never edited, a cancelled document never posted, a delete \
     never removes a posted document)."
);

// ---------------------------------------------------------------------------------------------
// events-per_operation
// ---------------------------------------------------------------------------------------------

/// `events-per_operation`.
pub struct EventsPerOperation {
    run: Arc<EngineRun>,
    world: Arc<World>,
}

impl EventsPerOperation {
    /// The invariant over an engine run.
    pub fn new(run: Arc<EngineRun>, world: Arc<World>) -> EventsPerOperation {
        EventsPerOperation { run, world }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        let records = ctx.outbox().await?;
        let key = |topic: &str, entity: &str, id: RecordId, version: u64| {
            (topic.to_owned(), entity.to_owned(), id, version)
        };
        let mut actual: BTreeMap<(String, String, RecordId, u64), Vec<&OutboxRecord>> =
            BTreeMap::new();
        for rec in &records {
            let version = rec.event.subject_version.map_or(0, |v| v.0);
            actual
                .entry(key(
                    rec.event.topic.as_str(),
                    rec.event.subject.entity.as_str(),
                    rec.event.subject.id,
                    version,
                ))
                .or_default()
                .push(rec);
        }
        let expected: BTreeSet<(String, String, RecordId, u64)> = self
            .run
            .events
            .iter()
            .map(|e| key(e.topic, &e.entity, e.id, e.version))
            .collect();
        for expected_event in &self.run.events {
            let k = key(
                expected_event.topic,
                &expected_event.entity,
                expected_event.id,
                expected_event.version,
            );
            match actual.get(&k).map(Vec::as_slice) {
                None | Some([]) => out.push(Violation::new(
                    EVENTS_PER_OPERATION,
                    &expected_event.entity,
                    expected_event.id,
                    format!(
                        "missing_event op={} topic={} version={}",
                        expected_event.op.label(),
                        expected_event.topic,
                        expected_event.version
                    ),
                )),
                Some([rec]) => {
                    if let Some(actor) = event_actor(rec)
                        && actor != expected_actor(&self.world, &expected_event.actor)
                    {
                        out.push(Violation::new(
                            EVENTS_PER_OPERATION,
                            &expected_event.entity,
                            expected_event.id,
                            format!(
                                "wrong_actor op={} expected={} actual={}",
                                expected_event.op.label(),
                                expected_event.actor,
                                describe_actor(&actor)
                            ),
                        ));
                    }
                }
                Some(many) => out.push(Violation::new(
                    EVENTS_PER_OPERATION,
                    &expected_event.entity,
                    expected_event.id,
                    format!(
                        "duplicate_event op={} copies={}",
                        expected_event.op.label(),
                        many.len()
                    ),
                )),
            }
        }
        // No lifecycle event for a transaction that did not commit, and none the workload never
        // enqueued: exactly the expected lifecycle events are present.
        let lifecycle = |topic: &str| {
            spot_events_contract::lifecycle::Operation::ALL
                .iter()
                .any(|o| o.topic_name() == topic)
        };
        for rec in &records {
            if !lifecycle(rec.event.topic.as_str()) {
                continue;
            }
            let k = key(
                rec.event.topic.as_str(),
                rec.event.subject.entity.as_str(),
                rec.event.subject.id,
                rec.event.subject_version.map_or(0, |v| v.0),
            );
            if !expected.contains(&k) {
                let denied = self.run.denials.iter().any(|d| {
                    d.entity == rec.event.subject.entity.as_str() && d.id == rec.event.subject.id
                });
                out.push(Violation::new(
                    EVENTS_PER_OPERATION,
                    rec.event.subject.entity.as_str(),
                    rec.event.subject.id,
                    if denied {
                        "event_for_refused_operation"
                    } else {
                        "unknown_event"
                    },
                ));
            }
        }
        // Per document, the events appear in the outbox in the order the operations committed.
        let mut positions: BTreeMap<(String, RecordId), Vec<usize>> = BTreeMap::new();
        for (i, rec) in records.iter().enumerate() {
            positions
                .entry((
                    rec.event.subject.entity.as_str().to_owned(),
                    rec.event.subject.id,
                ))
                .or_default()
                .push(i);
        }
        for doc_events in self
            .run
            .events
            .chunk_by(|a, b| a.entity == b.entity && a.id == b.id)
        {
            let Some((entity, id)) = doc_events.first().map(|e| (e.entity.clone(), e.id)) else {
                continue;
            };
            let Some(seen) = positions.get(&(entity.clone(), id)) else {
                continue;
            };
            let ordered: Vec<usize> = doc_events
                .iter()
                .filter_map(|e| {
                    let k = key(e.topic, &e.entity, e.id, e.version);
                    actual.get(&k).and_then(|v| {
                        v.first().map(|r| {
                            seen.iter()
                                .position(|p| records[*p].event.id == r.event.id)
                                .unwrap_or(usize::MAX)
                        })
                    })
                })
                .collect();
            if ordered.windows(2).any(|w| w[1] < w[0]) {
                out.push(Violation::new(
                    EVENTS_PER_OPERATION,
                    &entity,
                    id,
                    "events_out_of_order",
                ));
            }
        }
        Ok(())
    }
}

fn event_actor(rec: &OutboxRecord) -> Option<spot_events_contract::Actor> {
    PayloadDocument::from_outbox_event(&rec.event)
        .ok()
        .map(|p| p.meta.actor)
}

fn expected_actor(world: &World, name: &str) -> spot_events_contract::Actor {
    if name == "system" {
        spot_events_contract::Actor::System {
            component: Name::new("l7_engine").unwrap_or_else(|e| panic!("a name: {e:?}")),
        }
    } else {
        spot_events_contract::Actor::User {
            id: world.principals.id(name),
            via: None,
        }
    }
}

fn describe_actor(actor: &spot_events_contract::Actor) -> String {
    match actor {
        spot_events_contract::Actor::System { component } => format!("system:{component}"),
        spot_events_contract::Actor::User { id, .. } => format!("user:{id}"),
        spot_events_contract::Actor::Consumer { id } => format!("consumer:{id}"),
    }
}

engine_invariant!(
    EventsPerOperation,
    EVENTS_PER_OPERATION,
    "Every committed write produced exactly one lifecycle event of the topic its operation maps \
     to, about the right subject at the version it wrote, with the actor of the invocation; no \
     event exists for a transaction that did not commit (a refusal leaves none); no lifecycle \
     event the workload never enqueued is present; and one document's events appear in the \
     outbox in the order its operations committed."
);

// ---------------------------------------------------------------------------------------------
// audit-no_secret_in_events
// ---------------------------------------------------------------------------------------------

/// `audit-no_secret_in_events`.
pub struct NoSecretInEvents {
    run: Arc<EngineRun>,
}

impl NoSecretInEvents {
    /// The invariant over an engine run.
    pub fn new(run: Arc<EngineRun>) -> NoSecretInEvents {
        NoSecretInEvents { run }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        let records = ctx.outbox().await?;
        for rec in &records {
            // The journal's `doc_state` image is the whole record on purpose (WP-JRN-02.T2,
            // ADR-061 section 3): personal fields stay in clear form there and the relay's
            // protect transform seals them on the way out. Every other topic stays value-free.
            if rec.event.topic.as_str() == "doc_state" {
                continue;
            }
            for secret in &self.run.secrets {
                if rec.event.payload.contains(secret.as_str()) {
                    out.push(Violation::new(
                        AUDIT_NO_SECRET,
                        "outbox",
                        rec.event.id,
                        format!("secret_in_event topic={}", rec.event.topic),
                    ));
                }
            }
        }
        Ok(())
    }
}

engine_invariant!(
    NoSecretInEvents,
    AUDIT_NO_SECRET,
    "None of the secret values the workload planted in the confidential `task.details` field \
     appears anywhere in the serialised outbox payloads: the lifecycle event carries the name \
     and the changed field names, never the values."
);

// ---------------------------------------------------------------------------------------------
// links-checked
// ---------------------------------------------------------------------------------------------

/// `links-checked`.
pub struct LinksChecked {
    run: Arc<EngineRun>,
}

impl LinksChecked {
    /// The invariant over an engine run.
    pub fn new(run: Arc<EngineRun>) -> LinksChecked {
        LinksChecked { run }
    }

    async fn run(&self, _ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for link in &self.run.links {
            let known = self.run.docs.keys().any(|(_, id)| *id == link.target_id);
            if !known {
                out.push(Violation::new(
                    LINKS_CHECKED,
                    &link.holder_entity,
                    link.holder_id,
                    format!("link_to_unknown field={}", link.field),
                ));
            }
        }
        Ok(())
    }
}

engine_invariant!(
    LinksChecked,
    LINKS_CHECKED,
    "Every link a committed document holds points at a document the oracle saw exist (a user, a \
     label, a project, a task or a time entry): the engine refused a create or update that \
     named a target that did not exist. The catalog's own `link-integrity` covers the store's \
     current rows as well."
);

// ---------------------------------------------------------------------------------------------
// perm-no_effect_on_denial
// ---------------------------------------------------------------------------------------------

/// `perm-no_effect_on_denial`.
pub struct PermNoEffectOnDenial {
    run: Arc<EngineRun>,
}

impl PermNoEffectOnDenial {
    /// The invariant over an engine run.
    pub fn new(run: Arc<EngineRun>) -> PermNoEffectOnDenial {
        PermNoEffectOnDenial { run }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for denied in &self.run.denials {
            let posted = denied.entity == TIME_ENTRY;
            let stored = doc_row(ctx, &denied.entity, denied.id, posted).await?;
            match denied.op {
                Op::CreateProject | Op::CreateTask | Op::CreateTimeEntry => {
                    if stored.is_some() {
                        out.push(Violation::new(
                            PERM_NO_EFFECT,
                            &denied.entity,
                            denied.id,
                            format!("denied_create_left_document actor={}", denied.actor),
                        ));
                    }
                }
                Op::DeleteTask | Op::DeleteTimeEntry => {
                    let still_expected = self
                        .run
                        .docs
                        .get(&(denied.entity.clone(), denied.id))
                        .is_none_or(|d| d.live);
                    if stored.is_none() && still_expected {
                        out.push(Violation::new(
                            PERM_NO_EFFECT,
                            &denied.entity,
                            denied.id,
                            format!("denied_delete_removed_document actor={}", denied.actor),
                        ));
                    }
                }
                _ => {
                    if let Some((version, _)) = stored
                        && let Some(doc) = self.run.docs.get(&(denied.entity.clone(), denied.id))
                        && !doc.versions.contains(&version)
                    {
                        out.push(Violation::new(
                            PERM_NO_EFFECT,
                            &denied.entity,
                            denied.id,
                            format!("denied_write_left_version actor={}", denied.actor),
                        ));
                    }
                }
            }
        }
        // The engine's denial log holds exactly the refusals the oracle recorded (capped).
        let expected_logged = self.run.denials.len().min(DENIAL_LOG);
        if self.run.denial_count != expected_logged {
            out.push(Violation::new(
                PERM_NO_EFFECT,
                "engine",
                "-",
                format!(
                    "denial_log expected={expected_logged} actual={}",
                    self.run.denial_count
                ),
            ));
        }
        if self.run.denials.is_empty() {
            out.push(Violation::new(
                PERM_NO_EFFECT,
                "engine",
                "-",
                "no_denials_exercised",
            ));
        }
        Ok(())
    }
}

engine_invariant!(
    PermNoEffectOnDenial,
    PERM_NO_EFFECT,
    "Every operation a principal was refused left no effect: a refused create left no document, \
     a refused delete removed none, a refused write produced no version the oracle did not \
     expect, and the engine's denial log holds exactly the refusals the run exercised (capped \
     at its 256-entry log). The workload always attempts at least one refusal."
);

// ---------------------------------------------------------------------------------------------
// Registries
// ---------------------------------------------------------------------------------------------

/// The engine invariants, plus the T1 catalog invariants that hold for any catalog.
pub fn engine_registry(run: &Arc<EngineRun>, world: &Arc<World>) -> InvariantRegistry {
    let mut registry = InvariantRegistry::new();
    let items: Vec<Box<dyn Invariant>> = vec![
        Box::new(NamingUnique::new(run.clone())),
        Box::new(LifecycleStateMachine::new(run.clone())),
        Box::new(EventsPerOperation::new(run.clone(), world.clone())),
        Box::new(NoSecretInEvents::new(run.clone())),
        Box::new(LinksChecked::new(run.clone())),
        Box::new(PermNoEffectOnDenial::new(run.clone())),
        Box::new(ChildIntegrity),
        Box::new(LinkIntegrity),
        Box::new(UniqueKeys),
    ];
    for item in items {
        // The ids are distinct; a refusal would be a bug in this list, caught by tests.
        let _ = registry.register(item);
    }
    registry
}

/// The naming invariant on its own registry.
pub fn naming_registry(naming: &Arc<NamingRun>) -> InvariantRegistry {
    let mut registry = InvariantRegistry::new();
    let _ = registry.register(Box::new(NamingGapless::new(naming.clone())));
    registry
}

/// The profiles the suite understands, for the CI script and the README.
pub fn profiles() -> [Profile; 3] {
    [Profile::Smoke, Profile::Standard, Profile::Nightly]
}

/// The number of operations the engine invariants can report, for tests.
pub fn engine_invariant_ids() -> Vec<&'static str> {
    vec![
        NAMING_UNIQUE,
        LIFECYCLE_STATE_MACHINE,
        EVENTS_PER_OPERATION,
        AUDIT_NO_SECRET,
        LINKS_CHECKED,
        PERM_NO_EFFECT,
        spot_l7_invariants::builtin::CHILD_INTEGRITY,
        spot_l7_invariants::builtin::LINK_INTEGRITY,
        spot_l7_invariants::builtin::UNIQUE_KEYS,
    ]
}

/// A violation of an operation that failed, for a report.
pub fn failed_op_violation(op: Op, id: RecordId, code: &str) -> Violation {
    Violation::new(
        "engine.operation",
        op.entity(),
        id,
        format!("failed op={} code={code}", op.label()),
    )
}

/// Whether an outcome is a refusal (never a success).
pub fn is_refusal(outcome: &OpOutcome) -> bool {
    !matches!(outcome, OpOutcome::Committed)
}
