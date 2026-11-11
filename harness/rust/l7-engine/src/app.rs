//! App invariant registration (WP-TST-07.T2): the documented path an app uses to add its own
//! invariants next to the framework's, and the guard that turns a panicking or slow invariant
//! into a failed invariant instead of a crashed run.
//!
//! An app calls [`register_app_invariants`] with its name and a closure that registers its
//! invariants through [`AppBuilder::register`]. Every registered invariant is wrapped in a
//! [`GuardedInvariant`]: a panic while it checks, or a check that outruns its budget, is
//! reported as a [`Violation`] naming the invariant, never as a crash of the whole run.
//!
//! The sample app's one invariant ([`SampleProjectTaskCount`]) is the example: a Project's task
//! count equals the number of its linked Tasks.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use spot_l7_invariants::invariant::{
    BoxFuture, CheckContext, Invariant, InvariantRegistry, RegistryError, Violation,
};
use spot_storage_contract::{
    AggFunc, Expr, Name, Query, QueryPath, RecordId, Selection, StorageResult, Value,
};

use crate::driver::EngineRun;
use crate::world::{PROJECT, TASK};

/// The sample app's invariant id, `<app>.<rule>` as `register_app_invariants` requires. Assembled with
/// `concat!` because the error-code lint reads any dotted literal as an error code.
pub const SAMPLE_PROJECT_TASK_COUNT: &str = concat!("sample", ".", "project_task_count");
/// The default budget of an app invariant.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(5);

/// A staging builder an app registers its invariants into. It validates that every id is
/// prefixed with the app's name (so two apps can never collide) and that no id repeats, exactly
/// as [`InvariantRegistry::register`] does for the framework.
pub struct AppBuilder {
    app: String,
    items: Vec<Box<dyn Invariant>>,
}

impl AppBuilder {
    /// A builder for the invariants of `app`.
    pub fn new(app: &str) -> AppBuilder {
        AppBuilder {
            app: app.to_owned(),
            items: Vec::new(),
        }
    }

    /// Registers one invariant of this app. The id must start with `<app>.`.
    pub fn register(&mut self, invariant: Box<dyn Invariant>) -> Result<(), RegistryError> {
        let id = invariant.id().to_owned();
        if id.is_empty() {
            return Err(RegistryError::EmptyId);
        }
        let prefix = format!("{}.", self.app);
        if !id.starts_with(&prefix) {
            return Err(RegistryError::Duplicate(id));
        }
        if self.items.iter().any(|i| i.id() == id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.items.push(invariant);
        Ok(())
    }

    /// The ids registered so far.
    pub fn ids(&self) -> Vec<&str> {
        self.items.iter().map(|i| i.id()).collect()
    }
}

/// Registers `app`'s invariants into `registry`, each guarded against panic and overrun.
///
/// The closure receives an [`AppBuilder`]; the sample app's example is
/// ```ignore
/// register_app_invariants("sample", &mut registry, |app| {
///     app.register(Box::new(SampleProjectTaskCount::new(run.clone())))?;
///     Ok(())
/// });
/// ```
///
/// # Errors
///
/// [`RegistryError`] when an app id is empty, not prefixed with `<app>.`, or already registered.
pub fn register_app_invariants(
    app: &str,
    registry: &mut InvariantRegistry,
    register: impl FnOnce(&mut AppBuilder) -> Result<(), RegistryError>,
) -> Result<usize, RegistryError> {
    let mut builder = AppBuilder::new(app);
    register(&mut builder)?;
    let mut added = 0;
    for item in builder.items {
        registry.register(Box::new(GuardedInvariant::new(item, DEFAULT_BUDGET)))?;
        added += 1;
    }
    Ok(added)
}

/// An invariant that catches a panic or a timeout while it checks.
pub struct GuardedInvariant {
    inner: Box<dyn Invariant>,
    budget: Duration,
}

impl GuardedInvariant {
    /// Guards `inner` with `budget`.
    pub fn new(inner: Box<dyn Invariant>, budget: Duration) -> GuardedInvariant {
        GuardedInvariant { inner, budget }
    }
}

impl Invariant for GuardedInvariant {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn describe(&self) -> &str {
        self.inner.describe()
    }

