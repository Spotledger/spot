//! The backend-independent [`AppRegistryStore`] contract suite (feature `testing`).
//!
//! Every registry store must pass [`run_all`]: the in-memory fake here (`tests/contract.rs`) and
//! the database implementation a later task writes. The suite proves the promises the executor
//! relies on: one call is one transaction, the journal row is written with the change it
//! describes, a refused commit leaves neither, and the journal is keyed by step.
//!
//! ```ignore
//! let report = run_all(|| async { Arc::new(MyStore::new()) as Arc<dyn AppRegistryStore> }).await;
//! report.assert_all_passed();
//! ```

use std::future::Future;
use std::sync::Arc;

use semver::Version;
use spot_bundle::Digest;
use spot_core::SiteId;

use crate::journal::{InstallJournal, SiteLifecycle, StepId, StepKind, StepState};
use crate::model::{GrantedCapabilities, InstalledApp};
use crate::outcome::ExecError;
use crate::traits::{
    AppRegistryStore, InstallChange, LifecycleConflict, LifecycleError, UpgradeChange,
};

/// How one case ended: `Err` carries what was wrong.
pub type CheckResult = Result<(), String>;

/// One case's name and outcome.
#[derive(Debug)]
pub struct CaseResult {
    /// The case's name.
    pub name: &'static str,
    /// `Ok` when the store behaved.
    pub outcome: CheckResult,
}

/// The outcome of [`run_all`].
#[derive(Debug, Default)]
pub struct Report {
    /// One entry per case, in run order.
    pub cases: Vec<CaseResult>,
}

impl Report {
    /// The cases that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&CaseResult> {
        self.cases.iter().filter(|c| c.outcome.is_err()).collect()
    }

    /// Panics, listing every failed case, unless all passed (and at least one ran).
    ///
    /// # Panics
    /// Panics when a case failed or none ran.
    pub fn assert_all_passed(&self) {
        assert!(!self.cases.is_empty(), "the contract suite ran no cases");
        let failures = self.failures();
        if failures.is_empty() {
            return;
        }
        let mut text = format!(
            "{} of {} registry contract cases failed:",
            failures.len(),
            self.cases.len()
        );
        for case in failures {
            if let Err(why) = &case.outcome {
                text.push_str(&format!("\n  - {}: {why}", case.name));
            }
        }
        panic!("{text}");
    }
}

/// Runs every case, each against a fresh store from `make`.
pub async fn run_all<F, Fut>(make: F) -> Report
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn AppRegistryStore>>,
{
    let mut report = Report::default();
    macro_rules! case {
        ($case:path) => {{
            let outcome = $case(make().await).await;
            report.cases.push(CaseResult {
                name: stringify!($case),
                outcome,
            });
        }};
    }
    case!(install_is_atomic_and_journalled);
    case!(a_refused_upgrade_leaves_nothing);
    case!(uninstall_then_restore_round_trips);
    case!(journal_rows_are_unique_by_step);
    case!(clear_journal_empties_it);
    case!(lifecycle_round_trips);
    case!(a_lifecycle_move_from_the_wrong_state_changes_nothing);
    case!(racing_idle_to_installing_lets_exactly_one_win);
    case!(owned_doctypes_is_readable);
    report
}

/// A site id for a case.
fn site() -> Result<SiteId, String> {
    SiteId::new("contract-site").map_err(|error| error.to_string())
}

/// A digest whose 32 bytes are all `byte`.
fn digest(byte: u8) -> Result<Digest, String> {
    format!("{byte:02x}")
        .repeat(32)
        .parse()
        .map_err(|error: spot_bundle::DigestFromStrError| error.to_string())
}

/// A journal row for a step.
fn row(app: &str, index: u32, state: StepState) -> InstallJournal {
    InstallJournal::new(
        "plan-hash",
        StepId::new(index, StepKind::Commit, app),
        state,
        0,
        None,
    )
}

/// An install change.
fn install_change(byte: u8) -> Result<InstallChange, String> {
    Ok(InstallChange {
        version: Version::new(1, 0, 0),
        digest: digest(byte)?,
        approved: GrantedCapabilities::default(),
        applied_patches: Vec::new(),
    })
}

/// An installed app at `version`.
fn installed(app: &str, byte: u8) -> Result<InstalledApp, String> {
    Ok(InstalledApp::new(app, Version::new(1, 0, 0), digest(byte)?))
}

