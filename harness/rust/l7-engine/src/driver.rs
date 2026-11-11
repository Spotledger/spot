//! The engine workload driver: T1's operation set run over `DocumentService` instead of raw
//! storage (WP-TST-07.T2).
//!
//! Each worker runs a deterministic, seeded mix of named operations (create with a naming
//! series, child rows and links; update with the version check; post; cancel; delete with link
//! checks; rename; a whitelisted method call; a read) as one of the six seeded principals. An
//! operation a principal may not do is *attempted* and expected to be refused; the oracle
//! records the refusal, and the invariants prove it left no trace.
//!
//! The driver keeps two records, both exact:
//!
//! - a T1 [`History`] of every operation, mapped onto T1's [`OpKind`] taxonomy only so the T1
//!   report's outcome counts are meaningful (the engine invariants do not read it);
//! - an [`EngineRun`] oracle: the documents a committed operation left, their versions and
//!   states, the lifecycle event each committed operation must have produced, and the refusals.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::prelude::{Date, Decimal, Month, Name, RecordId, Value};
use spot_app_api::{AppError, ChildRow, DocState, Document, DocumentPatch, ErrorClass};
use spot_document::engine::Engine;
use spot_events_contract::lifecycle::Operation;
use spot_l7_invariants::history::{History, HistoryEntry, Level, OpKind, Outcome};
use spot_l7_invariants::invariant::RunMeta;
use spot_l7_invariants::schedule::{Profile, Rng};
use spot_storage_contract::{StorageBackend, Version};

pub use crate::oracle::{
    DeniedOp, EngineRun, ExpectedDoc, ExpectedEvent, OpOutcome, OpRecord, RecordedLink,
};
use crate::world::{LABEL, PROJECT, TASK, TIME_ENTRY, USER, World};

/// One operation the engine driver runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// Create a Project (child rows, an owner link, a name).
    CreateProject,
    /// Create a Task (checklist rows, a project link).
    CreateTask,
    /// Create a Time Entry draft (task and person links, a name).
    CreateTimeEntry,
    /// Update a Task (optimistic version check).
    UpdateTask,
    /// Update a Time Entry (optimistic version check).
    UpdateTimeEntry,
    /// Post a Time Entry.
    PostTimeEntry,
    /// Cancel a posted Time Entry.
    CancelTimeEntry,
    /// Delete a Task.
    DeleteTask,
    /// Delete a Time Entry draft.
    DeleteTimeEntry,
    /// Rename a Task.
    RenameTask,
    /// Call the whitelisted `l7:touch` method on a Task.
    CallTaskMethod,
    /// Load a Task (a permission-filtered read).
    ReadTask,
}

impl Op {
    /// Every operation, in a stable order.
    pub const ALL: [Op; 12] = [
        Op::CreateProject,
        Op::CreateTask,
        Op::CreateTimeEntry,
        Op::UpdateTask,
        Op::UpdateTimeEntry,
        Op::PostTimeEntry,
        Op::CancelTimeEntry,
        Op::DeleteTask,
        Op::DeleteTimeEntry,
        Op::RenameTask,
        Op::CallTaskMethod,
        Op::ReadTask,
    ];

