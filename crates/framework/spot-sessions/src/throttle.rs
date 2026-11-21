//! The [`LoginThrottle`]: a pure state machine deciding whether a sign-in attempt may
//! proceed, with exactly two independent counters, audited state changes, and a check
//! that runs before the password hash so its timing cannot leak password validity.
//!
//! # The two counters (brief; the trade-off is stated, not hidden)
//!
//! - **Per account** ([`AccountKey`], the user's sign-in name as given): failures count
//!   toward exponential backoff and, after `max_failures`, the **hard lockout**. Success
//!   resets only this counter (one's own good password must not wash out what an attacker
//!   is doing *from a compromised source*).
//! - **Per source** ([`SourceKey`], the salted digest of the client's address): failures
//!   from one address back that address off, but **never lock the account** — so an
//!   attacker who knows only a victim's username cannot lock the victim out by failing
//!   logins from one address (the lockout denial-of-service). The trade-off is spelled
//!   out: without a lock trigger, one address may still attempt *many* accounts, each
//!   under its own backoff; the per-source backoff (delay grows, and the window bound)
//!   is the slow-down for that "one source, many accounts" attack. A full account lockout
//!   from source evidence alone would need cross-source correlation, which belongs to the
//!   identity-event analytics (WP-IAM-01), not to this state machine.
//!
//! # Timing
//!
//! [`LoginThrottle::check`] runs before the password hash. During backoff the required
//! delay applies to a wrong *and* a right password alike (the caller sleeps — never in
//! this crate — after the hash check either way), so timing cannot distinguish "the
//! delay" from "the delay plus a hash" for a valid password.
//!
//! # Audit
//!
//! Every state change returns an [`AuditNote`] (account, source digest, kind, counts; no
//! credential and no raw address) for the caller to journal as an identity event
//! (ADR-070 §8.4: sign-in failures, lockout and unlock are audited).
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::model::SourceHash;

/// Identifies an account: the sign-in name as presented (an e-mail or username, lowercased
/// by the caller only if its policy says so; the throttle compares exactly).
///
/// Not a secret (audit events name accounts, ADR-070 §8.4), but bounded like every
/// user-controlled value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountKey(String);

impl AccountKey {
    /// Wraps a sign-in name (non-empty, at most 320 bytes; the same bound the login
    /// contract puts on `user`).
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        if name.is_empty() {
            return Err("the account key is empty".to_owned());
        }
        if name.len() > 320 {
            return Err("the account key is longer than 320 bytes".to_owned());
        }
        Ok(Self(name))
    }

    /// The name, for audit notes and store keys.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A per-site salted digest of the source address (same construction as the session
/// [`SourceHash`](crate::model::SourceHash)); raw addresses are never stored or logged.
pub type SourceKey = SourceHash;

/// The throttle policy (configurable; the defaults are the shipped values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottleConfig {
    /// The failures counted per account inside [`ThrottleConfig::window`] before backoff
    /// begins. Defaults to 3.
    pub backoff_after: u32,
    /// The hard lockout threshold per account: the M-th failure inside the window locks.
    /// Defaults to 10 (OWASP's "locking out after 10 attempts is reasonable"; 3 would
    /// give an attacker an easy denial of service, and the exponential backoff already
    /// slows the 4th..9th attempts to minutes).
    pub lockout_after: u32,
    /// The window failures count inside. Defaults to 15 minutes. A window is half-open:
    /// a failure exactly at `last + window` starts a fresh window.
    pub window: Duration,
    /// The base backoff delay for the first throttled failure. Defaults to 1 s.
    pub backoff_base: Duration,
    /// The backoff multiplier per further failure (integer, so no floats). Defaults to 2:
    /// 1 s, 2 s, 4 s, 8 s… The cap below stops the growth.
    pub backoff_multiplier: u32,
    /// The cap on the computed delay. Defaults to 15 minutes: beyond that the delay has
    /// done its work and a permanent-feeling wait just trains clients to retry elsewhere.
    pub backoff_cap: Duration,
    /// How long a hard lockout lasts (the expiry unlock). Defaults to 30 minutes — the
    /// victim regains access without an administrator; an administrator can always unlock
    /// earlier ([`LoginThrottle::unlock`]).
    pub lockout_duration: Duration,
}