/// The apps installed on a store's site, as names.
async fn apps(store: &Arc<dyn AppRegistryStore>, site: &SiteId) -> Result<Vec<String>, String> {
    let state = store.load(site).await.map_err(|e| e.message)?;
    Ok(state.apps.iter().map(|app| app.name.clone()).collect())
}

/// The journal rows on a store's site.
async fn rows(
    store: &Arc<dyn AppRegistryStore>,
    site: &SiteId,
) -> Result<Vec<InstallJournal>, String> {
    store.journal(site).await.map_err(|e| e.message)
}

/// A commit adds the app and its journal row together.
async fn install_is_atomic_and_journalled(store: Arc<dyn AppRegistryStore>) -> CheckResult {
    let site = site()?;
    let change = install_change(1)?;
    store
        .commit_install(&site, "a", change, row("a", 0, StepState::Done))
        .await
        .map_err(|e| e.message)?;
    if apps(&store, &site).await? != vec!["a".to_owned()] {
        return Err("the app was not installed".to_owned());
    }
    let rows = rows(&store, &site).await?;
    if rows.len() != 1 || rows[0].step.name != "a" {
        return Err(format!(
            "the journal row was not written with the commit: {rows:?}"
        ));
    }
    Ok(())
}

/// A commit that loses the `from` race changes nothing and journals nothing.
async fn a_refused_upgrade_leaves_nothing(store: Arc<dyn AppRegistryStore>) -> CheckResult {
    let site = site()?;
    store
        .commit_install(&site, "a", install_change(1)?, row("a", 0, StepState::Done))
        .await
        .map_err(|e| e.message)?;
    let refused = store
        .commit_upgrade(
            &site,
            "a",
            UpgradeChange {
                from: Version::new(2, 0, 0),
                to: Version::new(3, 0, 0),
                digest: digest(3)?,
                approved: GrantedCapabilities::default(),
                applied_patches: Vec::new(),
            },
            row("a", 1, StepState::Done),
        )
        .await;
    if refused.is_ok() {
        return Err("an upgrade from the wrong version was accepted".to_owned());
    }
    let state = store.load(&site).await.map_err(|e| e.message)?;
    let version = state
        .get("a")
        .map(|app| app.version.clone())
        .ok_or_else(|| "the installed app vanished".to_owned())?;
    if version != Version::new(1, 0, 0) {
        return Err(format!(
            "the refused commit changed the version to {version}"
        ));
    }
    if rows(&store, &site).await?.len() != 1 {
        return Err("the refused commit wrote a journal row".to_owned());
    }
    Ok(())
}

/// Uninstall removes the row, and restore puts it back exactly.
async fn uninstall_then_restore_round_trips(store: Arc<dyn AppRegistryStore>) -> CheckResult {
    let site = site()?;
    store
        .commit_install(&site, "a", install_change(1)?, row("a", 0, StepState::Done))
        .await
        .map_err(|e| e.message)?;
    store
        .commit_uninstall(&site, "a", false, row("a", 1, StepState::Done))
        .await
        .map_err(|e| e.message)?;
    if !apps(&store, &site).await?.is_empty() {
        return Err("the app was not uninstalled".to_owned());
    }
    let before = installed("a", 1)?;
    store
        .restore(&site, &before, row("a", 2, StepState::Undone))
        .await
        .map_err(|e| e.message)?;
    let state = store.load(&site).await.map_err(|e| e.message)?;
    if state.get("a") != Some(&before) {
        return Err("restore did not put the app back exactly".to_owned());
    }
    Ok(())
}

/// Two writes for one step leave one row, not two.
async fn journal_rows_are_unique_by_step(store: Arc<dyn AppRegistryStore>) -> CheckResult {
    let site = site()?;
    store
        .record(&site, row("a", 4, StepState::Started))
        .await
        .map_err(|e| e.message)?;
    store
        .record(&site, row("a", 4, StepState::Done))
        .await
        .map_err(|e| e.message)?;
    let rows = rows(&store, &site).await?;
    if rows.len() != 1 || rows[0].state != StepState::Done {
        return Err(format!("the step did not collapse to one row: {rows:?}"));
    }
    Ok(())
}