    /// The DocType the operation targets.
    pub fn entity(self) -> &'static str {
        match self {
            Op::CreateProject => PROJECT,
            Op::CreateTask
            | Op::UpdateTask
            | Op::DeleteTask
            | Op::RenameTask
            | Op::CallTaskMethod
            | Op::ReadTask => TASK,
            Op::CreateTimeEntry
            | Op::UpdateTimeEntry
            | Op::PostTimeEntry
            | Op::CancelTimeEntry
            | Op::DeleteTimeEntry => TIME_ENTRY,
        }
    }

    /// The lifecycle topic of a committed operation (`""` for a read).
    pub fn topic(self) -> &'static str {
        match self {
            Op::CreateProject | Op::CreateTask | Op::CreateTimeEntry => {
                Operation::Create.topic_name()
            }
            Op::UpdateTask | Op::UpdateTimeEntry | Op::CallTaskMethod => {
                Operation::Update.topic_name()
            }
            Op::PostTimeEntry => Operation::Post.topic_name(),
            Op::CancelTimeEntry => Operation::Cancel.topic_name(),
            Op::DeleteTask | Op::DeleteTimeEntry => Operation::Delete.topic_name(),
            Op::RenameTask => Operation::Rename.topic_name(),
            Op::ReadTask => "",
        }
    }

    /// Whether the operation writes (and so must produce a lifecycle event when it commits).
    pub fn writes(self) -> bool {
        self != Op::ReadTask
    }

    /// A human label, for reports.
    pub fn label(self) -> &'static str {
        match self {
            Op::CreateProject => "create_project",
            Op::CreateTask => "create_task",
            Op::CreateTimeEntry => "create_time_entry",
            Op::UpdateTask => "update_task",
            Op::UpdateTimeEntry => "update_time_entry",
            Op::PostTimeEntry => "post_time_entry",
            Op::CancelTimeEntry => "cancel_time_entry",
            Op::DeleteTask => "delete_task",
            Op::DeleteTimeEntry => "delete_time_entry",
            Op::RenameTask => "rename_task",
            Op::CallTaskMethod => "call_task_method",
            Op::ReadTask => "read_task",
        }
    }
}

/// The principals an operation is attempted as, weighted so the workload reaches every right.
fn actors(op: Op) -> &'static [&'static str] {
    match op {
        Op::CreateProject
        | Op::CreateTask
        | Op::CreateTimeEntry
        | Op::UpdateTask
        | Op::RenameTask => &[
            "manager", "manager", "member", "admin", "system", "viewer", "ghost",
        ],
        Op::UpdateTimeEntry => &["manager", "manager", "admin", "system", "member", "viewer"],
        Op::PostTimeEntry | Op::CancelTimeEntry | Op::DeleteTask | Op::DeleteTimeEntry => {
            &["manager", "manager", "admin", "system", "member", "viewer"]
        }
        Op::CallTaskMethod => &["manager", "member", "admin", "system", "viewer", "ghost"],
        Op::ReadTask => &["manager", "member", "viewer", "admin", "system", "ghost"],
    }
}

/// How a run is configured.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    /// The seed: the plan is a pure function of it.
    pub seed: u64,
    /// Concurrent workers.
    pub workers: u32,
    /// Operations over all workers.
    pub operations: u64,
    /// The profile name, for the report.
    pub profile: String,
    /// The backend name, for the report.
    pub backend: String,
}

impl DriverConfig {
    /// The configuration of a named profile.
    pub fn for_profile(profile: Profile, seed: u64, backend: &str) -> DriverConfig {
        DriverConfig {
            seed,
            workers: profile.workers(),
            operations: profile.operations(),
            profile: profile.name().to_owned(),
            backend: backend.to_owned(),
        }
    }
}

/// A finished engine run: the history, the oracle, the engine (for the checks' store access)
/// and the run's description.
pub struct Run<B: StorageBackend> {
    /// T1's history, for the report's outcome counts.
    pub history: History,
    /// The engine oracle.
    pub oracle: Arc<EngineRun>,
    /// The engine, so the checks can read the store it wrote.
    pub engine: Arc<Engine<B>>,
    /// The run's description.
    pub meta: RunMeta,
}

/// One planned operation.
#[derive(Clone, Copy, Debug)]
struct Planned {
    op: Op,
    actor: &'static str,
    pick: u64,
}

fn plan(seed: u64, workers: u32, operations: u64) -> Vec<Vec<Planned>> {
    let workers = workers.max(1);
    let mut out = Vec::new();
    for w in 0..workers {
        let mut rng = Rng::new(seed ^ (u64::from(w) + 1).wrapping_mul(0xA076_1D64_78BD_642F));
        let share = operations / u64::from(workers)
            + u64::from(u64::from(w) < operations % u64::from(workers));
        let ops = (0..share)
            .map(|_| {
                let op = Op::ALL[rng.below(Op::ALL.len() as u64) as usize];
                let names = actors(op);
                let actor = names[rng.below(names.len() as u64) as usize];
                Planned {
                    op,
                    actor,
                    pick: rng.next_u64(),
                }
            })
            .collect();
        out.push(ops);
    }
    out
}

