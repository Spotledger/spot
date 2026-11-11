//! Adapters from the systems that already count things into this crate's meters.
//!
//! The LLM router keeps its own usage ledger keyed by request id; the directory knows
//! who signed in. Neither should learn to speak metering directly, so the translation
//! lives here.
//!
//! # The double-count guard
//!
//! [`AiTokensFromLedger::event_for_record`] derives each [`MeterEvent`]'s id from the
//! ledger's own `request_id`. The router's ledger drops a repeated `request_id` and so
//! does this crate's store, and because the two share the id the *same* retried
//! request can never be counted twice across both — whichever path records it first
//! wins, and the reconciliation between the two totals stays clean.

use spot_llm_router::{SumQuery, UsageLedger, UsageRecord};

use crate::error::MeteringError;
use crate::event::MeterEvent;
use crate::meter::{MemberDigest, Meter};
use crate::period::PeriodKey;

/// Turns AI-token usage into [`Meter::AiTokens`] facts.
///
/// The router's ledger sums by *day* ([`SumQuery::and_day`]) because that is what its
/// billing windows need; this adapter walks the UTC day keys a period covers, so the
/// two ledgers are compared over the same window.
#[derive(Clone, Copy, Debug, Default)]
pub struct AiTokensFromLedger;

impl AiTokensFromLedger {
    /// Exact total from day-only sums. Reject non-UTC-aligned boundaries rather than
    /// silently borrowing usage from adjacent tenant-zone periods. The upstream ledger
    /// exposes no range query or record enumeration; use `total_for_records` for those.
    /// # Errors
    /// InvalidPeriodSpec when UTC days cannot express the period exactly.
    pub fn total_for_period(
        ledger: &dyn UsageLedger,
        query: &SumQuery,
        period: &PeriodKey,
    ) -> Result<u64, MeteringError> {
        if period.start < 0
            || period.start >= period.end_exclusive
            || period.start % 86_400 != 0
            || period.end_exclusive % 86_400 != 0
        {
            return Err(MeteringError::InvalidPeriodSpec {
                reason: "day-only UsageLedger cannot sum partial UTC days; supply records"
                    .to_owned(),
            });
        }
        let mut total = 0u64;
        for day in (period.start / 86_400)..(period.end_exclusive / 86_400) {
            let day = spot_llm_router::DayKey(day as u64);
            if query.day.is_some_and(|filter| filter != day) {
                continue;
            }
            total = total
                .checked_add(token_sum(&ledger.sum(&query.clone().and_day(day)))?)
                .ok_or(MeteringError::CounterOverflow)?;
        }
        Ok(total)
    }
    /// Exact tenant-zone total from caller-enumerated records, first request id wins
    /// just as in UsageLedger. No prompts, providers or model names are retained.
    /// # Errors
    /// CounterOverflow if an exact total cannot be represented.
    pub fn total_for_records(
        records: &[UsageRecord],
        query: &SumQuery,
        period: &PeriodKey,
    ) -> Result<u64, MeteringError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut total = 0u64;
        for record in records {
            if !seen.insert(&record.request_id) {
                continue;
            }
            if !period.contains(record_at_seconds(record))
                || query.tenant.as_ref().is_some_and(|v| v != &record.tenant)
                || query.task.as_ref().is_some_and(|v| v != &record.task)
                || query
                    .provider
                    .as_ref()
                    .is_some_and(|v| v != &record.provider)
                || query.model.as_ref().is_some_and(|v| v != &record.model)
                || query
                    .day
                    .is_some_and(|v| v != spot_llm_router::DayKey::from_millis(record.at))
            {
                continue;
            }
            let amount = record
                .input
                .checked_add(record.output)
                .and_then(|v| v.checked_add(record.cache_read))
                .and_then(|v| v.checked_add(record.cache_write))
                .ok_or(MeteringError::CounterOverflow)?;
            total = total
                .checked_add(amount)
                .ok_or(MeteringError::CounterOverflow)?;
        }
        Ok(total)
    }

    /// One ledger record as a metering event.
    ///
    /// The id is the record's `request_id`, so a retried
    /// settlement and a retried metering batch that carry the same request cannot
    /// double-count. The amount is every token the request consumed
    /// ([`token_sum`] of the record); the instant is the settle time in UTC seconds.
    /// # Errors
    /// CounterOverflow when the combined token amount cannot be represented exactly.
    pub fn event_for_record(record: &UsageRecord) -> Result<MeterEvent, MeteringError> {
        let amount = record
            .input
            .checked_add(record.output)
            .and_then(|v| v.checked_add(record.cache_read))
            .and_then(|v| v.checked_add(record.cache_write))
            .ok_or(MeteringError::CounterOverflow)?;
        Ok(MeterEvent::amount(
            record.request_id.clone(),
            record.tenant.clone(),
            Meter::AiTokens,
            amount,
            record_at_seconds(record),
        ))
    }
    /// Convert caller-enumerated records to an idempotent event batch.
    /// # Errors
    /// CounterOverflow for any unrepresentable amount.
    pub fn events_for_records(records: &[UsageRecord]) -> Result<Vec<MeterEvent>, MeteringError> {
        records.iter().map(Self::event_for_record).collect()
    }
}

/// Every token a request consumed: input, output and both cache directions.
///
/// This is the number a token quota charges against — the same accounting the
/// router's own [`spot_llm_router::TokenSums`] exposes, summed.
/// # Errors
/// CounterOverflow if the combined amount exceeds u64.
pub fn token_sum(sums: &spot_llm_router::TokenSums) -> Result<u64, MeteringError> {
    sums.input
        .checked_add(sums.output)
        .and_then(|v| v.checked_add(sums.cache_read))
        .and_then(|v| v.checked_add(sums.cache_write))
        .ok_or(MeteringError::CounterOverflow)
}

