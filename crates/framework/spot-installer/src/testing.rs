//! In-memory fakes and the backend-independent contract suite (feature `testing`).
//!
//! The fakes implement every executor trait, record every call with its [`SiteId`], and can be
//! told to fail at any step and any kind of call ([`Schedule::fail_at`]). [`Harness`] wires them
//! to an [`Executor`], an in-process coordinator and a manual clock, so the integration tests
//! prove the executor's logic — including every failure point — without a database, a wasm host
//! or a real clock.
//!
//! A database implementation of [`AppRegistryStore`] reuses [`contract::run_all`].

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use semver::Version;
use spot_bundle::Digest;
use spot_core::SiteId;

use crate::error::STEP_FAILED;
use crate::journal::{InstallJournal, LoadedComponent, SiteLifecycle, SnapshotId, StepState};
use crate::model::{InstalledApp, SiteState};
use crate::outcome::ExecError;
use crate::traits::{
    AppRegistryStore, BoxFuture, ComponentHost, FixtureLoader, InstallChange, LifecycleConflict,
    LifecycleError, PatchOutcome, PatchRunner, Snapshotter, UpgradeChange,
};

pub mod contract;

/// A future that is pending once and then ready: a deterministic cancellation point, so a test
/// can drop the executor's future at each journal write without a real clock.
#[derive(Debug)]
pub struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// A [`YieldOnce`] future.
#[must_use]
pub fn yield_once() -> YieldOnce {
    YieldOnce(false)
}

/// A boxed future that yields once (a cancellation point) before returning `value`.
fn respond<T: Send + 'static>(value: T) -> BoxFuture<'static, T> {
    Box::pin(async move {
        yield_once().await;
        value
    })
}

/// One recorded trait call: the site it was made for and the method's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The site the call named.
    pub site: String,
    /// The method that was called.
    pub method: &'static str,
}

/// The patch ids applied per `(site, app)`.
type PatchMap = BTreeMap<(String, String), BTreeSet<String>>;

/// The fixture files loaded per `(site, app)`.
type FixtureMap = BTreeMap<(String, String), BTreeSet<String>>;

/// Which call at a step a [`Schedule`] fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FailKind {
    /// A journal write (`record`, or a `commit_*`/`restore` journal row).
    Journal,
    /// `AppRegistryStore::commit_install`/`commit_upgrade`/`commit_uninstall`/`restore`.
    Commit,
    /// `AppRegistryStore::owned_doctypes`.
    Owned,
    /// `PatchRunner::apply`.
    Patch,
    /// `PatchRunner::undo`.
    Undo,
    /// `FixtureLoader::load`.
    Fixture,
    /// `FixtureLoader::remove`.
    Remove,
    /// `ComponentHost::load`.
    Load,
    /// `ComponentHost::unload`.
    Unload,
    /// `ComponentHost::smoke_test`.
    Smoke,
    /// `Snapshotter::take`.
    Snapshot,
    /// `Snapshotter::restore`.
    Restore,
}

/// The failure-injection schedule and call log the fakes share.
///
/// The executor journals each step before it runs it, so a fake learns the current step from the
/// journal row the registry wrote; a call that follows is checked against `(step, kind)`. This is
/// why the tests can fail at *every* step without hand-picking call counts.
#[derive(Clone, Debug, Default)]
pub struct Schedule {
    inner: Arc<Mutex<Sched>>,
}

#[derive(Debug, Default)]
struct Sched {
    failures: BTreeMap<(Option<String>, u32, FailKind), String>,
    current: BTreeMap<String, u32>,
    calls: Vec<Call>,
}

impl Schedule {
    /// A fresh schedule.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fails the `kind` call that runs at step `step` for every site.
    pub fn fail_at(&self, step: u32, kind: FailKind) {
        self.fail_at_site_inner(None, step, kind);
    }

    /// Fails the `kind` call that runs at step `step` for one site only.
    pub fn fail_at_site(&self, site: &SiteId, step: u32, kind: FailKind) {
        self.fail_at_site_inner(Some(site.as_str().to_owned()), step, kind);
    }