struct Shared {
    t0: Instant,
    oracle: Mutex<EngineRun>,
    entries: Mutex<Vec<HistoryEntry>>,
    pools: Mutex<Pools>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug)]
struct Target {
    id: RecordId,
    version: u64,
    state: Option<DocState>,
}

#[derive(Default)]
struct Pools {
    projects: Vec<Target>,
    tasks: Vec<Target>,
    entries: Vec<Target>,
    users: Vec<RecordId>,
    labels: Vec<RecordId>,
}

impl Pools {
    fn pick(list: &[Target], pick: u64) -> Option<Target> {
        if list.is_empty() {
            None
        } else {
            list.get((pick % list.len() as u64) as usize).copied()
        }
    }

    fn pick_id(list: &[RecordId], pick: u64) -> Option<RecordId> {
        if list.is_empty() {
            None
        } else {
            list.get((pick % list.len() as u64) as usize).copied()
        }
    }
}

/// The engine workload driver.
pub struct EngineDriver<B: StorageBackend> {
    engine: Arc<Engine<B>>,
    world: Arc<World>,
    config: DriverConfig,
    shared: Arc<Shared>,
}

impl<B: StorageBackend> EngineDriver<B> {
    /// A driver over `engine`.
    pub fn new(engine: Arc<Engine<B>>, world: Arc<World>, config: DriverConfig) -> Self {
        EngineDriver {
            engine,
            world,
            shared: Arc::new(Shared {
                t0: Instant::now(),
                oracle: Mutex::new(EngineRun::new(
                    config.seed,
                    config.workers,
                    &config.profile,
                    &config.backend,
                )),
                entries: Mutex::new(Vec::new()),
                pools: Mutex::new(Pools::default()),
            }),
            config,
        }
    }

    /// The engine under test.
    pub fn engine(&self) -> &Arc<Engine<B>> {
        &self.engine
    }

    /// Prepares the store, seeds the link targets, runs the workload and returns the records.
    pub async fn run(self) -> Result<Run<B>, AppError> {
        self.engine.prepare().await?;
        self.seed().await?;
        let schedule = plan(
            self.config.seed,
            self.config.workers,
            self.config.operations,
        );
        let mut tasks = Vec::new();
        for (worker, ops) in schedule.into_iter().enumerate() {
            let shared = self.shared.clone();
            let engine = self.engine.clone();
            let world = self.world.clone();
            let worker = u32::try_from(worker).unwrap_or(u32::MAX);
            let seed = self.config.seed;
            tasks.push(tokio::spawn(async move {
                let mut rng =
                    Rng::new(seed ^ (u64::from(worker) + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93));
                for (seq, planned) in ops.into_iter().enumerate() {
                    run_op(
                        &engine, &world, &shared, worker, seq as u64, planned, &mut rng,
                    )
                    .await;
                }
            }));
        }
        for task in tasks {
            task.await.map_err(|e| {
                AppError::new(ErrorClass::Internal, "l7_worker_panicked", e.to_string())
            })?;
        }
        let denial_count = self.engine.denial_count();
        let mut oracle = lock(&self.shared.oracle).clone();
        oracle.denial_count = denial_count;
        let mut entries = std::mem::take(&mut *lock(&self.shared.entries));
        entries.sort_by_key(|e| (e.start_us, e.worker, e.seq));
        let mut history = History::new(self.config.seed, self.config.workers, &self.config.profile);
        history.entries = entries;
        Ok(Run {
            history,
            oracle: Arc::new(oracle),
            engine: self.engine.clone(),
            meta: RunMeta {
                seed: self.config.seed,
                backend: self.config.backend.clone(),
                profile: self.config.profile.clone(),
                workers: self.config.workers,
            },
        })
    }

