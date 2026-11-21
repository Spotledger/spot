//! Runs a sequence through the engine and compares every step with the model.
//!
//! After **every** call the runner asserts the result matches the model's expectation (accepted,
//! or refused with the right class) and reads back every document the sequence has touched:
//! state, version, hours or subject, the amendment link, and that a deleted document is gone.
//! Once per sequence it checks that the number of lifecycle events equals the number of committed
//! writes, each once.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::prelude::{Date, Decimal, Month, Name, RecordId, Value};
use spot_app_api::{AppError, AppResult, DocState, Document, DocumentPatch};
use spot_document::engine::Engine;
use spot_events_contract::lifecycle::Operation;
use spot_l7_engine::world::{PROJECT, TASK, TIME_ENTRY, USER, World};
use spot_storage_contract::{FeedPosition, StorageBackend, Version};

use crate::model::{Call, Expectation, Model, St, Ver, Who};

/// How a sequence ended when the engine and the model agreed all the way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunReport {
    /// Calls made.
    pub calls: usize,
    /// Calls the engine accepted.
    pub committed: usize,
    /// Calls the engine refused.
    pub refused: usize,
    /// Calls skipped because their slot was never created.
    pub skipped: usize,
}

/// The first place the engine and the model disagreed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The index of the call in the sequence.
    pub step: usize,
    /// The call.
    pub call: Call,
    /// What the model said.
    pub expected: String,
    /// What the engine did.
    pub actual: String,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "step {} {:?}: expected {}, engine {}",
            self.step, self.call, self.expected, self.actual
        )
    }
}

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("a name: {e:?}"))
}

fn date() -> Date {
    Date::from_calendar_date(2026, Month::January, 15).unwrap_or_else(|e| panic!("a date: {e:?}"))
}

/// The runner over one engine.
pub struct Runner<B: StorageBackend> {
    engine: Arc<Engine<B>>,
    world: Arc<World>,
    model: Model,
    tes: BTreeMap<u8, RecordId>,
    tasks: BTreeMap<u8, RecordId>,
    project: RecordId,
    user: RecordId,
    baseline_events: usize,
    expected_events: usize,
    report: RunReport,
}

impl<B: StorageBackend> Runner<B> {
    /// Prepares the store, seeds the user and the project, and starts with `model`.
    ///
    /// # Errors
    ///
    /// The engine's error when the store cannot be prepared or seeded.
    pub async fn new(world: Arc<World>, backend: B, model: Model) -> AppResult<Runner<B>> {
        let engine = Arc::new(world.engine(backend)?);
        engine.prepare().await?;
        let system = world.inv("system");
        let mut user = Document::new(n(USER), RecordId::new_v7());
        user.set(n("user_name"), Value::Text("l3-model-user".to_owned()));
        user.set(n("enabled"), Value::Bool(true));
        let user = engine.create(&system, user).await?.value.id();
        let mut project = Document::new(n(PROJECT), RecordId::new_v7());
        project.set(n("code"), Value::Text("PRJ-L3-MODEL".to_owned()));
        project.set(n("title"), Value::Text("L3 model".to_owned()));
        project.set(n("status"), Value::Text("Planning".to_owned()));
        project.set(n("owner"), Value::Id(user));
        let project = engine.create(&system, project).await?.value.id();
        let mut runner = Runner {
            engine,
            world,
            model,
            tes: BTreeMap::new(),
            tasks: BTreeMap::new(),
            project,
            user,
            baseline_events: 0,
            expected_events: 0,
            report: RunReport {
                calls: 0,
                committed: 0,
                refused: 0,
                skipped: 0,
            },
        };
        runner.baseline_events = runner
            .lifecycle_events()
            .await
            .map_err(|e| AppError::new(spot_app_api::ErrorClass::Internal, "l3_outbox", e))?;
        Ok(runner)
    }

    /// The engine, for a caller that wants to look at the store.
    pub fn engine(&self) -> &Arc<Engine<B>> {
        &self.engine
    }

    fn inv(&self, who: Who) -> Invocation {
        self.world.inv(match who {
            Who::Manager => "manager",
            Who::Viewer => "viewer",
        })
    }

    fn sent(&self, current: Option<u64>, ver: Ver) -> Version {
        let v = current.unwrap_or(1);
        Version(match ver {
            Ver::Current => v,
            Ver::Stale => v + 7,
        })
    }

