//! What each account is doing now, and the metrics of the service.
//!
//! Metric names are under `spot_mail_intake_`; the only labels are `site` and `account`
//! (names, never an address). A dropped message is counted in one counter per reason, so the
//! reason is part of the name and not a label.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use spot_mail_intake::{PollOutcome, PollReport};
use time::OffsetDateTime;

/// Polls run.
pub const POLLS_TOTAL: &str = "spot_mail_intake_polls_total";
/// Messages the sink created a document for.
pub const ACCEPTED_TOTAL: &str = "spot_mail_intake_accepted_total";
/// Messages skipped because they were already imported.
pub const DUPLICATES_TOTAL: &str = "spot_mail_intake_duplicates_total";
/// Auto-replies and bulk mail dropped.
pub const DROPPED_AUTO_REPLY_TOTAL: &str = "spot_mail_intake_dropped_auto_reply_total";
/// Our own mail dropped (loop protection).
pub const DROPPED_OWN_MAIL_TOTAL: &str = "spot_mail_intake_dropped_own_mail_total";
/// Messages dropped over the daily cap.
pub const DROPPED_DAILY_CAP_TOTAL: &str = "spot_mail_intake_dropped_daily_cap_total";
/// Messages the sink chose to drop, any reason.
pub const DROPPED_OTHER_TOTAL: &str = "spot_mail_intake_dropped_other_total";
/// Messages rejected as poison.
pub const REJECTED_TOTAL: &str = "spot_mail_intake_rejected_total";
/// Attachments skipped (blocked extension or over the per-message cap).
pub const ATTACHMENTS_SKIPPED_TOTAL: &str = "spot_mail_intake_attachments_skipped_total";
/// Seconds since the last successful poll of the account.
pub const LAG_SECONDS: &str = "spot_mail_intake_lag_seconds";
/// The account's state as a number (see [`state_code`]).
pub const ACCOUNT_STATE: &str = "spot_mail_intake_account_state";

/// The states an account reports.
pub mod state {
    /// Not polled yet.
    pub const STARTING: &str = "starting";
    /// Polling normally.
    pub const RUNNING: &str = "running";
    /// Another node holds the lease.
    pub const NOT_LEADER: &str = "not_leader";
    /// The server refused the credentials; stopped until an operator acts.
    pub const AUTH_FAILED: &str = "auth_failed";
    /// The server or the egress guard could not be reached; retrying with backoff.
    pub const UNREACHABLE: &str = "unreachable";
    /// The `run_as` user is missing, disabled or may not create the document; paused.
    pub const RUN_AS_UNAVAILABLE: &str = "run_as_unavailable";
    /// The server or the egress guard refused the configuration; stopped until an operator acts.
    pub const CONFIG_REJECTED: &str = "config_rejected";
    /// The entry breaks a rule; never polled.
    pub const INVALID: &str = "invalid";
    /// Disabled by the operator.
    pub const DISABLED: &str = "disabled";
}

/// The number the `account_state` gauge carries for `state`.
#[must_use]
pub fn state_code(state: &str) -> f64 {
    match state {
        state::RUNNING => 0.0,
        state::STARTING => 1.0,
        state::NOT_LEADER => 2.0,
        state::UNREACHABLE => 3.0,
        state::RUN_AS_UNAVAILABLE => 4.0,
        state::AUTH_FAILED => 5.0,
        state::CONFIG_REJECTED => 6.0,
        state::INVALID => 7.0,
        state::DISABLED => 8.0,
        _ => 9.0,
    }
}

/// One account's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountStatus {
    /// One of the [`state`] words.
    pub state: &'static str,
    /// The last poll that completed without a failure.
    pub last_ok: Option<OffsetDateTime>,
    /// The sink's pause reason, set while it is paused.
    pub paused: Option<&'static str>,
}