    /// Creates the link targets every later operation needs, as the system principal, and folds
    /// them into the oracle as the create operations they are.
    async fn seed(&self) -> Result<(), AppError> {
        let inv = self.world.inv("system");
        for i in 0..4 {
            let mut doc = Document::new(n(USER), RecordId::new_v7());
            doc.set(n("user_name"), Value::Text(format!("l7-user-{i}")));
            doc.set(n("enabled"), Value::Bool(true));
            let saved = self.engine.create(&inv, doc).await?.value;
            fold(&self.shared, 0, 0, "system", Op::CreateTask, USER, &saved);
            lock(&self.shared.pools).users.push(saved.id());
        }
        for i in 0..4 {
            let mut doc = Document::new(n(LABEL), RecordId::new_v7());
            doc.set(n("name"), Value::Text(format!("l7-label-{i}")));
            doc.set(n("colour"), Value::Text("#112233".to_owned()));
            let saved = self.engine.create(&inv, doc).await?.value;
            fold(&self.shared, 0, 0, "system", Op::CreateTask, LABEL, &saved);
            lock(&self.shared.pools).labels.push(saved.id());
        }
        let owner = Pools::pick_id(&lock(&self.shared.pools).users, 0);
        let label = Pools::pick_id(&lock(&self.shared.pools).labels, 0);
        let project = build_create(Op::CreateProject, RecordId::new_v7(), owner, label, 0);
        let saved_project = self.engine.create(&inv, project).await?.value;
        record_links(&self.shared, PROJECT, saved_project.id(), &saved_project);
        fold(
            &self.shared,
            0,
            0,
            "system",
            Op::CreateProject,
            PROJECT,
            &saved_project,
        );
        let project_id = saved_project.id();
        let task = build_create(
            Op::CreateTask,
            RecordId::new_v7(),
            Some(project_id),
            None,
            0,
        );
        let saved_task = self.engine.create(&inv, task).await?.value;
        record_links(&self.shared, TASK, saved_task.id(), &saved_task);
        fold(
            &self.shared,
            0,
            0,
            "system",
            Op::CreateTask,
            TASK,
            &saved_task,
        );
        let task_id = saved_task.id();
        let entry = build_create(
            Op::CreateTimeEntry,
            RecordId::new_v7(),
            Some(task_id),
            owner,
            0,
        );
        let saved_entry = self.engine.create(&inv, entry).await?.value;
        record_links(&self.shared, TIME_ENTRY, saved_entry.id(), &saved_entry);
        fold(
            &self.shared,
            0,
            0,
            "system",
            Op::CreateTimeEntry,
            TIME_ENTRY,
            &saved_entry,
        );
        Ok(())
    }
}

/// Builds a create document for `op`, with its child rows and links.
fn build_create(
    op: Op,
    id: RecordId,
    link: Option<RecordId>,
    label: Option<RecordId>,
    child_rows: u64,
) -> Document {
    let mut doc = Document::new(n(op.entity()), id);
    match op {
        Op::CreateProject => {
            doc.set(n("code"), Value::Text(format!("PRJ-{}", doc.id())));
            doc.set(n("title"), Value::Text(format!("Project {}", doc.id())));
            doc.set(n("status"), Value::Text("Planning".to_owned()));
            doc.set(n("owner"), link.map_or(Value::Null, Value::Id));
            if let Some(label) = label {
                let mut row = ChildRow {
                    id: RecordId::new_v7(),
                    fields: BTreeMap::new(),
                };
                row.fields.insert(n("label"), Value::Id(label));
                doc.rows_mut(n("labels")).push(row);
            }
        }
        Op::CreateTask => {
            doc.set(n("subject"), Value::Text(format!("Task {}", doc.id())));
            doc.set(n("project"), link.map_or(Value::Null, Value::Id));
            doc.set(n("status"), Value::Text("Open".to_owned()));
            doc.set(n("priority"), Value::Text("Medium".to_owned()));
            for i in 0..(child_rows % 3) {
                let mut row = ChildRow {
                    id: RecordId::new_v7(),
                    fields: BTreeMap::new(),
                };
                row.fields
                    .insert(n("text"), Value::Text(format!("step {}", i + 1)));
                row.fields.insert(n("done"), Value::Bool(false));
                doc.rows_mut(n("checklist")).push(row);
            }
        }
        Op::CreateTimeEntry => {
            doc.set(n("entry_no"), Value::Text(format!("TE-{}", doc.id())));
            doc.set(n("task"), link.map_or(Value::Null, Value::Id));
            doc.set(n("person"), label.map_or(Value::Null, Value::Id));
            doc.set(n("work_date"), Value::Date(date()));
            doc.set(n("hours"), Value::Decimal(Decimal::new(2, 0)));
            doc.set(n("billable"), Value::Bool(true));
        }
        _ => {}
    }
    doc
}