    fn fail_at_site_inner(&self, site: Option<String>, step: u32, kind: FailKind) {
        let message = match &site {
            Some(site) => format!("injected {kind:?} failure at step {step} on {site}"),
            None => format!("injected {kind:?} failure at step {step}"),
        };
        self.lock().failures.insert((site, step, kind), message);
    }

    /// Clears every injected failure.
    pub fn clear_failures(&self) {
        self.lock().failures.clear();
    }

    /// The step the executor is on for `site`, from the last journal row it wrote.
    #[must_use]
    pub fn current_step(&self, site: &SiteId) -> u32 {
        self.lock().current.get(site.as_str()).copied().unwrap_or(0)
    }

    /// Every call made so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The calls made for one site.
    #[must_use]
    pub fn calls_for(&self, site: &SiteId) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.site == site.as_str())
            .collect()
    }

    /// The distinct sites any call named.
    #[must_use]
    pub fn sites_called(&self) -> BTreeSet<String> {
        self.lock()
            .calls
            .iter()
            .map(|call| call.site.clone())
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Sched> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn log(&self, site: &SiteId, method: &'static str) {
        self.lock().calls.push(Call {
            site: site.as_str().to_owned(),
            method,
        });
    }

    fn set_current(&self, site: &SiteId, step: u32) {
        self.lock().current.insert(site.as_str().to_owned(), step);
    }

    fn check_step(&self, site: &SiteId, step: u32, kind: FailKind) -> Result<(), ExecError> {
        let key = Some(site.as_str().to_owned());
        // A failure fires once: the rollback re-runs the same trait method at the same step
        // index, and it must not fail again.
        let message = {
            let mut sched = self.lock();
            sched
                .failures
                .remove(&(key, step, kind))
                .or_else(|| sched.failures.remove(&(None, step, kind)))
        };
        match message {
            Some(message) => Err(ExecError::new(STEP_FAILED, message)),
            None => Ok(()),
        }
    }

    fn check_current(&self, site: &SiteId, kind: FailKind) -> Result<(), ExecError> {
        let step = self.current_step(site);
        self.check_step(site, step, kind)
    }
}

/// One site's registry state.
#[derive(Clone, Debug, Default)]
struct SiteRecord {
    state: SiteState,
    lifecycle: Option<SiteLifecycle>,
    doctypes: BTreeMap<String, Vec<String>>,
}

/// The in-memory [`AppRegistryStore`].
#[derive(Clone, Debug)]
pub struct FakeRegistry {
    sites: Arc<Mutex<BTreeMap<String, SiteRecord>>>,
    journal: Arc<Mutex<BTreeMap<String, Vec<InstallJournal>>>>,
    purged: Arc<Mutex<BTreeSet<(String, String)>>>,
    schedule: Schedule,
}

impl FakeRegistry {
    /// A registry over `schedule`.
    #[must_use]
    pub fn new(schedule: Schedule) -> Self {
        Self {
            sites: Arc::new(Mutex::new(BTreeMap::new())),
            journal: Arc::new(Mutex::new(BTreeMap::new())),
            purged: Arc::new(Mutex::new(BTreeSet::new())),
            schedule,
        }
    }

    /// Seeds the apps installed on a site before a run.
    pub fn seed(&self, site: &SiteId, state: SiteState) {
        self.with_site(site, |record| record.state = state);
    }

    /// Records the DocTypes an app owns, for the uninstall hook.
    pub fn set_owned_doctypes(&self, site: &SiteId, app: &str, doctypes: &[&str]) {
        self.with_site(site, |record| {
            record.doctypes.insert(
                app.to_owned(),
                doctypes.iter().map(|d| (*d).to_owned()).collect(),
            );
        });
    }

    /// The apps installed on a site, for assertions.
    #[must_use]
    pub fn state(&self, site: &SiteId) -> SiteState {
        self.lock_sites()
            .get(site.as_str())
            .map(|record| record.state.clone())
            .unwrap_or_default()
    }