    /// The version the model holds for the document `call` names, before the call.
    fn current_of(&self, call: &Call) -> Option<u64> {
        let te = |s: &u8| self.model.tes.get(s).map(|t| t.version);
        let task = |s: &u8| self.model.tasks.get(s).map(|t| t.version);
        match call {
            Call::CreateTask { .. } | Call::CreateTe { .. } => None,
            Call::SetHours { te: s, .. }
            | Call::Post { te: s, .. }
            | Call::Cancel { te: s, .. }
            | Call::Correct { te: s, .. }
            | Call::DeleteTe { te: s, .. } => te(s),
            Call::SetSubject { task: s, .. }
            | Call::RenameTask { task: s, .. }
            | Call::DeleteTask { task: s, .. } => task(s),
        }
    }

    fn id_of(&self, map: &BTreeMap<u8, RecordId>, slot: u8) -> RecordId {
        map.get(&slot).copied().unwrap_or_else(RecordId::new_v7)
    }

    async fn make(
        &self,
        call: &Call,
        current: Option<u64>,
    ) -> AppResult<Option<(u8, bool, RecordId)>> {
        // Returns the slot and id a successful call binds (a create or the draft of a correct).
        let ver = |v: Ver| self.sent(current, v);
        match *call {
            Call::CreateTask { slot } => {
                let mut doc = Document::new(n(TASK), RecordId::new_v7());
                doc.set(n("subject"), Value::Text(format!("task-created-{slot}")));
                doc.set(n("project"), Value::Id(self.project));
                doc.set(n("status"), Value::Text("Open".to_owned()));
                doc.set(n("priority"), Value::Text("Medium".to_owned()));
                let saved = self
                    .engine
                    .create(&self.inv(Who::Manager), doc)
                    .await?
                    .value;
                Ok(Some((slot, true, saved.id())))
            }
            Call::CreateTe { slot, task } => {
                let mut doc = Document::new(n(TIME_ENTRY), RecordId::new_v7());
                doc.set(n("entry_no"), Value::Text(format!("TE-L3-{}", doc.id())));
                doc.set(n("task"), Value::Id(self.id_of(&self.tasks, task)));
                doc.set(n("person"), Value::Id(self.user));
                doc.set(n("work_date"), Value::Date(date()));
                doc.set(n("hours"), Value::Decimal(Decimal::new(2, 0)));
                doc.set(n("billable"), Value::Bool(true));
                let saved = self
                    .engine
                    .create(&self.inv(Who::Manager), doc)
                    .await?
                    .value;
                Ok(Some((slot, false, saved.id())))
            }
            Call::SetHours {
                te,
                hours,
                ver: v,
                who,
            } => {
                let mut patch = DocumentPatch::default();
                patch.fields.insert(
                    n("hours"),
                    Value::Decimal(Decimal::new(i64::from(hours), 0)),
                );
                let id = self.id_of(&self.tes, te);
                self.engine
                    .update(&self.inv(who), TIME_ENTRY, id, patch, ver(v))
                    .await?;
                Ok(None)
            }
            Call::Post { te, ver: v, who } => {
                let id = self.id_of(&self.tes, te);
                self.engine
                    .post(&self.inv(who), TIME_ENTRY, id, ver(v))
                    .await?;
                Ok(None)
            }
            Call::Cancel { te, ver: v, who } => {
                let id = self.id_of(&self.tes, te);
                self.engine
                    .cancel(&self.inv(who), TIME_ENTRY, id, ver(v))
                    .await?;
                Ok(None)
            }
            Call::Correct {
                te,
                into,
                ver: v,
                who,
            } => {
                let id = self.id_of(&self.tes, te);
                let draft = self
                    .engine
                    .correct(&self.inv(who), TIME_ENTRY, id, ver(v))
                    .await?
                    .value;
                Ok(Some((into, false, draft.id())))
            }
            Call::DeleteTe { te, ver: v } => {
                let id = self.id_of(&self.tes, te);
                self.engine
                    .delete(&self.inv(Who::Manager), TIME_ENTRY, id, ver(v))
                    .await?;
                Ok(None)
            }
            Call::SetSubject {
                task,
                tag,
                ver: v,
                who,
            } => {
                let mut patch = DocumentPatch::default();
                patch
                    .fields
                    .insert(n("subject"), Value::Text(format!("subject-{tag}")));
                let id = self.id_of(&self.tasks, task);
                self.engine
                    .update(&self.inv(who), TASK, id, patch, ver(v))
                    .await?;
                Ok(None)
            }
            Call::RenameTask {
                task,
                tag,
                ver: v,
                who,
            } => {
                let id = self.id_of(&self.tasks, task);
                self.engine
                    .rename(&self.inv(who), TASK, id, &format!("renamed-{tag}"), ver(v))
                    .await?;
                Ok(None)
            }
            Call::DeleteTask { task, ver: v } => {
                let id = self.id_of(&self.tasks, task);
                self.engine
                    .delete(&self.inv(Who::Manager), TASK, id, ver(v))
                    .await?;
                Ok(None)
            }
        }
    }