/// A secret the actor may plant in a Task's confidential `details` field (manager, admin and
/// system only; everyone else holds nothing in the `confidential` group).
fn secret_for(actor: &str, index: u64) -> Option<String> {
    matches!(actor, "manager" | "admin" | "system").then(|| format!("l7-secret-{index}"))
}

/// Records the link values a document holds, so `links-checked` can prove each target existed.
fn record_links(shared: &Arc<Shared>, entity: &str, id: RecordId, doc: &Document) {
    let fields: &[&str] = match entity {
        PROJECT => &["owner"],
        TASK => &["project", "assignee"],
        TIME_ENTRY => &["task", "person"],
        _ => &[],
    };
    let mut links = Vec::new();
    for field in fields {
        if let Value::Id(target) = doc.get(field) {
            links.push(RecordedLink {
                holder_entity: entity.to_owned(),
                holder_id: id,
                field: (*field).to_owned(),
                target_id: *target,
            });
        }
    }
    // The child link of a Project's label picks.
    for row in doc.rows("labels") {
        if let Some(Value::Id(target)) = row
            .fields
            .iter()
            .find(|(k, _)| k.as_str() == "label")
            .map(|(_, v)| v)
        {
            links.push(RecordedLink {
                holder_entity: entity.to_owned(),
                holder_id: id,
                field: "labels:label".to_owned(),
                target_id: *target,
            });
        }
    }
    if !links.is_empty() {
        lock(&shared.oracle).links.extend(links);
    }
}

/// Folds a committed operation into the oracle and the pools.
fn fold(
    shared: &Arc<Shared>,
    worker: u32,
    seq: u64,
    actor: &str,
    op: Op,
    entity: &str,
    saved: &Document,
) {
    let id = saved.id();
    let version = saved.version().map_or(0, |v| v.0);
    let state = saved.state();
    let name = saved
        .text(title_field(entity))
        .ok()
        .flatten()
        .unwrap_or_default()
        .to_owned();
    let mut oracle = lock(&shared.oracle);
    if op.writes() {
        oracle.events.push(ExpectedEvent {
            op,
            topic: op.topic(),
            entity: entity.to_owned(),
            id,
            version,
            actor: actor.to_owned(),
        });
    }
    oracle.ops.push(OpRecord {
        worker,
        seq,
        op,
        actor: actor.to_owned(),
        entity: entity.to_owned(),
        id,
        outcome: OpOutcome::Committed,
    });
    let key = (entity.to_owned(), id);
    match op {
        Op::DeleteTask | Op::DeleteTimeEntry => {
            if let Some(doc) = oracle.docs.get_mut(&key) {
                doc.live = false;
                doc.transitions.push(op);
            }
        }
        _ => {
            let entry = oracle.docs.entry(key).or_insert_with(|| ExpectedDoc {
                entity: entity.to_owned(),
                id,
                version,
                versions: BTreeSet::new(),
                state,
                live: true,
                name: name.clone(),
                transitions: Vec::new(),
            });
            entry.version = version;
            entry.versions.insert(version);
            entry.state = state;
            entry.live = true;
            if !name.is_empty() {
                entry.name = name;
            }
            entry.transitions.push(op);
        }
    }
    drop(oracle);
    let mut pools = lock(&shared.pools);
    let target = Target { id, version, state };
    match op {
        Op::CreateProject => pools.projects.push(target),
        Op::CreateTask => pools.tasks.push(target),
        Op::CreateTimeEntry => pools.entries.push(target),
        Op::DeleteTask => pools.tasks.retain(|t| t.id != id),
        Op::DeleteTimeEntry => pools.entries.retain(|t| t.id != id),
        Op::UpdateTask | Op::RenameTask | Op::CallTaskMethod => {
            if let Some(t) = pools.tasks.iter_mut().find(|t| t.id == id) {
                t.version = version;
                t.state = state;
            }
        }
        Op::UpdateTimeEntry | Op::PostTimeEntry | Op::CancelTimeEntry => {
            if let Some(t) = pools.entries.iter_mut().find(|t| t.id == id) {
                t.version = version;
                t.state = state;
            }
        }
        _ => {}
    }
}