    /// The site's lifecycle flag, for assertions.
    #[must_use]
    pub fn lifecycle_of(&self, site: &SiteId) -> Option<SiteLifecycle> {
        self.lock_sites()
            .get(site.as_str())
            .and_then(|record| record.lifecycle)
    }

    /// The journal rows for a site, for assertions.
    #[must_use]
    pub fn rows(&self, site: &SiteId) -> Vec<InstallJournal> {
        self.lock_journal()
            .get(site.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// Whether an uninstall for `app` deleted its data.
    #[must_use]
    pub fn was_purged(&self, site: &SiteId, app: &str) -> bool {
        self.lock_purged()
            .contains(&(site.as_str().to_owned(), app.to_owned()))
    }

    fn lock_sites(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, SiteRecord>> {
        self.sites
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_journal(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<InstallJournal>>> {
        self.journal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_purged(&self) -> std::sync::MutexGuard<'_, BTreeSet<(String, String)>> {
        self.purged
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_site<F: FnOnce(&mut SiteRecord)>(&self, site: &SiteId, edit: F) {
        let mut guard = self.lock_sites();
        let entry = guard.entry(site.as_str().to_owned()).or_default();
        edit(entry);
    }

    fn write_journal(&self, site: &SiteId, row: InstallJournal) {
        let mut guard = self.lock_journal();
        let rows = guard.entry(site.as_str().to_owned()).or_default();
        if let Some(existing) = rows.iter_mut().find(|r| r.step.index == row.step.index) {
            *existing = row;
        } else {
            rows.push(row);
        }
    }
}

impl AppRegistryStore for FakeRegistry {
    fn load(&self, site: &SiteId) -> BoxFuture<'_, Result<SiteState, ExecError>> {
        self.schedule.log(site, "load");
        let state = self.state(site);
        respond(Ok(state))
    }

    fn lifecycle(&self, site: &SiteId) -> BoxFuture<'_, Result<SiteLifecycle, ExecError>> {
        self.schedule.log(site, "lifecycle");
        let state = self.lifecycle_of(site).unwrap_or(SiteLifecycle::Idle);
        respond(Ok(state))
    }

    fn set_lifecycle(
        &self,
        site: &SiteId,
        from: SiteLifecycle,
        to: SiteLifecycle,
    ) -> BoxFuture<'_, Result<(), LifecycleError>> {
        self.schedule.log(site, "set_lifecycle");
        // The compare and the write happen under one lock, as the database's single `UPDATE`
        // does; the yield comes after, so a racing caller sees the new flag.
        let mut moved = Ok(());
        self.with_site(site, |record| {
            let stored = record.lifecycle.unwrap_or(SiteLifecycle::Idle);
            if stored == from {
                record.lifecycle = Some(to);
            } else {
                moved = Err(LifecycleError::Conflict(LifecycleConflict {
                    stored,
                    expected: from,
                    target: to,
                }));
            }
        });
        respond(moved)
    }

    fn commit_install(
        &self,
        site: &SiteId,
        app: &str,
        change: InstallChange,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "commit_install");
        let result = self
            .schedule
            .check_step(site, journal.step.index, FailKind::Commit);
        if result.is_ok() {
            self.with_site(site, |record| {
                record.state.apps.retain(|installed| installed.name != app);
                record.state.apps.push(InstalledApp {
                    name: app.to_owned(),
                    version: change.version,
                    digest: change.digest,
                    approved: change.approved,
                    applied_patches: change.applied_patches,
                    order: u32::try_from(record.state.apps.len()).unwrap_or(u32::MAX),
                    depends: Vec::new(),
                });
            });
            self.write_journal(site, journal);
        }
        respond(result)
    }

    fn commit_upgrade(
        &self,
        site: &SiteId,
        app: &str,
        change: UpgradeChange,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "commit_upgrade");
        let failure = self
            .schedule
            .check_step(site, journal.step.index, FailKind::Commit);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        let mut conflict = None;
        self.with_site(site, |record| {
            match record
                .state
                .apps
                .iter_mut()
                .find(|installed| installed.name == app)
            {
                Some(installed) if installed.version == change.from => {
                    installed.version = change.to;
                    installed.digest = change.digest;
                    installed.approved = change.approved;
                    installed.applied_patches = change.applied_patches;
                }
                Some(installed) => {
                    conflict = Some(installed.version.clone());
                }
                None => conflict = Some(Version::new(0, 0, 0)),
            }
        });
        match conflict {
            Some(found) => {
                let message = format!(
                    "`{app}` is at {found}, not {}; the commit lost the race",
                    change.from
                );
                respond(Err(ExecError::new(STEP_FAILED, message)))
            }
            None => {
                self.write_journal(site, journal);
                respond(Ok(()))
            }
        }
    }

    fn commit_uninstall(
        &self,
        site: &SiteId,
        app: &str,
        purge: bool,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "commit_uninstall");
        let failure = self
            .schedule
            .check_step(site, journal.step.index, FailKind::Commit);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        let mut present = false;
        self.with_site(site, |record| {
            let before = record.state.apps.len();
            record.state.apps.retain(|installed| installed.name != app);
            present = record.state.apps.len() != before;
        });
        if !present {
            let message = format!("`{app}` is not installed, so it cannot be uninstalled");
            return respond(Err(ExecError::new(STEP_FAILED, message)));
        }
        if purge {
            self.lock_purged()
                .insert((site.as_str().to_owned(), app.to_owned()));
        }
        self.write_journal(site, journal);
        respond(Ok(()))
    }

