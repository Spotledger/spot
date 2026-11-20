//! Recording what happened to API and scheduled scripts: compile results, runs, disables.

use std::sync::{Mutex, MutexGuard};

use spot_app_api::prelude::OffsetDateTime;
use spot_app_api::{AppResult, BoxFuture};

use super::ScriptKind;
use crate::compile::CompileOutcome;

/// One run of an API or scheduled script.
///
/// The result code is `ok` for a run that finished, otherwise the stable error code (a
/// `script.*` engine code or a `serverscript.*` code). It never carries script text, arguments
/// or results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindRun {
    /// The script's name.
    pub script: String,
    /// Which kind of script ran.
    pub kind: ScriptKind,
    /// The result code: `ok`, or the error's code.
    pub code: String,
    /// Whether the run finished without an error.
    pub ok: bool,
    /// How long the run took, in milliseconds.
    pub duration_ms: u64,
    /// How many host calls the run made.
    pub host_calls: u32,
    /// When the run finished (the injected clock).
    pub at: OffsetDateTime,
    /// The user the script ran as, as the record id text.
    pub user: String,
    /// The last message the script logged with `host.log`, when it logged one.
    pub message: Option<String>,
}

/// Where compile results, runs and disables of the two new kinds go. A host implements it over
/// the documents (as the system principal, through the audited path); [`FakeKindStore`] serves
/// tests.
pub trait KindStore: Send + Sync {
    /// Records the result of compiling `name`.
    fn record_compile<'a>(
        &'a self,
        kind: ScriptKind,
        name: &'a str,
        outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>>;

    /// Records one finished run (the run count and, on a failure, the last error).
    fn record_run<'a>(&'a self, run: KindRun) -> BoxFuture<'a, AppResult<()>>;

    /// Disables `name` with a visible reason.
    fn disable<'a>(
        &'a self,
        kind: ScriptKind,
        name: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>>;
}

/// An in-memory [`KindStore`] for tests.
#[derive(Default)]
pub struct FakeKindStore {
    compiles: Mutex<Vec<(ScriptKind, String, CompileOutcome)>>,
    runs: Mutex<Vec<KindRun>>,
    disabled: Mutex<Vec<(ScriptKind, String, String)>>,
}

impl FakeKindStore {
    /// An empty store.
    pub fn new() -> FakeKindStore {
        FakeKindStore::default()
    }

    /// Every compile result recorded, in order.
    pub fn compiles(&self) -> Vec<(ScriptKind, String, CompileOutcome)> {
        lock(&self.compiles).clone()
    }

    /// Every run recorded, in order.
    pub fn runs(&self) -> Vec<KindRun> {
        lock(&self.runs).clone()
    }

    /// Every disable recorded, as `(kind, name, reason)`, in order.
    pub fn disabled(&self) -> Vec<(ScriptKind, String, String)> {
        lock(&self.disabled).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl KindStore for FakeKindStore {
    fn record_compile<'a>(
        &'a self,
        kind: ScriptKind,
        name: &'a str,
        outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>> {
        lock(&self.compiles).push((kind, name.to_owned(), outcome));
        Box::pin(async move { Ok(()) })
    }

    fn record_run<'a>(&'a self, run: KindRun) -> BoxFuture<'a, AppResult<()>> {
        lock(&self.runs).push(run);
        Box::pin(async move { Ok(()) })
    }

    fn disable<'a>(
        &'a self,
        kind: ScriptKind,
        name: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>> {
        lock(&self.disabled).push((kind, name.to_owned(), reason.to_owned()));
        Box::pin(async move { Ok(()) })
    }
}