/// Folds a committed delete: the document leaves the store and the event carries the deleted
/// version.
fn fold_delete(
    shared: &Arc<Shared>,
    worker: u32,
    seq: u64,
    actor: &str,
    op: Op,
    entity: &str,
    target: Target,
) {
    let mut oracle = lock(&shared.oracle);
    oracle.events.push(ExpectedEvent {
        op,
        topic: op.topic(),
        entity: entity.to_owned(),
        id: target.id,
        version: target.version,
        actor: actor.to_owned(),
    });
    oracle.ops.push(OpRecord {
        worker,
        seq,
        op,
        actor: actor.to_owned(),
        entity: entity.to_owned(),
        id: target.id,
        outcome: OpOutcome::Committed,
    });
    if let Some(doc) = oracle.docs.get_mut(&(entity.to_owned(), target.id)) {
        doc.live = false;
        doc.transitions.push(op);
    }
    drop(oracle);
    let mut pools = lock(&shared.pools);
    if op == Op::DeleteTask {
        pools.tasks.retain(|t| t.id != target.id);
    } else {
        pools.entries.retain(|t| t.id != target.id);
    }
}

fn title_field(entity: &str) -> &'static str {
    match entity {
        PROJECT => "code",
        TIME_ENTRY => "entry_no",
        _ => "subject",
    }
}

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("a storage name: {e:?}"))
}

fn date() -> Date {
    Date::from_calendar_date(2026, Month::January, 15)
        .unwrap_or_else(|e| panic!("a calendar date: {e:?}"))
}

fn version_of(target: Target) -> Version {
    Version(target.version)
}

/// Runs one planned operation and records it.
async fn run_op<B: StorageBackend>(
    engine: &Arc<Engine<B>>,
    world: &Arc<World>,
    shared: &Arc<Shared>,
    worker: u32,
    seq: u64,
    planned: Planned,
    rng: &mut Rng,
) {
    let start_us = u64::try_from(shared.t0.elapsed().as_micros()).unwrap_or(u64::MAX);
    let inv = world.inv(planned.actor);
    let op = planned.op;
    let target = snapshot_target(shared, op, planned.pick);
    let id = target.map_or_else(RecordId::new_v7, |t| t.id);
    let outcome = execute(engine, shared, &inv, planned, target, id, rng).await;
    let end_us = u64::try_from(shared.t0.elapsed().as_micros()).unwrap_or(u64::MAX);

    if !matches!(outcome, OpOutcome::Committed) {
        let mut oracle = lock(&shared.oracle);
        if let OpOutcome::Denied(code) = &outcome {
            oracle.denials.push(DeniedOp {
                op,
                entity: op.entity().to_owned(),
                id,
                actor: planned.actor.to_owned(),
                code: code.clone(),
            });
        }
        oracle.ops.push(OpRecord {
            worker,
            seq,
            op,
            actor: planned.actor.to_owned(),
            entity: op.entity().to_owned(),
            id,
            outcome: outcome.clone(),
        });
    }

    let entry = HistoryEntry {
        worker,
        seq,
        start_us,
        end_us,
        kind: t1_kind(op),
        isolation: Level::ReadCommitted,
        outcome: t1_outcome(&outcome),
        attempts: 1,
        touched: Vec::new(),
        values: BTreeMap::new(),
    };
    lock(&shared.entries).push(entry);
}