    fn restore(
        &self,
        site: &SiteId,
        app: &InstalledApp,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "restore");
        let result = self
            .schedule
            .check_step(site, journal.step.index, FailKind::Commit);
        if result.is_ok() {
            self.with_site(site, |record| {
                record
                    .state
                    .apps
                    .retain(|installed| installed.name != app.name);
                record.state.apps.push(app.clone());
            });
            self.write_journal(site, journal);
        }
        respond(result)
    }

    fn record(
        &self,
        site: &SiteId,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "record");
        self.schedule.set_current(site, journal.step.index);
        let result = self
            .schedule
            .check_step(site, journal.step.index, FailKind::Journal);
        if result.is_ok() {
            self.write_journal(site, journal);
        }
        respond(result)
    }

    fn journal(&self, site: &SiteId) -> BoxFuture<'_, Result<Vec<InstallJournal>, ExecError>> {
        self.schedule.log(site, "journal");
        let rows = self.rows(site);
        respond(Ok(rows))
    }

    fn clear_journal(&self, site: &SiteId) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "clear_journal");
        self.lock_journal().remove(site.as_str());
        respond(Ok(()))
    }

    fn owned_doctypes(
        &self,
        site: &SiteId,
        app: &str,
    ) -> BoxFuture<'_, Result<Vec<String>, ExecError>> {
        self.schedule.log(site, "owned_doctypes");
        let failure = self.schedule.check_current(site, FailKind::Owned);
        let doctypes = self
            .lock_sites()
            .get(site.as_str())
            .and_then(|record| record.doctypes.get(app).cloned())
            .unwrap_or_default();
        respond(failure.map(|()| doctypes))
    }
}

/// The in-memory [`PatchRunner`].
#[derive(Clone, Debug)]
pub struct FakePatches {
    applied: Arc<Mutex<PatchMap>>,
    schedule: Schedule,
}

impl FakePatches {
    /// A patch runner over `schedule`.
    #[must_use]
    pub fn new(schedule: Schedule) -> Self {
        Self {
            applied: Arc::new(Mutex::new(BTreeMap::new())),
            schedule,
        }
    }