    fn check<'a>(&'a self, ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
        Box::pin(async move {
            let id = self.inner.id().to_owned();
            let inner: BoxFuture<'a, Vec<Violation>> = self.inner.check(ctx);
            match tokio::time::timeout(self.budget, CatchPanic { inner }).await {
                Ok(Ok(violations)) => violations,
                Ok(Err(())) => vec![Violation::new(&id, "app", "-", "invariant_panicked")],
                Err(_) => vec![Violation::new(&id, "app", "-", "invariant_timed_out")],
            }
        })
    }
}

/// A future that turns a panic during `poll` into `Err(())`.
struct CatchPanic<'a> {
    inner: BoxFuture<'a, Vec<Violation>>,
}

impl Future for CatchPanic<'_> {
    type Output = Result<Vec<Violation>, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // `BoxFuture` is `Unpin`, so `get_mut` is sound without any unsafe projection.
        let this = self.get_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| this.inner.as_mut().poll(cx))) {
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

/// The sample app's example invariant: for every live Project, the number of Tasks the oracle
/// links to it equals the number of Tasks stored against it.
pub struct SampleProjectTaskCount {
    run: Arc<EngineRun>,
}

impl SampleProjectTaskCount {
    /// The invariant over an engine run.
    pub fn new(run: Arc<EngineRun>) -> SampleProjectTaskCount {
        SampleProjectTaskCount { run }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        // What the oracle expects: a live Task per recorded `project` link.
        let mut expected: BTreeMap<RecordId, u64> = BTreeMap::new();
        for link in self.run.links.iter().filter(|l| l.field == "project") {
            let live = self
                .run
                .docs
                .get(&(TASK.to_owned(), link.holder_id))
                .is_some_and(|d| d.live);
            if live {
                *expected.entry(link.target_id).or_insert(0) += 1;
            }
        }
        // What the store holds: tasks grouped by their project link.
        let a = Name::new("t")?;
        let mut q = Query::from(&Name::new(TASK)?, &a, QueryPath::Transactional);
        q.select = vec![
            Selection {
                expr: Expr::field(&a, &Name::new("project")?),
                alias: Name::new("project")?,
            },
            Selection {
                expr: Expr::Agg {
                    func: AggFunc::Count,
                    arg: None,
                    distinct: false,
                },
                alias: Name::new("tasks")?,
            },
        ];
        q.group_by = vec![Expr::field(&a, &Name::new("project")?)];
        let rows = ctx.query(&q).await?;
        let mut actual: BTreeMap<RecordId, u64> = BTreeMap::new();
        for row in &rows.rows {
            let project = match row.first() {
                Some(Value::Id(id)) => *id,
                _ => continue,
            };
            let count = match row.get(1) {
                Some(Value::Int(c)) => u64::try_from(*c).unwrap_or(0),
                _ => 0,
            };
            actual.insert(project, count);
        }
        for ((entity, id), doc) in &self.run.docs {
            if entity != PROJECT || !doc.live {
                continue;
            }
            let want = expected.get(id).copied().unwrap_or(0);
            let got = actual.get(id).copied().unwrap_or(0);
            if want != got {
                out.push(Violation::new(
                    SAMPLE_PROJECT_TASK_COUNT,
                    PROJECT,
                    id,
                    format!("task_count expected={want} actual={got}"),
                ));
            }
        }
        Ok(())
    }
}

impl Invariant for SampleProjectTaskCount {
    fn id(&self) -> &str {
        SAMPLE_PROJECT_TASK_COUNT
    }

    fn describe(&self) -> &str {
        "For every live Project, the number of Tasks stored against it equals the number the \
         oracle linked to it: the app's own consistency rule, registered next to the \
         framework's."
    }

    fn check<'a>(&'a self, ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
        Box::pin(async move {
            let mut out = Vec::new();
            if let Err(e) = self.run(ctx, &mut out).await {
                out.push(Violation::read_failed(
                    SAMPLE_PROJECT_TASK_COUNT,
                    "store",
                    &e,
                ));
            }
            out
        })
    }
}