fn snapshot_target(shared: &Arc<Shared>, op: Op, pick: u64) -> Option<Target> {
    let pools = lock(&shared.pools);
    match op {
        Op::UpdateTask | Op::DeleteTask | Op::RenameTask | Op::CallTaskMethod | Op::ReadTask => {
            Pools::pick(&pools.tasks, pick)
        }
        Op::UpdateTimeEntry | Op::PostTimeEntry | Op::CancelTimeEntry | Op::DeleteTimeEntry => {
            Pools::pick(&pools.entries, pick)
        }
        Op::CreateProject | Op::CreateTask | Op::CreateTimeEntry => None,
    }
}

async fn execute<B: StorageBackend>(
    engine: &Arc<Engine<B>>,
    shared: &Arc<Shared>,
    inv: &Invocation,
    planned: Planned,
    target: Option<Target>,
    id: RecordId,
    rng: &mut Rng,
) -> OpOutcome {
    let op = planned.op;
    match op {
        Op::CreateProject | Op::CreateTask | Op::CreateTimeEntry => {
            let link = match op {
                Op::CreateProject => Pools::pick_id(&lock(&shared.pools).users, planned.pick),
                Op::CreateTask => {
                    Pools::pick(&lock(&shared.pools).projects, planned.pick).map(|t| t.id)
                }
                _ => Pools::pick(&lock(&shared.pools).tasks, planned.pick).map(|t| t.id),
            };
            let label = match op {
                Op::CreateProject => Pools::pick_id(&lock(&shared.pools).labels, planned.pick),
                Op::CreateTimeEntry => Pools::pick_id(&lock(&shared.pools).users, planned.pick),
                _ => None,
            };
            let mut doc = build_create(op, id, link, label, planned.pick);
            if op == Op::CreateTask
                && let Some(secret) = secret_for(planned.actor, planned.pick)
            {
                doc.set(n("details"), Value::Text(secret.clone()));
                lock(&shared.oracle).secrets.push(secret);
            }
            match engine.create(inv, doc).await {
                Ok(outcome) => {
                    record_links(shared, op.entity(), outcome.value.id(), &outcome.value);
                    fold(shared, 0, 0, planned.actor, op, op.entity(), &outcome.value);
                    OpOutcome::Committed
                }
                Err(e) => classify(&e),
            }
        }
        Op::UpdateTask | Op::UpdateTimeEntry => {
            let Some(target) = target else {
                return OpOutcome::Refused("document.not_found".to_owned());
            };
            let mut patch = DocumentPatch::default();
            if op == Op::UpdateTask {
                patch.fields.insert(
                    n("subject"),
                    Value::Text(format!(
                        "Task {} #{}",
                        target.id,
                        rng.next_u64() % 1_000_000
                    )),
                );
                if let Some(secret) = secret_for(planned.actor, rng.next_u64()) {
                    patch
                        .fields
                        .insert(n("details"), Value::Text(secret.clone()));
                    lock(&shared.oracle).secrets.push(secret);
                }
            } else {
                patch
                    .fields
                    .insert(n("hours"), Value::Decimal(Decimal::new(3, 0)));
            }
            match engine
                .update(inv, op.entity(), target.id, patch, version_of(target))
                .await
            {
                Ok(outcome) => {
                    fold(shared, 0, 0, planned.actor, op, op.entity(), &outcome.value);
                    OpOutcome::Committed
                }
                Err(e) => classify(&e),
            }
        }
        Op::PostTimeEntry | Op::CancelTimeEntry => {
            let Some(target) = target else {
                return OpOutcome::Refused("document.not_found".to_owned());
            };
            let result = if op == Op::PostTimeEntry {
                engine
                    .post(inv, TIME_ENTRY, target.id, version_of(target))
                    .await
            } else {
                engine
                    .cancel(inv, TIME_ENTRY, target.id, version_of(target))
                    .await
            };
            match result {
                Ok(outcome) => {
                    fold(shared, 0, 0, planned.actor, op, TIME_ENTRY, &outcome.value);
                    OpOutcome::Committed
                }
                Err(e) => classify(&e),
            }
        }
        Op::DeleteTask | Op::DeleteTimeEntry => {
            let Some(target) = target else {
                return OpOutcome::Refused("document.not_found".to_owned());
            };
            match engine
                .delete(inv, op.entity(), target.id, version_of(target))
                .await
            {
                Ok(_) => {
                    fold_delete(shared, 0, 0, planned.actor, op, op.entity(), target);
                    OpOutcome::Committed
                }
                Err(e) => classify(&e),
            }
        }
        Op::RenameTask => {
            let Some(target) = target else {
                return OpOutcome::Refused("document.not_found".to_owned());
            };
            let new_name = format!("Task renamed {}", target.id);
            match engine
                .rename(inv, TASK, target.id, &new_name, version_of(target))
                .await
            {
                Ok(outcome) => {
                    fold(shared, 0, 0, planned.actor, op, TASK, &outcome.value);
                    OpOutcome::Committed
                }
                Err(e) => classify(&e),
            }
        }
        Op::CallTaskMethod => {
            let Some(target) = target else {
                return OpOutcome::Refused("document.not_found".to_owned());
            };
            match engine
                .call_method(
                    inv,
                    TASK,
                    target.id,
                    "touch",
                    serde_json::Value::Null,
                    Some(version_of(target)),
                )
                .await
            {
                Ok(outcome) => {
                    fold(shared, 0, 0, planned.actor, op, TASK, &outcome.value.0);
                    OpOutcome::Committed
                }
                Err(e) => classify(&e),
            }
        }
        Op::ReadTask => {
            let Some(target) = target else {
                return OpOutcome::Refused("document.not_found".to_owned());
            };
            match engine.load(inv, TASK, target.id).await {
                Ok(_) => OpOutcome::Committed,
                Err(e) => classify(&e),
            }
        }
    }
}