    /// Makes one call and compares it, and then every document, with the model.
    ///
    /// # Errors
    ///
    /// The first [`Divergence`].
    pub async fn apply(&mut self, step: usize, call: &Call) -> Result<(), Divergence> {
        let current = self.current_of(call);
        let expectation = self.model.step(call);
        self.report.calls += 1;
        let diverge = |expected: String, actual: String| Divergence {
            step,
            call: *call,
            expected,
            actual,
        };
        match expectation {
            Expectation::Skip => self.report.skipped += 1,
            Expectation::Ok { events } => match self.make(call, current).await {
                Ok(bound) => {
                    self.report.committed += 1;
                    self.expected_events += events as usize;
                    if let Some((slot, is_task, id)) = bound {
                        if is_task {
                            self.tasks.insert(slot, id);
                        } else {
                            self.tes.insert(slot, id);
                        }
                    }
                }
                Err(e) => {
                    return Err(diverge(
                        "accepted".into(),
                        format!("refused {} ({:?}){}", e.code, e.class, field_codes(&e)),
                    ));
                }
            },
            Expectation::Refused(r) => match self.make(call, current).await {
                Ok(_) => return Err(diverge(format!("refused {r:?}"), "accepted".into())),
                Err(e) if r.accepts(e.class) => self.report.refused += 1,
                Err(e) => {
                    return Err(diverge(
                        format!("refused {r:?}"),
                        format!("refused {} ({:?}){}", e.code, e.class, field_codes(&e)),
                    ));
                }
            },
        }
        self.read_back(step, call).await
    }

    async fn load(&self, doctype: &str, id: RecordId) -> AppResult<Document> {
        Ok(self
            .engine
            .load(&self.inv(Who::Manager), doctype, id)
            .await?
            .value)
    }