    /// The patches applied to an app, for assertions.
    #[must_use]
    pub fn applied(&self, site: &SiteId, app: &str) -> BTreeSet<String> {
        self.lock()
            .get(&(site.as_str().to_owned(), app.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PatchMap> {
        self.applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl PatchRunner for FakePatches {
    fn apply(
        &self,
        site: &SiteId,
        app: &str,
        patch_id: &str,
    ) -> BoxFuture<'_, Result<PatchOutcome, ExecError>> {
        self.schedule.log(site, "patch_apply");
        let failure = self.schedule.check_current(site, FailKind::Patch);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        let key = (site.as_str().to_owned(), app.to_owned());
        let mut guard = self.lock();
        let set = guard.entry(key).or_default();
        let outcome = if set.insert(patch_id.to_owned()) {
            PatchOutcome::Applied
        } else {
            PatchOutcome::AlreadyApplied
        };
        respond(Ok(outcome))
    }

    fn undo(
        &self,
        site: &SiteId,
        app: &str,
        patch_id: &str,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "patch_undo");
        let failure = self.schedule.check_current(site, FailKind::Undo);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        let key = (site.as_str().to_owned(), app.to_owned());
        self.lock().entry(key).or_default().remove(patch_id);
        respond(Ok(()))
    }

    fn is_applied(
        &self,
        site: &SiteId,
        app: &str,
        patch_id: &str,
    ) -> BoxFuture<'_, Result<bool, ExecError>> {
        self.schedule.log(site, "patch_is_applied");
        let key = (site.as_str().to_owned(), app.to_owned());
        let applied = self
            .lock()
            .get(&key)
            .is_some_and(|set| set.contains(patch_id));
        respond(Ok(applied))
    }
}

/// The in-memory [`ComponentHost`].
#[derive(Clone, Debug)]
pub struct FakeComponents {
    serving: Arc<Mutex<BTreeMap<(String, String), LoadedComponent>>>,
    schedule: Schedule,
}

impl FakeComponents {
    /// A component host over `schedule`.
    #[must_use]
    pub fn new(schedule: Schedule) -> Self {
        Self {
            serving: Arc::new(Mutex::new(BTreeMap::new())),
            schedule,
        }
    }

    /// Seeds the component serving for an app before a run.
    pub fn seed(&self, site: &SiteId, app: &str, component: LoadedComponent) {
        self.lock()
            .insert((site.as_str().to_owned(), app.to_owned()), component);
    }

    /// The component serving for an app, for assertions.
    #[must_use]
    pub fn serving(&self, site: &SiteId, app: &str) -> Option<LoadedComponent> {
        self.lock()
            .get(&(site.as_str().to_owned(), app.to_owned()))
            .cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), LoadedComponent>> {
        self.serving
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ComponentHost for FakeComponents {
    fn load(
        &self,
        site: &SiteId,
        app: &str,
        version: &Version,
        digest: Digest,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "component_load");
        let failure = self.schedule.check_current(site, FailKind::Load);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        self.lock().insert(
            (site.as_str().to_owned(), app.to_owned()),
            LoadedComponent::new(version.clone(), digest),
        );
        respond(Ok(()))
    }

    fn unload(&self, site: &SiteId, app: &str) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "component_unload");
        let failure = self.schedule.check_current(site, FailKind::Unload);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        self.lock()
            .remove(&(site.as_str().to_owned(), app.to_owned()));
        respond(Ok(()))
    }

    fn smoke_test(&self, site: &SiteId, app: &str) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "component_smoke");
        let failure = self.schedule.check_current(site, FailKind::Smoke);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        let serving = self
            .lock()
            .contains_key(&(site.as_str().to_owned(), app.to_owned()));
        let message = if serving {
            None
        } else {
            Some(format!("`{app}` has no component serving"))
        };
        respond(match message {
            None => Ok(()),
            Some(message) => Err(ExecError::new(STEP_FAILED, message)),
        })
    }
}

/// The in-memory [`FixtureLoader`].
#[derive(Clone, Debug)]
pub struct FakeFixtures {
    loaded: Arc<Mutex<FixtureMap>>,
    schedule: Schedule,
}

impl FakeFixtures {
    /// A fixture loader over `schedule`.
    #[must_use]
    pub fn new(schedule: Schedule) -> Self {
        Self {
            loaded: Arc::new(Mutex::new(BTreeMap::new())),
            schedule,
        }
    }

    /// Seeds the fixtures an app already loaded before a run.
    pub fn seed(&self, site: &SiteId, app: &str, fixtures: &[&str]) {
        let mut guard = self.lock();
        let set = guard
            .entry((site.as_str().to_owned(), app.to_owned()))
            .or_default();
        for fixture in fixtures {
            set.insert((*fixture).to_owned());
        }
    }

