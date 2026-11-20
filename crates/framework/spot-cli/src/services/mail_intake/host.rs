//! The poll loop: one task per account, each under the coordinator lease
//! `mail_intake:<site>:<account>` that [`poll_once`] takes itself, so one node polls an account.
//!
//! The delay between polls is the report's `next_delay`. A poll that is still running when the
//! stop signal arrives gets [`STOP_BOUND`] to finish and is then dropped: the cursor only moves
//! past handled mail, so a dropped poll loses nothing, and its lease expires by itself.

use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::Coordinator;
use spot_core::Clock;
use spot_mail_intake::{
    Account, CredentialSource, CursorStore, IntakeAudit, IntakeSink, MailFetcher, PollDeps,
    PollOutcome, PollState, SeenStore, poll_once,
};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::status::{Status, record_poll, state, state_of};
use spot_mail_intake::AuditEvent;

/// How long a running poll may continue after the stop signal.
pub const STOP_BOUND: Duration = Duration::from_secs(3);

/// How often the seen set is purged.
pub const PURGE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// How long a Message-ID digest is kept: far beyond any mailbox rescan after a `UIDVALIDITY`
/// change, so a re-read mailbox never imports an old message twice.
pub const SEEN_RETENTION_DAYS: i64 = 90;

/// How often the all-accounts-halted check runs, unless [`IntakeHost::with_halt_check`] says
/// otherwise.
const HALT_CHECK: Duration = Duration::from_secs(5);

/// Everything the loop needs, built by the registration (or by a test).
pub struct Deps {
    /// The site's name.
    pub site: String,
    /// The coordinator the leases live in.
    pub coordinator: Arc<dyn Coordinator>,
    /// This node's identity in leases.
    pub node: String,
    /// How long a lease lasts.
    pub lease_ttl: Duration,
    /// The UID cursors.
    pub cursors: Arc<dyn CursorStore>,
    /// The Message-ID digests.
    pub seen: Arc<dyn SeenStore>,
    /// The sink that creates documents.
    pub sink: Arc<dyn IntakeSink>,
    /// The audit trail.
    pub audit: Arc<dyn IntakeAudit>,
    /// Where passwords come from.
    pub credentials: Arc<dyn CredentialSource>,
    /// What fetches mail.
    pub fetcher: Arc<dyn MailFetcher>,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// The shared account status.
    pub status: Arc<Status>,
}

/// The audit sink: the `spot::audit::mail_intake` log target. Entries hold counts, UID
/// positions and reason codes only.
pub struct TracingAudit {
    /// The site.
    pub site: String,
}

impl IntakeAudit for TracingAudit {
    fn record(&self, account: &str, event: AuditEvent) {
        tracing::info!(
            target: "spot::audit::mail_intake",
            site = %self.site, account = %account, event = ?event,
            "mail intake"
        );
    }
}

/// The loop over the ready accounts of one site.
pub struct IntakeHost {
    deps: Arc<Deps>,
    accounts: Vec<Account>,
    halt_check: Duration,
}

/// Waits for the stop signal, then for [`STOP_BOUND`].
async fn stopped_and_bound(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            break;
        }
        if stop.changed().await.is_err() {
            break;
        }
    }
    tokio::time::sleep(STOP_BOUND).await;
}

/// Waits `delay`, or until the stop signal. `true` when it is time to stop.
async fn wait(stop: &mut watch::Receiver<bool>, delay: Option<Duration>) -> bool {
    loop {
        if *stop.borrow() {
            return true;
        }
        match delay {
            Some(delay) => {
                tokio::select! {
                    () = tokio::time::sleep(delay) => return *stop.borrow(),
                    changed = stop.changed() => {
                        if changed.is_err() {
                            return true;
                        }
                    }
                }
            }
            None => {
                if stop.changed().await.is_err() {
                    return true;
                }
            }
        }
    }
}