    async fn read_back(&self, step: usize, call: &Call) -> Result<(), Divergence> {
        let bad = |what: String, expected: String, actual: String| Divergence {
            step,
            call: *call,
            expected: format!("{what}: {expected}"),
            actual,
        };
        for (slot, te) in &self.model.tes {
            let Some(id) = self.tes.get(slot) else {
                continue;
            };
            let what = format!("time entry {slot}");
            match (te.exists, self.load(TIME_ENTRY, *id).await) {
                (false, Err(e)) if e.class == spot_app_api::ErrorClass::NotFound => {}
                (false, other) => {
                    return Err(bad(
                        what,
                        "gone".into(),
                        format!("{:?}", other.map(|d| d.version())),
                    ));
                }
                (true, Err(e)) => {
                    return Err(bad(what, "present".into(), format!("error {}", e.code)));
                }
                (true, Ok(doc)) => {
                    let state = match te.state {
                        St::Draft => DocState::Draft,
                        St::Posted => DocState::Posted,
                        St::Cancelled => DocState::Cancelled,
                    };
                    if doc.version() != Some(Version(te.version)) {
                        return Err(bad(
                            what,
                            format!("version {}", te.version),
                            format!("{:?}", doc.version()),
                        ));
                    }
                    if doc.state() != Some(state) {
                        return Err(bad(
                            what,
                            format!("state {state:?}"),
                            format!("{:?}", doc.state()),
                        ));
                    }
                    let hours = Value::Decimal(Decimal::new(i64::from(te.hours), 0));
                    if *doc.get("hours") != hours {
                        return Err(bad(
                            what,
                            format!("hours {}", te.hours),
                            format!("{:?}", doc.get("hours")),
                        ));
                    }
                    let amended = doc.link("amended_from").ok().flatten();
                    let want = te.amended_from.and_then(|s| self.tes.get(&s).copied());
                    if amended != want {
                        return Err(bad(
                            what,
                            format!("amended_from {want:?}"),
                            format!("{amended:?}"),
                        ));
                    }
                }
            }
        }
        for (slot, task) in &self.model.tasks {
            let Some(id) = self.tasks.get(slot) else {
                continue;
            };
            let what = format!("task {slot}");
            match (task.exists, self.load(TASK, *id).await) {
                (false, Err(e)) if e.class == spot_app_api::ErrorClass::NotFound => {}
                (false, other) => {
                    return Err(bad(
                        what,
                        "gone".into(),
                        format!("{:?}", other.map(|d| d.version())),
                    ));
                }
                (true, Err(e)) => {
                    return Err(bad(what, "present".into(), format!("error {}", e.code)));
                }
                (true, Ok(doc)) => {
                    if doc.version() != Some(Version(task.version)) {
                        return Err(bad(
                            what,
                            format!("version {}", task.version),
                            format!("{:?}", doc.version()),
                        ));
                    }
                    let subject = task
                        .subject
                        .clone()
                        .unwrap_or_else(|| format!("task-created-{slot}"));
                    if doc.text("subject").ok().flatten() != Some(subject.as_str()) {
                        return Err(bad(
                            what,
                            format!("subject {subject}"),
                            format!("{:?}", doc.get("subject")),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    async fn lifecycle_events(&self) -> Result<usize, String> {
        let mut after = FeedPosition::START;
        let mut count = 0;
        loop {
            let page = self
                .engine
                .backend()
                .read_outbox(after, 5_000)
                .await
                .map_err(|e| e.to_string())?;
            let Some(last) = page.last() else { break };
            after = last.position;
            let full = page.len() >= 5_000;
            count += page
                .iter()
                .filter(|r| {
                    Operation::ALL
                        .iter()
                        .any(|o| o.topic_name() == r.event.topic.as_str())
                })
                .count();
            if !full {
                break;
            }
        }
        Ok(count)
    }

    /// The end-of-sequence check: one lifecycle event per committed write, each once.
    ///
    /// # Errors
    ///
    /// A [`Divergence`] at `step` when the count differs.
    pub async fn finish(self, step: usize) -> Result<RunReport, Divergence> {
        let actual = self.lifecycle_events().await.map_err(|e| Divergence {
            step,
            call: Call::CreateTask { slot: 0 },
            expected: "the outbox can be read".into(),
            actual: e,
        })?;
        let expected = self.baseline_events + self.expected_events;
        if actual != expected {
            return Err(Divergence {
                step,
                call: Call::CreateTask { slot: 0 },
                expected: format!("{expected} lifecycle events"),
                actual: format!("{actual} lifecycle events"),
            });
        }
        Ok(self.report)
    }
}

/// Runs `calls` on a fresh store from `backend` against `model`.
///
/// # Errors
///
/// The first [`Divergence`]. A store that cannot be prepared is a divergence at step 0.
pub async fn run_sequence<B: StorageBackend>(
    backend: B,
    calls: &[Call],
    model: Model,
) -> Result<RunReport, Divergence> {
    let world = Arc::new(World::new());
    let mut runner = Runner::new(world, backend, model)
        .await
        .map_err(|e| Divergence {
            step: 0,
            call: calls
                .first()
                .copied()
                .unwrap_or(Call::CreateTask { slot: 0 }),
            expected: "a prepared store".into(),
            actual: e.code.clone(),
        })?;
    for (i, call) in calls.iter().enumerate() {
        runner.apply(i, call).await?;
    }
    runner.finish(calls.len()).await
}

/// The refused fields of an error, as ` [field: code, ..]` (empty when it names none).
fn field_codes(e: &spot_app_api::AppError) -> String {
    if e.fields.is_empty() {
        return String::new();
    }
    let list: Vec<String> = e
        .fields
        .iter()
        .map(|f| format!("{:?}: {}", f.field, f.code))
        .collect();
    format!(" [{}]", list.join(", "))
}