/// Every account's state, shared by the loop, the sink and the readiness check.
#[derive(Debug, Default)]
pub struct Status {
    accounts: Mutex<BTreeMap<String, AccountStatus>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Status {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `account`'s state, keeping its last success.
    pub fn set(&self, account: &str, state: &'static str) {
        let mut accounts = lock(&self.accounts);
        let entry = accounts.entry(account.to_owned()).or_insert(AccountStatus {
            state: state::STARTING,
            last_ok: None,
            paused: None,
        });
        entry.state = state;
    }

    /// Records a completed poll at `now`.
    pub fn ok(&self, account: &str, now: OffsetDateTime) {
        let mut accounts = lock(&self.accounts);
        if let Some(entry) = accounts.get_mut(account) {
            entry.last_ok = Some(now);
        }
    }

    /// The sink could not run: the account is paused for `reason`.
    pub fn pause(&self, account: &str, reason: &'static str) {
        let mut accounts = lock(&self.accounts);
        let entry = accounts.entry(account.to_owned()).or_insert(AccountStatus {
            state: state::STARTING,
            last_ok: None,
            paused: None,
        });
        entry.paused = Some(reason);
    }

    /// The sink worked again.
    pub fn resume(&self, account: &str) {
        let mut accounts = lock(&self.accounts);
        if let Some(entry) = accounts.get_mut(account) {
            entry.paused = None;
        }
    }

    /// The state to report now: a pause outranks the poll's own outcome.
    #[must_use]
    pub fn effective(&self, account: &str) -> Option<&'static str> {
        lock(&self.accounts).get(account).map(|s| {
            if s.paused.is_some() {
                state::RUN_AS_UNAVAILABLE
            } else {
                s.state
            }
        })
    }

    /// A copy of every account's status.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, AccountStatus> {
        lock(&self.accounts).clone()
    }
}

/// The state a poll's outcome means.
#[must_use]
pub fn state_of(report: &PollReport) -> &'static str {
    match report.outcome {
        PollOutcome::Disabled => state::DISABLED,
        PollOutcome::NotLeader => state::NOT_LEADER,
        PollOutcome::AuthFailed => state::AUTH_FAILED,
        PollOutcome::ConfigRejected => state::CONFIG_REJECTED,
        PollOutcome::Failed => match report.reason {
            Some("sink_retryable") => state::RUN_AS_UNAVAILABLE,
            _ => state::UNREACHABLE,
        },
        PollOutcome::Polled | PollOutcome::MailboxReset => state::RUNNING,
    }
}

/// Emits the metrics of one poll.
pub fn record_poll(site: &str, account: &str, report: &PollReport, lag: Option<f64>, state: &str) {
    let (s, a) = (site.to_owned(), account.to_owned());
    metrics::counter!(POLLS_TOTAL, "site" => s.clone(), "account" => a.clone()).increment(1);
    metrics::counter!(ACCEPTED_TOTAL, "site" => s.clone(), "account" => a.clone())
        .increment(u64::from(report.accepted));
    metrics::counter!(DUPLICATES_TOTAL, "site" => s.clone(), "account" => a.clone())
        .increment(u64::from(report.duplicates));
    metrics::counter!(REJECTED_TOTAL, "site" => s.clone(), "account" => a.clone())
        .increment(u64::from(report.rejected));
    let mut other = 0u64;
    for (reason, count) in &report.dropped {
        let count = u64::from(*count);
        match *reason {
            "auto_reply" => metrics::counter!(DROPPED_AUTO_REPLY_TOTAL, "site" => s.clone(), "account" => a.clone()).increment(count),
            "own_mail" => metrics::counter!(DROPPED_OWN_MAIL_TOTAL, "site" => s.clone(), "account" => a.clone()).increment(count),
            "daily_cap" => metrics::counter!(DROPPED_DAILY_CAP_TOTAL, "site" => s.clone(), "account" => a.clone()).increment(count),
            _ => other += count,
        }
    }
    if other > 0 {
        metrics::counter!(DROPPED_OTHER_TOTAL, "site" => s.clone(), "account" => a.clone())
            .increment(other);
    }
    if let Some(lag) = lag {
        metrics::gauge!(LAG_SECONDS, "site" => s.clone(), "account" => a.clone()).set(lag);
    }
    metrics::gauge!(ACCOUNT_STATE, "site" => s, "account" => a).set(state_code(state));
}

/// Counts attachments the sink skipped.
pub fn record_skipped(site: &str, account: &str, skipped: u64) {
    if skipped > 0 {
        metrics::counter!(ATTACHMENTS_SKIPPED_TOTAL, "site" => site.to_owned(), "account" => account.to_owned())
            .increment(skipped);
    }
}