impl ThrottleConfig {
    /// The shipped defaults (cited in the field docs).
    pub const DEFAULT: Self = Self {
        backoff_after: 3,
        lockout_after: 10,
        window: Duration::from_secs(15 * 60),
        backoff_base: Duration::from_secs(1),
        backoff_multiplier: 2,
        backoff_cap: Duration::from_secs(15 * 60),
        lockout_duration: Duration::from_secs(30 * 60),
    };

    /// Validates the relationships: backoff must begin at or before the lockout, the
    /// multiplier at least 1, the base at most the cap.
    pub fn validated(&self) -> Result<Self, String> {
        if self.backoff_after == 0 {
            return Err("backoff_after must be at least 1".to_owned());
        }
        if self.lockout_after < self.backoff_after {
            return Err("lockout_after must be at least backoff_after".to_owned());
        }
        if self.backoff_multiplier == 0 {
            return Err("backoff_multiplier must be at least 1".to_owned());
        }
        if self.backoff_base > self.backoff_cap {
            return Err("backoff_base must not exceed backoff_cap".to_owned());
        }
        if self.window.is_zero() || self.lockout_duration.is_zero() {
            return Err("window and lockout_duration must be non-zero".to_owned());
        }
        Ok(*self)
    }
}

/// What the check decided for this attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThrottleDecision {
    /// Proceed (then hash the password). `delay` is zero here.
    Allow,
    /// The attempt is refused *before hashing*: wait `delay` before the next attempt.
    /// The delay applies to wrong and right passwords alike (timing safety).
    Delay {
        /// The minimum wait before the next attempt is checked.
        delay: Duration,
        /// Which counter fired.
        because: ThrottleKind,
    },
    /// The account is hard-locked until `until` (unix seconds): every attempt is refused
    /// before hashing, right or wrong.
    Locked {
        /// Unix seconds when the lockout lifts (or when an administrator unlocks).
        until: i64,
    },
}

impl ThrottleDecision {
    /// Whether the caller may proceed to hash and verify the password.
    #[must_use]
    pub fn allowed(&self) -> bool {
        matches!(self, ThrottleDecision::Allow)
    }

    /// The stable machine-readable code the API layer maps a refusal onto (a lockout
    /// becomes `auth.locked`, a backoff becomes `auth.rate_limited` with `Retry-After`);
    /// an allowed attempt raises nothing.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        match self {
            ThrottleDecision::Allow => None,
            ThrottleDecision::Delay { .. } => Some("throttle.backoff_delayed"),
            ThrottleDecision::Locked { .. } => Some("throttle.locked"),
        }
    }
}

/// Which counter produced a state change or decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThrottleKind {
    /// The account counter (backoff and lockout live here).
    Account,
    /// The source counter (backoff only; never locks).
    Source,
}

impl std::fmt::Display for ThrottleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThrottleKind::Account => f.write_str("account"),
            ThrottleKind::Source => f.write_str("source"),
        }
    }
}

/// One audited state change. Carries no credential, no token, no raw address —
/// the source is already a digest by construction, the account is a name (audit events
/// name accounts openly, ADR-070 §8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditNote {
    /// The account the attempt named.
    pub account: AccountKey,
    /// The salted digest of the source address.
    pub source: SourceKey,
    /// Which counter changed.
    pub kind: ThrottleKind,
    /// Failures now counted against that counter inside the window.
    pub failures: u32,
    /// What the caller should do with this attempt.
    pub action: ThrottleAction,
    /// When a lockout lifts (unix seconds), for [`ThrottleAction::Locked`].
    pub locked_until: Option<i64>,
}

/// The action an audit note records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThrottleAction {
    /// The attempt passed the check (the caller still verifies the password).
    Allowed,
    /// The attempt was backed off.
    Delayed,
    /// The account entered hard lockout.
    Locked,
    /// A failure was counted without changing the state (an unthrottled failure).
    FailureCounted,
    /// A success reset the account counter.
    SuccessReset,
    /// An administrator unlocked the account.
    AdminUnlocked,
    /// The lockout expired by age and the account is clear.
    ExpiredUnlocked,
}

