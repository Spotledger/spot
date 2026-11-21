//! The retention purge of held payload references (WP-MIG-08.T6).
//!
//! A held effect keeps a reference to its payload (the mail body, the webhook body, the connector
//! request) until it is released or discarded; after that nothing used to delete it. A discarded
//! mail body with a customer's address would stay in the ledger indefinitely. [`purge_expired`]
//! replaces the reference of an effect that was **released** (dispatched) more than
//! [`RetentionPolicy::released_days`] ago, or **discarded** more than
//! [`RetentionPolicy::discarded_days`] ago, with the empty marker, and **keeps the row**, so
//! deduplication on `(site, key)` and the checkpoint arithmetic are unchanged. A `held`,
//! `releasing` or `needs_confirmation` effect is never touched, whatever its age.
//!
//! The purge is irreversible and bounded: at most `limit` references per call (default
//! [`DEFAULT_LIMIT`]), so it never holds a long transaction; [`PurgeReport::more`] tells the caller
//! to call again. Time is an argument; this module reads no clock. Scheduling it is the host's job.

use crate::effect::EffectState;
use crate::error::{Error, Result};
use crate::payload::{PayloadStore, drain_payload_deletes};
use crate::store::LedgerStore;
use spot_core::SiteId;

/// Microseconds in a day.
const DAY_MICROS: i64 = 86_400_000_000;
/// The largest number of days a policy may name.
pub const MAX_DAYS: u32 = 3650;
/// References purged per call unless the caller says otherwise.
pub const DEFAULT_LIMIT: usize = 1_000;

/// How long a payload reference is kept after its effect finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    released_days: u32,
    discarded_days: u32,
}

impl RetentionPolicy {
    /// A policy; zero is refused (a payload is never purged in the same instant) and so is more than
    /// [`MAX_DAYS`].
    ///
    /// # Errors
    /// [`Error::RetentionInvalid`] (`effect_hold.retention_invalid`).
    pub fn new(released_days: u32, discarded_days: u32) -> Result<Self> {
        for days in [released_days, discarded_days] {
            if days == 0 {
                return Err(Error::RetentionInvalid {
                    reason: "a retention period must be at least one day".to_owned(),
                });
            }
            if days > MAX_DAYS {
                return Err(Error::RetentionInvalid {
                    reason: format!("a retention period must be at most {MAX_DAYS} days"),
                });
            }
        }
        Ok(Self {
            released_days,
            discarded_days,
        })
    }

    /// Days a released (dispatched) effect's reference is kept.
    #[must_use]
    pub fn released_days(&self) -> u32 {
        self.released_days
    }

    /// Days a discarded effect's reference is kept.
    #[must_use]
    pub fn discarded_days(&self) -> u32 {
        self.discarded_days
    }
}

impl Default for RetentionPolicy {
    /// 7 days after release, 30 after discard.
    fn default() -> Self {
        Self {
            released_days: 7,
            discarded_days: 30,
        }
    }
}

/// What one purge call did. Counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// References replaced by the empty marker in this call.
    pub purged: u64,
    /// Whether the call stopped at its limit, so another call may purge more.
    pub more: bool,
    /// Stored payloads whose bytes were deleted in this call (always `0` from [`purge_expired`],
    /// which only clears and queues; see [`purge_expired_with_payloads`]).
    pub payloads_deleted: u64,
}

/// Purges the expired payload references of `site` (irreversible).
///
/// `limit` bounds the work per call; with `limit` of zero nothing is purged. The released and the
/// discarded effects each get up to `limit`.
///
/// # Errors
/// The ledger's error (`effect_hold.ledger_unavailable`, including a ledger that cannot purge).
pub async fn purge_expired(
    store: &dyn LedgerStore,
    site: &SiteId,
    policy: RetentionPolicy,
    now: i64,
    limit: usize,
) -> Result<PurgeReport> {
    if limit == 0 {
        return Ok(PurgeReport::default());
    }
    let released_before =
        now.saturating_sub(i64::from(policy.released_days).saturating_mul(DAY_MICROS));
    let discarded_before =
        now.saturating_sub(i64::from(policy.discarded_days).saturating_mul(DAY_MICROS));
    let released = store
        .purge_payloads(site, &[EffectState::Dispatched], released_before, limit)
        .await?;
    let discarded = store
        .purge_payloads(site, &[EffectState::Discarded], discarded_before, limit)
        .await?;
    let cap = u64::try_from(limit).unwrap_or(u64::MAX);
    Ok(PurgeReport {
        purged: released.saturating_add(discarded),
        more: released >= cap || discarded >= cap,
        payloads_deleted: 0,
    })
}

/// [`purge_expired`], then deletes the stored bytes of every queued handle no effect still
/// references (see [`crate::payload`]): the reference is cleared and the handle queued in one
/// ledger step, the bytes go second, so a crash between them leaves orphan bytes that the next
/// call deletes, never a reference to missing bytes. A payload shared by two holds is kept until
/// the last reference is purged. Counts only.
///
/// `limit` bounds both the references cleared and the byte deletions per call.
///
/// # Errors
/// The ledger's error or the byte store's; the references already cleared stay cleared and their
/// handles stay queued.
pub async fn purge_expired_with_payloads(
    store: &dyn LedgerStore,
    payloads: &dyn PayloadStore,
    site: &SiteId,
    policy: RetentionPolicy,
    now: i64,
    limit: usize,
) -> Result<PurgeReport> {
    let mut report = purge_expired(store, site, policy, now, limit).await?;
    // Drain after the purge, and also when it purged nothing: a previous call may have crashed
    // between clearing and deleting.
    report.payloads_deleted = drain_payload_deletes(store, payloads, site, limit).await?;
    Ok(report)
}