    /// The fixtures loaded for an app, for assertions.
    #[must_use]
    pub fn loaded(&self, site: &SiteId, app: &str) -> BTreeSet<String> {
        self.lock()
            .get(&(site.as_str().to_owned(), app.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FixtureMap> {
        self.loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl FixtureLoader for FakeFixtures {
    fn load(
        &self,
        site: &SiteId,
        app: &str,
        fixture: &str,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "fixture_load");
        let failure = self.schedule.check_current(site, FailKind::Fixture);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        self.lock()
            .entry((site.as_str().to_owned(), app.to_owned()))
            .or_default()
            .insert(fixture.to_owned());
        respond(Ok(()))
    }

    fn remove(
        &self,
        site: &SiteId,
        app: &str,
        fixture: &str,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "fixture_remove");
        let failure = self.schedule.check_current(site, FailKind::Remove);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        self.lock()
            .entry((site.as_str().to_owned(), app.to_owned()))
            .or_default()
            .remove(fixture);
        respond(Ok(()))
    }

    fn owned(&self, site: &SiteId, app: &str) -> BoxFuture<'_, Result<Vec<String>, ExecError>> {
        self.schedule.log(site, "fixture_owned");
        let owned = self.loaded(site, app).into_iter().collect();
        respond(Ok(owned))
    }
}

/// The in-memory [`Snapshotter`].
#[derive(Clone, Debug)]
pub struct FakeSnapshots {
    taken: Arc<Mutex<Vec<(String, SnapshotId)>>>,
    restored: Arc<Mutex<Vec<(String, SnapshotId)>>>,
    counter: Arc<AtomicU64>,
    schedule: Schedule,
}

impl FakeSnapshots {
    /// A snapshotter over `schedule`.
    #[must_use]
    pub fn new(schedule: Schedule) -> Self {
        Self {
            taken: Arc::new(Mutex::new(Vec::new())),
            restored: Arc::new(Mutex::new(Vec::new())),
            counter: Arc::new(AtomicU64::new(0)),
            schedule,
        }
    }

    /// How many snapshots were taken for a site.
    #[must_use]
    pub fn taken_count(&self, site: &SiteId) -> usize {
        self.lock_taken()
            .iter()
            .filter(|(called, _)| called == site.as_str())
            .count()
    }

    /// How many snapshots were restored for a site.
    #[must_use]
    pub fn restored_count(&self, site: &SiteId) -> usize {
        self.lock_restored()
            .iter()
            .filter(|(called, _)| called == site.as_str())
            .count()
    }

    fn lock_taken(&self) -> std::sync::MutexGuard<'_, Vec<(String, SnapshotId)>> {
        self.taken
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_restored(&self) -> std::sync::MutexGuard<'_, Vec<(String, SnapshotId)>> {
        self.restored
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Snapshotter for FakeSnapshots {
    fn take(&self, site: &SiteId) -> BoxFuture<'_, Result<SnapshotId, ExecError>> {
        self.schedule.log(site, "snapshot_take");
        let failure = self.schedule.check_current(site, FailKind::Snapshot);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        let next = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let id = SnapshotId::new(format!("snap-{next}"));
        self.lock_taken()
            .push((site.as_str().to_owned(), id.clone()));
        respond(Ok(id))
    }

    fn restore(&self, site: &SiteId, id: &SnapshotId) -> BoxFuture<'_, Result<(), ExecError>> {
        self.schedule.log(site, "snapshot_restore");
        let failure = self.schedule.check_current(site, FailKind::Restore);
        if let Err(error) = failure {
            return respond(Err(error));
        }
        self.lock_restored()
            .push((site.as_str().to_owned(), id.clone()));
        respond(Ok(()))
    }
}

/// The number of steps a run journalled as done.
#[must_use]
pub fn done_rows(rows: &[InstallJournal]) -> usize {
    rows.iter()
        .filter(|row| row.state == StepState::Done)
        .count()
}