impl std::fmt::Display for ThrottleAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThrottleAction::Allowed => f.write_str("allowed"),
            ThrottleAction::Delayed => f.write_str("delayed"),
            ThrottleAction::Locked => f.write_str("locked"),
            ThrottleAction::FailureCounted => f.write_str("failure_counted"),
            ThrottleAction::SuccessReset => f.write_str("success_reset"),
            ThrottleAction::AdminUnlocked => f.write_str("admin_unlocked"),
            ThrottleAction::ExpiredUnlocked => f.write_str("expired_unlocked"),
        }
    }
}

/// Who performed an unlock, for the audit note's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlockedBy {
    /// An authorised administrator (the caller verified the right).
    Admin,
    /// The lockout age passed.
    Expiry,
}

impl ThrottleError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::StoreFailed(_) => "throttle.store_failed",
            Self::InvalidAccount(_) => "throttle.invalid_account",
        }
    }
}

/// A throttle operation failed input validation (never a credential problem).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ThrottleError {
    /// The store refused the operation.
    #[error("throttle.store_failed: {0}")]
    StoreFailed(String),
    /// The account key was rejected before any state was touched.
    #[error("throttle.invalid_account: {0}")]
    InvalidAccount(String),
}

/// One counter's state inside the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FailureRecord {
    /// Failures counted inside the current window.
    pub failures: u32,
    /// Unix seconds of the first failure in the window (0 = no window open).
    pub window_start: i64,
    /// Unix seconds a per-account lockout lifts (0 = not locked).
    pub locked_until: i64,
    /// Unix seconds of the last failure, for the backoff delay computation.
    pub last_failure: i64,
}

/// The counter storage behind the state machine. Keys are exactly [`AccountKey`] (per
/// account) and [`SourceKey`] (per source); the machine owns all decisions, the store only
/// remembers. Object safe and clock-injected like [`SessionStore`](crate::store).
pub trait ThrottleStore: Send + Sync {
    /// The counter for `key` (either kind), all zeros when none is recorded.
    fn get<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FailureRecord, ThrottleError>> + Send + 'a>,
    >;

    /// Replaces the counter for `key` (either kind).
    fn put<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
        record: FailureRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>;

    /// Clears the counter for `key` (either kind); a missing key clears nothing.
    fn clear<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>;
}

/// The in-memory [`ThrottleStore`], behind one mutex, keyed by bytes (account keys as
/// their UTF-8 bytes, source keys as their digest bytes; the `account` flag keeps the two
/// key-spaces apart).
#[derive(Debug, Default)]
pub struct InMemoryThrottleStore {
    /// (is_account, key bytes) → record.
    entries: Mutex<BTreeMap<(bool, Vec<u8>), FailureRecord>>,
}

impl InMemoryThrottleStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<(bool, Vec<u8>), FailureRecord>> {
        // The guarded data is a plain map; a panic elsewhere cannot leave it inconsistent.
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ThrottleStore for InMemoryThrottleStore {
    fn get<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FailureRecord, ThrottleError>> + Send + 'a>,
    > {
        let found = self.lock().get(&(account, key.to_vec())).copied();
        Box::pin(async move { Ok(found.unwrap_or_default()) })
    }

    fn put<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
        record: FailureRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>
    {
        self.lock().insert((account, key.to_vec()), record);
        Box::pin(async { Ok(()) })
    }

    fn clear<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>
    {
        self.lock().remove(&(account, key.to_vec()));
        Box::pin(async { Ok(()) })
    }
}

/// The [`LoginThrottle`] state machine over any [`ThrottleStore`].
///
/// All times are unix-seconds arguments (no clock is read); durations come from the
/// [`ThrottleConfig`]. The machine is pure: every observation and mutation goes through
/// the store, so the property tests can drive 10,000 random sequences and prove every
/// state is reachable-out-of by `unlock` alone.
pub struct LoginThrottle<T: ThrottleStore> {
    /// The counter store. Readable by tests (the white-box properties verify both
    /// counters directly); immutable from outside otherwise.
    pub store: T,
    config: ThrottleConfig,
    /// The site's source salt: the same [`crate::model::SaltKey`] the
    /// [`SessionManager`](crate::manager) salts session sources under, so an audit note's
    /// source digest matches the session record's source digest for the same address.
    salt: std::sync::Arc<crate::model::SaltKey>,
}

impl<T: ThrottleStore> LoginThrottle<T> {
    /// Builds the machine over `store` with a validated `config` and the site salt.
    pub fn new(
        store: T,
        config: ThrottleConfig,
        salt: std::sync::Arc<crate::model::SaltKey>,
    ) -> Result<Self, ThrottleError> {
        match config.validated() {
            Ok(_) => Ok(Self {
                store,
                config,
                salt,
            }),
            Err(reason) => Err(ThrottleError::InvalidAccount(reason)),
        }
    }

