//! The host guard of the access review service (WP-SEC-02.T6): the duties the stores and the
//! state machines leave to the host.
//!
//! 1. **Fail-closed permission check** ([`AccessGuard::authorise`]). Every [`Operation`] of the
//!    review API asks the permission engine (through [`ReviewPermissions`]) before it reads or
//!    writes a store. It refuses when the engine errors, when the caller has no role snapshot (a
//!    missing or empty snapshot is a refusal, never a pass) and when the engine does not answer
//!    within the deadline (default 2 s, `deadline`). The refusal is a catalogued code
//!    ([`CODE_PERMISSION_UNAVAILABLE`], [`CODE_NO_ROLE_SNAPSHOT`], [`CODE_NOT_PERMITTED`]) and the
//!    caller learns no more than the code.
//! 2. **Refusal records are rate-limited** ([`RefusalLimiter`]). A refusal is audited, but a caller
//!    who is refused over and over cannot fill the audit store: at most `per_window` records per
//!    principal, tenant and action in a window (default 20 per 60 s). Beyond that the refusal is
//!    still returned and counted; one `refusals_suppressed` summary with the count is written when
//!    the window closes. A granted action never touches the limiter.
//! 3. **Overrides are read only through `active_overrides(now)`** ([`active_override_records`]):
//!    the slice that feeds the analysis is the store's `active_overrides(tenant, now)` filtered
//!    again by `is_active(now)`, never `list_overrides`.
//!
//! Time is the injected clock's; nothing here reads the wall clock. The engine deadline uses the
//! runtime timer, which is a duration, not a date.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_core::Clock;
use spot_http::api::service::BoxFuture;
use spot_sod::{SodOverride, SodStore};
use spot_tenancy::TenantId;

use crate::services::access_review::Operation;

/// The engine errored or did not answer in time (class Unavailable, retryable).
pub const CODE_PERMISSION_UNAVAILABLE: &str = "access_review_host.permission_unavailable";
/// The caller has no role snapshot (class PermissionDenied).
pub const CODE_NO_ROLE_SNAPSHOT: &str = "access_review_host.no_role_snapshot";
/// The caller does not hold the permission (class PermissionDenied).
pub const CODE_NOT_PERMITTED: &str = "access_review_host.not_permitted";
/// The summary record's kind.
pub const SUMMARY_KIND: &str = "refusals_suppressed";

/// What the permission engine answers for one principal and action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The principal has no role snapshot (or an empty one).
    NoSnapshot,
    /// Held.
    Allowed,
    /// Not held.
    Denied,
}

/// The permission engine of the site, with the caller's role snapshot. `Err` is an engine error;
/// the guard treats it as a refusal.
pub trait ReviewPermissions: Send + Sync {
    /// Whether `principal` may do `action`.
    fn decide<'a>(
        &'a self,
        principal: &'a str,
        action: &'static str,
    ) -> BoxFuture<'a, std::result::Result<Verdict, String>>;
}

/// Why the guard refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardRefusal {
    /// Engine error or timeout.
    PermissionUnavailable,
    /// No role snapshot.
    NoRoleSnapshot,
    /// The permission is not held.
    NotPermitted,
}

impl GuardRefusal {
    /// The catalogued code, and the only thing a caller learns.
    pub fn code(self) -> &'static str {
        match self {
            GuardRefusal::PermissionUnavailable => CODE_PERMISSION_UNAVAILABLE,
            GuardRefusal::NoRoleSnapshot => CODE_NO_ROLE_SNAPSHOT,
            GuardRefusal::NotPermitted => CODE_NOT_PERMITTED,
        }
    }
}

/// The permission an operation needs. Exhaustive on purpose: a new [`Operation`] does not
/// compile until it names its permission, so no operation is left unguarded.
pub fn action_of(operation: Operation) -> &'static str {
    match operation {
        Operation::Decide => "access_review.decide",
        Operation::Report => "access_review.report",
        Operation::PendingRemovals => "access_review.manage",
    }
}