async fn run_account(deps: Arc<Deps>, account: Account, mut stop: watch::Receiver<bool>) {
    let mut poll_state = PollState::new();
    deps.status.set(&account.name, state::STARTING);
    loop {
        if *stop.borrow() {
            return;
        }
        let now = deps.clock.now_utc();
        let poll_deps = PollDeps {
            coordinator: &*deps.coordinator,
            holder: &deps.node,
            site: &deps.site,
            lease_ttl: deps.lease_ttl,
            cursors: &*deps.cursors,
            seen: &*deps.seen,
            sink: &*deps.sink,
            audit: &*deps.audit,
            credentials: &*deps.credentials,
            fetcher: &*deps.fetcher,
        };
        let report = tokio::select! {
            report = poll_once(&account, now, &mut poll_state, &poll_deps) => report,
            () = stopped_and_bound(&mut stop) => return,
        };
        deps.status.set(&account.name, state_of(&report));
        if matches!(
            report.outcome,
            PollOutcome::Polled | PollOutcome::MailboxReset
        ) {
            deps.status.ok(&account.name, now);
        }
        let lag = deps
            .status
            .snapshot()
            .get(&account.name)
            .and_then(|s| s.last_ok)
            .map(|at| (now - at).as_seconds_f64().max(0.0));
        let shown = deps
            .status
            .effective(&account.name)
            .unwrap_or(state::STARTING);
        record_poll(&deps.site, &account.name, &report, lag, shown);
        if let Some(reason) = report.reason {
            tracing::warn!(
                site = %deps.site, account = %account.name, reason,
                "mail intake poll did not complete normally"
            );
        }
        if wait(&mut stop, report.next_delay).await {
            return;
        }
    }
}

async fn purge_loop(deps: Arc<Deps>, mut stop: watch::Receiver<bool>) {
    loop {
        let cutoff = deps.clock.now_utc() - time::Duration::days(SEEN_RETENTION_DAYS);
        if let Err(error) = deps.seen.purge_older_than(cutoff).await {
            tracing::warn!(site = %deps.site, %error, "the mail intake seen set could not be purged");
        }
        if wait(&mut stop, Some(PURGE_EVERY)).await {
            return;
        }
    }
}

impl IntakeHost {
    /// A loop over `accounts` (already validated).
    #[must_use]
    pub fn new(deps: Arc<Deps>, accounts: Vec<Account>) -> Self {
        Self {
            deps,
            accounts,
            halt_check: HALT_CHECK,
        }
    }

    /// How often the host looks at whether every account is halted (tests shorten it).
    #[must_use]
    pub fn with_halt_check(mut self, every: Duration) -> Self {
        self.halt_check = every;
        self
    }

    /// The accounts this host polls.
    #[must_use]
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    /// Runs every account until `stop` is set. Returns `Err(reason)` when every enabled account
    /// is halted (credentials or configuration refused), so readiness can say why mail is not
    /// arriving; the other states are transient and keep the loop running.
    ///
    /// # Errors
    /// The reason all accounts stopped.
    pub async fn run(&self, stop: watch::Receiver<bool>) -> Result<(), String> {
        let mut tasks = JoinSet::new();
        for account in &self.accounts {
            tasks.spawn(run_account(
                Arc::clone(&self.deps),
                account.clone(),
                stop.clone(),
            ));
        }
        tasks.spawn(purge_loop(Arc::clone(&self.deps), stop.clone()));
        let mut watcher = stop.clone();
        let enabled: Vec<&str> = self
            .accounts
            .iter()
            .filter(|a| a.enabled)
            .map(|a| a.name.as_str())
            .collect();
        loop {
            if wait(&mut watcher, Some(self.halt_check)).await {
                break;
            }
            if enabled.is_empty() {
                continue;
            }
            let halted = enabled
                .iter()
                .find_map(|name| match self.deps.status.effective(name) {
                    Some(s) if s == state::AUTH_FAILED || s == state::CONFIG_REJECTED => Some(s),
                    _ => None,
                });
            let all = enabled.iter().all(|name| {
                matches!(
                    self.deps.status.effective(name),
                    Some(state::AUTH_FAILED | state::CONFIG_REJECTED)
                )
            });
            if all && let Some(reason) = halted {
                tasks.abort_all();
                return Err(reason.to_owned());
            }
        }
        // Every task ends on its own once the stop signal is set; a poll gets STOP_BOUND.
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}