/// A ledger record's settle time as UTC seconds.
#[must_use]
fn record_at_seconds(record: &UsageRecord) -> i64 {
    i64::try_from(record.at / 1_000).unwrap_or(i64::MAX)
}

/// Document lifecycle observations. Reads, reversals and cancellations never charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentOperation {
    /// A successfully committed posting.
    Posted,
    /// A read, not usage of the transaction quota.
    Read,
    /// A corrective reversal.
    Reversal,
    /// A cancellation.
    Cancellation,
}
/// Caller-declared metered DocTypes. Names are schema identifiers, not document values.
#[derive(Clone, Debug)]
pub struct TransactionsFromDocuments {
    doctypes: std::collections::BTreeSet<String>,
}
impl TransactionsFromDocuments {
    /// Explicit allow-list; an empty list meters no documents.
    pub fn new(doctypes: impl IntoIterator<Item = String>) -> Self {
        Self {
            doctypes: doctypes.into_iter().collect(),
        }
    }
    /// A posted document counts once, under its producer-supplied digested event id.
    #[must_use]
    pub fn event(
        &self,
        id: &str,
        tenant: &str,
        doctype: &str,
        operation: DocumentOperation,
        at: i64,
    ) -> Option<MeterEvent> {
        (operation == DocumentOperation::Posted && self.doctypes.contains(doctype))
            .then(|| MeterEvent::amount(id, tenant, Meter::Transactions, 1, at))
    }
}

/// The directory of users, which the directory service implements later.
///
/// A distinct-set meter needs only a stable digest per user and the moment the user
/// was seen; the trait keeps the directory free of metering types and keeps this crate
/// free of a directory dependency.
pub trait UsersFromDirectory: Send + Sync {
    /// The digests of users active in `[start, end_exclusive)`. The host supplies
    /// a representative instant in that period when converting the population.
    ///
    /// Repeated digests are harmless. Enumeration failure must return an error,
    /// never an empty list that falsely reports no active users.
    fn active_users(
        &self,
        tenant: &str,
        start: i64,
        end_exclusive: i64,
    ) -> Result<Vec<MemberDigest>, MeteringError>;
}

/// Turns a directory's active users into [`Meter::ActiveUsers`] events.
///
/// Each member becomes one event with a per-member id derived from its digest and the
/// instant it was seen, so an identical batch replay adds nothing new while the next
/// day legitimately counts the member again.
#[must_use]
pub fn active_user_events(tenant: &str, members: &[MemberDigest], at: i64) -> Vec<MeterEvent> {
    members
        .iter()
        .map(|member| {
            MeterEvent::member(
                format!("user:{}:{at}", member.as_str()),
                tenant,
                Meter::ActiveUsers,
                member.clone(),
                at,
            )
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_llm_router::{MemoryLedger, UsageLedger};

    fn record(request_id: &str, at_millis: u64, input: u64, output: u64) -> UsageRecord {
        UsageRecord {
            at: at_millis,
            tenant: "acme".to_owned(),
            task: "chat".to_owned(),
            provider: "openai".to_owned(),
            model: "gpt".to_owned(),
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            request_id: request_id.to_owned(),
        }
    }

    #[test]
    fn a_record_becomes_an_event_keyed_by_the_request_id() {
        let event = AiTokensFromLedger::event_for_record(&record("req-1", 1_000, 100, 50)).unwrap();
        assert_eq!(event.id, "req-1");
        assert_eq!(event.meter, Meter::AiTokens);
        assert_eq!(event.amount, 150);
        assert_eq!(event.at, 1); // 1000 millis
    }

    #[test]
    fn the_event_id_prevents_double_counting_across_the_two_ledgers() {
        // The router drops a repeated request id; the metering store drops a repeated
        // event id. Sharing the id means a retry is a no-op in both.
        let first = AiTokensFromLedger::event_for_record(&record("req-9", 0, 10, 0)).unwrap();
        let second = AiTokensFromLedger::event_for_record(&record("req-9", 0, 10, 0)).unwrap();
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn the_period_total_sums_the_ledgers_days() {
        let ledger = MemoryLedger::new();
        ledger.record(record("r1", 0, 100, 0)); // 1970-01-01
        ledger.record(record("r2", 86_400_000, 200, 0)); // 1970-01-02
        let spec = crate::period::PeriodSpec::calendar_month("UTC").unwrap();
        let january = crate::period::period_of(0, &spec).unwrap();
        let total =
            AiTokensFromLedger::total_for_period(&ledger, &SumQuery::tenant("acme"), &january);
        assert_eq!(total.unwrap(), 300);
    }

    #[test]
    fn a_duplicated_record_is_counted_once_by_the_ledger() {
        let ledger = MemoryLedger::new();
        ledger.record(record("same", 0, 100, 0));
        ledger.record(record("same", 0, 100, 0));
        let spec = crate::period::PeriodSpec::calendar_month("UTC").unwrap();
        let january = crate::period::period_of(0, &spec).unwrap();
        assert_eq!(
            AiTokensFromLedger::total_for_period(&ledger, &SumQuery::all(), &january).unwrap(),
            100
        );
    }

    #[test]
    fn active_user_events_are_member_events_with_stable_ids() {
        let members = vec![MemberDigest::new("h1"), MemberDigest::new("h2")];
        let events = active_user_events("acme", &members, 0);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(MeterEvent::is_well_formed));
        assert!(events.iter().all(|event| event.meter == Meter::ActiveUsers));
        // Replaying the same day's batch reproduces the same ids (a no-op).
        let again = active_user_events("acme", &members, 0);
        assert_eq!(events[0].id, again[0].id);
    }
}