/// One audit record the guard asks the host to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefusalRecord {
    /// One refusal.
    Refused {
        /// The principal refused.
        principal: String,
        /// The action.
        action: &'static str,
        /// The code answered.
        code: &'static str,
    },
    /// The count of refusals not written in a closed window.
    Suppressed {
        /// The principal.
        principal: String,
        /// The action.
        action: &'static str,
        /// How many refusals were not written.
        count: u64,
    },
}

/// Where the guard's audit records go.
pub trait RefusalAudit: Send + Sync {
    /// Writes the record. A failure is the host's to log; it never changes the refusal.
    fn write<'a>(&'a self, record: RefusalRecord) -> BoxFuture<'a, ()>;
}

#[derive(Clone, Copy)]
struct Window {
    started: i64,
    written: u32,
    suppressed: u64,
}

/// The per principal, tenant and action budget of refusal records.
pub struct RefusalLimiter {
    per_window: u32,
    window_secs: i64,
    windows: Mutex<BTreeMap<(String, String, &'static str), Window>>,
    counter: AtomicU64,
}

impl RefusalLimiter {
    /// A limiter of `per_window` records per `window_secs`.
    pub fn new(per_window: u32, window_secs: i64) -> Self {
        Self {
            per_window,
            window_secs: window_secs.max(1),
            windows: Mutex::new(BTreeMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// How many refusals were suppressed since start (the counter the host exports).
    pub fn suppressed_total(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }

    /// Notes one refusal at `now`. Returns whether it is written, and the summary of the
    /// previous window of this key when that window just closed with suppressed refusals.
    pub fn note(
        &self,
        tenant: &str,
        principal: &str,
        action: &'static str,
        now: i64,
    ) -> (bool, Option<u64>) {
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        let key = (tenant.to_owned(), principal.to_owned(), action);
        let mut closed = None;
        let entry = windows.entry(key).or_insert(Window {
            started: now,
            written: 0,
            suppressed: 0,
        });
        if now - entry.started >= self.window_secs {
            if entry.suppressed > 0 {
                closed = Some(entry.suppressed);
            }
            *entry = Window {
                started: now,
                written: 0,
                suppressed: 0,
            };
        }
        if entry.written < self.per_window {
            entry.written += 1;
            (true, closed)
        } else {
            entry.suppressed += 1;
            self.counter.fetch_add(1, Ordering::SeqCst);
            (false, closed)
        }
    }

    /// Closes every window that ended by `now` and returns the summaries
    /// `(tenant, principal, action, suppressed)` of those that suppressed refusals. The host
    /// calls it at each tick so a quiet caller's summary is still written.
    pub fn close_windows(&self, now: i64) -> Vec<(String, String, &'static str, u64)> {
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::new();
        windows.retain(|(tenant, principal, action), w| {
            if now - w.started >= self.window_secs {
                if w.suppressed > 0 {
                    out.push((tenant.clone(), principal.clone(), *action, w.suppressed));
                }
                false
            } else {
                true
            }
        });
        out
    }
}

/// The guard of the review API.
pub struct AccessGuard {
    engine: Arc<dyn ReviewPermissions>,
    audit: Arc<dyn RefusalAudit>,
    clock: Arc<dyn Clock>,
    limiter: RefusalLimiter,
    tenant: String,
    deadline: Duration,
}

impl AccessGuard {
    /// A guard with the defaults: 2 s deadline, 20 refusal records per minute.
    pub fn new(
        engine: Arc<dyn ReviewPermissions>,
        audit: Arc<dyn RefusalAudit>,
        clock: Arc<dyn Clock>,
        tenant: &TenantId,
    ) -> Self {
        Self {
            engine,
            audit,
            clock,
            limiter: RefusalLimiter::new(20, 60),
            tenant: tenant.to_string(),
            deadline: Duration::from_secs(2),
        }
    }

    /// Sets the engine deadline.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Sets the refusal record budget.
    #[must_use]
    pub fn with_budget(mut self, per_window: u32, window_secs: i64) -> Self {
        self.limiter = RefusalLimiter::new(per_window, window_secs);
        self
    }

    /// The limiter, for the suppressed counter.
    pub fn limiter(&self) -> &RefusalLimiter {
        &self.limiter
    }

    /// Lets `principal` do `operation`, or refuses. Fails closed on every path.
    ///
    /// # Errors
    ///
    /// The [`GuardRefusal`]; its code is all a caller learns.
    pub async fn authorise(
        &self,
        principal: &str,
        operation: Operation,
    ) -> std::result::Result<(), GuardRefusal> {
        let action = action_of(operation);
        let answer =
            tokio::time::timeout(self.deadline, self.engine.decide(principal, action)).await;
        let refusal = match answer {
            Ok(Ok(Verdict::Allowed)) => return Ok(()),
            Ok(Ok(Verdict::Denied)) => GuardRefusal::NotPermitted,
            Ok(Ok(Verdict::NoSnapshot)) => GuardRefusal::NoRoleSnapshot,
            Ok(Err(_)) | Err(_) => GuardRefusal::PermissionUnavailable,
        };
        self.refused(principal, action, refusal).await;
        Err(refusal)
    }

    async fn refused(&self, principal: &str, action: &'static str, refusal: GuardRefusal) {
        let now = self.clock.now_utc().unix_timestamp();
        let (write, closed) = self.limiter.note(&self.tenant, principal, action, now);
        if let Some(count) = closed {
            self.audit
                .write(RefusalRecord::Suppressed {
                    principal: principal.to_owned(),
                    action,
                    count,
                })
                .await;
        }
        if write {
            self.audit
                .write(RefusalRecord::Refused {
                    principal: principal.to_owned(),
                    action,
                    code: refusal.code(),
                })
                .await;
        }
    }

    /// Writes the summaries of the windows that closed (called at each tick).
    pub async fn flush(&self) {
        let now = self.clock.now_utc().unix_timestamp();
        for (_tenant, principal, action, count) in self.limiter.close_windows(now) {
            self.audit
                .write(RefusalRecord::Suppressed {
                    principal,
                    action,
                    count,
                })
                .await;
        }
    }
}

/// The overrides that cover anything at `now`: the store's `active_overrides(tenant, now)`,
/// filtered again by `is_active(now)`. `list_overrides` is never the source of this slice.
///
/// # Errors
///
/// The store's error code (never its text).
pub async fn active_override_records(
    store: &dyn SodStore,
    tenant: &TenantId,
    now: i64,
) -> std::result::Result<Vec<SodOverride>, &'static str> {
    let stored = store
        .active_overrides(tenant, now)
        .await
        .map_err(|e| e.code())?;
    Ok(stored
        .into_iter()
        .filter(|s| s.is_active(now))
        .map(|s| s.record)
        .collect())
}

/// Fakes for the host's own unit tests: an engine that allows everyone and an audit that drops.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    struct Allow;
    impl ReviewPermissions for Allow {
        fn decide<'a>(
            &'a self,
            _p: &'a str,
            _a: &'static str,
        ) -> BoxFuture<'a, std::result::Result<Verdict, String>> {
            Box::pin(async { Ok(Verdict::Allowed) })
        }
    }
    struct Silent;
    impl RefusalAudit for Silent {
        fn write<'a>(&'a self, _r: RefusalRecord) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    pub(crate) fn allow_all() -> AccessGuard {
        let clock = Arc::new(spot_core::ManualClock::new(
            time::macros::datetime!(2026-11-12 10:00 UTC),
        ));
        AccessGuard::new(
            Arc::new(Allow),
            Arc::new(Silent),
            clock,
            &TenantId::new_v7(1_800_000_000_000, [7; 10]),
        )
    }
}