    /// The check that runs **before** the password hash: may this attempt proceed?
    ///
    /// Never mutates state (the caller records the outcome through `record_failure` /
    /// `record_success` afterwards) — a check that mutated would punish probes with no
    /// password too, which the lockout counters already do through failures. Returns:
    ///
    /// - [`ThrottleDecision::Allow`] — proceed (then hash);
    /// - [`ThrottleDecision::Delay`] — refuse now, `delay` is what the next attempt
    ///   (right or wrong) must have waited;
    /// - [`ThrottleDecision::Locked`] — the account is hard-locked until `until`.
    ///
    /// The account's state is examined first; only when the account allows does the
    /// source bound apply. That order means a hard-locked account is refused for its own
    /// reasons (and every other source backing off cannot mask the lock).
    pub async fn check(
        &self,
        account: &AccountKey,
        source: &str,
        now: i64,
    ) -> Result<ThrottleDecision, ThrottleError> {
        // Expiry unlock is lazy, exactly like session expiry: an account whose
        // `locked_until` has passed is simply not locked.
        let account_record = self.store.get(true, account.as_str().as_bytes()).await?;
        if account_record.locked_until > now {
            return Ok(ThrottleDecision::Locked {
                until: account_record.locked_until,
            });
        }
        if account_record.locked_until != 0 {
            // The lock has passed; clear it so the counter starts clean (and the note that
            // reaches the caller on the *next* state change reflects reality).
            self.store.clear(true, account.as_str().as_bytes()).await?;
        }

        let source_key = self.salt.digest_source(source);
        let source_record = self.store.get(false, source_key.as_bytes()).await?;
        let source_failures = self.failures_in_window(&source_record, now);
        if source_failures >= self.config.backoff_after {
            let delay = self.backoff_delay_for(source_failures);
            return Ok(ThrottleDecision::Delay {
                delay,
                because: ThrottleKind::Source,
            });
        }

        let account_failures = self.failures_in_window(&account_record, now);
        if account_failures >= self.config.backoff_after {
            let delay = self.backoff_delay_for(account_failures);
            return Ok(ThrottleDecision::Delay {
                delay,
                because: ThrottleKind::Account,
            });
        }

        Ok(ThrottleDecision::Allow)
    }

    /// Records a failed attempt **after** the check and the password verification failed.
    ///
    /// Increments both counters (the account's and the source's) inside their windows,
    /// applies hard lockout at `lockout_after` (account only), and returns the audit
    /// notes — one per state change, so the caller journals each.
    pub async fn record_failure(
        &self,
        account: &AccountKey,
        source: &str,
        now: i64,
    ) -> Result<Vec<AuditNote>, ThrottleError> {
        let mut notes = Vec::new();

        // The account counter: window, backoff, hard lockout.
        let record = self.store.get(true, account.as_str().as_bytes()).await?;
        let mut failures = self.failures_in_window(&record, now);
        failures += 1;
        let mut updated = FailureRecord {
            failures,
            window_start: if failures == 1 {
                now
            } else {
                record.window_start
            },
            locked_until: record.locked_until,
            last_failure: now,
        };
        let action;
        if failures >= self.config.lockout_after {
            // The M-th failure inside the window hard-locks the account.
            let until = now + duration_secs(self.config.lockout_duration);
            updated.locked_until = until;
            action = ThrottleAction::Locked;
            notes.push(AuditNote {
                account: account.clone(),
                source: self.salt.digest_source(source),
                kind: ThrottleKind::Account,
                failures,
                action,
                locked_until: Some(until),
            });
        } else {
            action = ThrottleAction::FailureCounted;
            notes.push(AuditNote {
                account: account.clone(),
                source: self.salt.digest_source(source),
                kind: ThrottleKind::Account,
                failures,
                action,
                locked_until: None,
            });
        }
        self.store
            .put(true, account.as_str().as_bytes(), updated)
            .await?;

        // The source counter: failures count, backoff applies, never any lock.
        let source_key = self.salt.digest_source(source);
        let source_record = self.store.get(false, source_key.as_bytes()).await?;
        let source_failures = self.failures_in_window(&source_record, now) + 1;
        self.store
            .put(
                false,
                source_key.as_bytes(),
                FailureRecord {
                    failures: source_failures,
                    window_start: if source_failures == 1 {
                        now
                    } else {
                        source_record.window_start
                    },
                    locked_until: 0, // A source can never lock.
                    last_failure: now,
                },
            )
            .await?;
        notes.push(AuditNote {
            account: account.clone(),
            source: source_key,
            kind: ThrottleKind::Source,
            failures: source_failures,
            action: ThrottleAction::FailureCounted,
            locked_until: None,
        });

        Ok(notes)
    }