/// Clearing the journal leaves nothing to resume.
async fn clear_journal_empties_it(store: Arc<dyn AppRegistryStore>) -> CheckResult {
    let site = site()?;
    store
        .record(&site, row("a", 0, StepState::Done))
        .await
        .map_err(|e| e.message)?;
    store.clear_journal(&site).await.map_err(|e| e.message)?;
    if !rows(&store, &site).await?.is_empty() {
        return Err("the journal was not cleared".to_owned());
    }
    Ok(())
}

/// The lifecycle flag round-trips through conditional moves.
async fn lifecycle_round_trips(store: Arc<dyn AppRegistryStore>) -> CheckResult {
    let site = site()?;
    store
        .set_lifecycle(&site, SiteLifecycle::Idle, SiteLifecycle::Installing)
        .await
        .map_err(|e| format!("{e:?}"))?;
    if store.lifecycle(&site).await.map_err(|e| e.message)? != SiteLifecycle::Installing {
        return Err("the installing flag did not stick".to_owned());
    }
    store
        .set_lifecycle(&site, SiteLifecycle::Installing, SiteLifecycle::Idle)
        .await
        .map_err(|e| format!("{e:?}"))?;
    if store.lifecycle(&site).await.map_err(|e| e.message)? != SiteLifecycle::Idle {
        return Err("the idle flag did not stick".to_owned());
    }
    Ok(())
}

/// A move from the wrong state is a typed conflict naming stored, expected and target, and
/// changes nothing. A site nobody has touched reads as idle.
async fn a_lifecycle_move_from_the_wrong_state_changes_nothing(
    store: Arc<dyn AppRegistryStore>,
) -> CheckResult {
    let site = site()?;
    let refused = store
        .set_lifecycle(&site, SiteLifecycle::Installing, SiteLifecycle::Idle)
        .await;
    let expected = Err(LifecycleError::Conflict(LifecycleConflict {
        stored: SiteLifecycle::Idle,
        expected: SiteLifecycle::Installing,
        target: SiteLifecycle::Idle,
    }));
    if refused != expected {
        return Err(format!(
            "moving an untouched site from installing gave {refused:?}"
        ));
    }
    store
        .set_lifecycle(&site, SiteLifecycle::Idle, SiteLifecycle::Installing)
        .await
        .map_err(|e| format!("{e:?}"))?;
    // Installing is stored; "from idle" is now wrong and must not clear it.
    let refused = store
        .set_lifecycle(&site, SiteLifecycle::Idle, SiteLifecycle::Idle)
        .await;
    if !matches!(refused, Err(LifecycleError::Conflict(c)) if c.stored == SiteLifecycle::Installing)
    {
        return Err(format!("a stale idle view was not refused: {refused:?}"));
    }
    if store.lifecycle(&site).await.map_err(|e| e.message)? != SiteLifecycle::Installing {
        return Err("a refused move changed the stored flag".to_owned());
    }
    Ok(())
}

/// Callers racing `Idle -> Installing` on one site: exactly one wins, the rest get a conflict
/// that maps to `installer.lease_held`, and the flag ends up `installing`.
async fn racing_idle_to_installing_lets_exactly_one_win(
    store: Arc<dyn AppRegistryStore>,
) -> CheckResult {
    let site = site()?;
    let racers =
        (0..8).map(|_| store.set_lifecycle(&site, SiteLifecycle::Idle, SiteLifecycle::Installing));
    let results = futures::future::join_all(racers).await;
    let mut won = 0_usize;
    for result in results {
        match result {
            Ok(()) => won += 1,
            Err(LifecycleError::Conflict(conflict)) => {
                let error = conflict.to_exec_error(&site);
                if error.code != crate::error::LEASE_HELD {
                    return Err(format!("the loser got {} not lease_held", error.code));
                }
            }
            Err(LifecycleError::Failed(error)) => {
                return Err(format!("a racer failed outright: {error}"));
            }
        }
    }
    if won != 1 {
        return Err(format!("{won} callers won the race; exactly one may"));
    }
    if store.lifecycle(&site).await.map_err(|e| e.message)? != SiteLifecycle::Installing {
        return Err("the winner's flag is not installing".to_owned());
    }
    Ok(())
}

/// The uninstall export hook is readable for any app.
async fn owned_doctypes_is_readable(store: Arc<dyn AppRegistryStore>) -> CheckResult {
    let site = site()?;
    store
        .owned_doctypes(&site, "a")
        .await
        .map_err(|e: ExecError| e.message)?;
    Ok(())
}