/// How an [`AppError`] is classified.
fn classify(e: &AppError) -> OpOutcome {
    match e.class {
        ErrorClass::PermissionDenied => OpOutcome::Denied(e.code.clone()),
        ErrorClass::StaleVersion
        | ErrorClass::InvalidState
        | ErrorClass::NotFound
        | ErrorClass::Duplicate
        | ErrorClass::Linked
        | ErrorClass::Validation
        | ErrorClass::MethodNotFound
        | ErrorClass::UnknownDocType => OpOutcome::Refused(e.code.clone()),
        ErrorClass::Unavailable | ErrorClass::Internal => OpOutcome::Failed(e.code.clone()),
    }
}

fn t1_kind(op: Op) -> OpKind {
    match op {
        Op::CreateProject | Op::CreateTask | Op::CreateTimeEntry => {
            OpKind::CreateOrder { lines: 0 }
        }
        Op::UpdateTask | Op::UpdateTimeEntry => OpKind::UpdateOrder,
        Op::DeleteTask | Op::DeleteTimeEntry => OpKind::DeleteOrder,
        Op::PostTimeEntry | Op::CancelTimeEntry | Op::RenameTask | Op::CallTaskMethod => {
            OpKind::Emit { events: 1 }
        }
        Op::ReadTask => OpKind::Read,
    }
}

fn t1_outcome(outcome: &OpOutcome) -> Outcome {
    match outcome {
        OpOutcome::Committed => Outcome::Committed,
        OpOutcome::Denied(code) | OpOutcome::Refused(code) => {
            Outcome::Rejected { code: code.clone() }
        }
        OpOutcome::Failed(code) => Outcome::Failed {
            code: code.clone(),
            exhausted: false,
        },
    }
}