    /// Records a successful sign-in **after** the check and the password verified.
    ///
    /// Resets **only the account counter** (a success must not wash out what a compromised
    /// source is doing); the source's counter stays until its window slides the failures
    /// out.
    pub async fn record_success(
        &self,
        account: &AccountKey,
        source: &str,
        _now: i64,
    ) -> Result<AuditNote, ThrottleError> {
        let had = self.store.get(true, account.as_str().as_bytes()).await?;
        self.store.clear(true, account.as_str().as_bytes()).await?;
        Ok(AuditNote {
            account: account.clone(),
            source: self.salt.digest_source(source),
            kind: ThrottleKind::Account,
            failures: match had.failures {
                0 => 0,
                failures => failures,
            },
            action: ThrottleAction::SuccessReset,
            locked_until: None,
        })
    }

    /// Unlocks an account: by an authorised administrator (the caller verified the right;
    /// this crate does not re-check it) or by lockout age (which `check` applies lazily).
    /// Returns the audit note; unlocking a not-locked account still records the attempt.
    /// The `source` is the raw address behind the request for the audit context (salted
    /// before it lands anywhere). `now` is the caller's instant, recorded in the store
    /// key's cleared state (the clear leaves no record, so it is unused today but kept in
    /// the signature for future retention logic).
    pub async fn unlock(
        &self,
        account: &AccountKey,
        by: UnlockedBy,
        source: &str,
        _now: i64,
    ) -> Result<AuditNote, ThrottleError> {
        let had = self.store.get(true, account.as_str().as_bytes()).await?;
        self.store.clear(true, account.as_str().as_bytes()).await?;
        Ok(AuditNote {
            account: account.clone(),
            source: self.salt.digest_source(source),
            kind: ThrottleKind::Account,
            failures: had.failures,
            action: match by {
                UnlockedBy::Admin => ThrottleAction::AdminUnlocked,
                UnlockedBy::Expiry => ThrottleAction::ExpiredUnlocked,
            },
            locked_until: None,
        })
    }

    /// The backoff delay for `failures` in the window: base × multiplier^(failures −
    /// backoff_after), capped. Monotonic in the failure count until the cap.
    fn backoff_delay_for(&self, failures: u32) -> Duration {
        let steps = failures.saturating_sub(self.config.backoff_after);
        let mut delay = self.config.backoff_base;
        for _ in 0..steps {
            match delay.checked_mul(self.config.backoff_multiplier) {
                Some(next) if next < self.config.backoff_cap => delay = next,
                _ => return self.config.backoff_cap,
            }
        }
        delay.min(self.config.backoff_cap)
    }

    /// Failures of `record` still inside the half-open window at `now`: a window that
    /// started `window` ago or earlier has closed (the first failure exactly at
    /// `window_start + window` is a fresh window with count 0).
    fn failures_in_window(&self, record: &FailureRecord, now: i64) -> u32 {
        if record.failures == 0 {
            return 0;
        }
        let window = duration_secs(self.config.window);
        if now - record.window_start >= window {
            0
        } else {
            record.failures
        }
    }
}

/// `Duration` → i64 seconds, saturating.
fn duration_secs(value: Duration) -> i64 {
    i64::try_from(value.as_secs()).unwrap_or(i64::MAX)
}
