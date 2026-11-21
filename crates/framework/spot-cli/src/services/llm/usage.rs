//! Usage accounting helpers: the ledger the router records into (kept in memory for sums, with an
//! outbox the gateway writes to the database after each call), the budget period arithmetic and
//! the router's clock adapter.
//!
//! Money is integer micro-units everywhere: [`spot_llm_router::cost`] is the only multiplication
//! and it rounds half up. No float appears in this module.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use spot_core::Clock as CoreClock;
use spot_llm::BoxFuture;
use spot_llm_router::{
    Clock as RouterClockTrait, MemoryLedger, Price, SumQuery, TokenSums, UsageLedger, UsageRecord,
    cost,
};
use time::OffsetDateTime;

use super::config::Period;

/// The ledger the router records into. It sums in memory (the router's own reads) and keeps every
/// newly recorded request in an outbox until the gateway has written it to the database.
#[derive(Debug, Default)]
pub struct CapturingLedger {
    inner: MemoryLedger,
    // Held across the count-and-record pair so a duplicate request id is never queued twice.
    gate: Mutex<()>,
    outbox: Mutex<Vec<UsageRecord>>,
}

impl CapturingLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the records not yet written, oldest first.
    pub fn drain(&self) -> Vec<UsageRecord> {
        std::mem::take(&mut *self.outbox.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Puts records back after a failed write, so the next flush retries them.
    pub fn requeue(&self, records: Vec<UsageRecord>) {
        let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
        let mut restored = records;
        restored.append(&mut outbox);
        *outbox = restored;
    }

    /// How many records wait for a write.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl UsageLedger for CapturingLedger {
    fn record(&self, record: UsageRecord) {
        let _gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let before = self.inner.records_count();
        self.inner.record(record.clone());
        if self.inner.records_count() > before {
            self.outbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(record);
        }
    }

    fn sum(&self, query: &SumQuery) -> TokenSums {
        self.inner.sum(query)
    }
}

/// The cost of one recorded request at `price`, in micro-units (an exact integer, half up).
#[must_use]
pub fn record_cost(record: &UsageRecord, price: Price) -> u64 {
    u64::try_from(cost(record.priced_input(), record.output, &price)).unwrap_or(u64::MAX)
}

/// The first UTC day (days since the Unix epoch) of the period that contains `now_ms`. Two calls
/// in the same period return the same day: it is the period's key, and the budget starts again
/// from zero when it changes.
#[must_use]
pub fn period_start_day(period: Period, now_ms: u64) -> u64 {
    let day = now_ms / 86_400_000;
    match period {
        Period::Day => day,
        Period::Month => {
            let secs = i64::try_from(now_ms / 1_000).unwrap_or(0);
            let Ok(at) = OffsetDateTime::from_unix_timestamp(secs) else {
                return day;
            };
            let first = at.date().replace_day(1);
            let epoch = time::Date::from_calendar_date(1970, time::Month::January, 1);
            match (first, epoch) {
                (Ok(first), Ok(epoch)) => {
                    let days = i64::from(first.to_julian_day()) - i64::from(epoch.to_julian_day());
                    u64::try_from(days).unwrap_or(day)
                }
                _ => day,
            }
        }
    }
}

/// The wall clock of the host, read by the router in milliseconds. It never reads the system
/// time on its own: the injected clock decides, so a test moves it.
#[derive(Clone)]
pub struct RouterClock(pub Arc<dyn CoreClock>);

impl RouterClockTrait for RouterClock {
    fn now_millis(&self) -> u64 {
        let nanos = self.0.now_utc().unix_timestamp_nanos();
        u64::try_from(nanos / 1_000_000).unwrap_or(0)
    }

    fn wait(&self, millis: u64) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(millis)))
    }
}

/// The host clock in milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms(clock: &dyn CoreClock) -> u64 {
    u64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn record(id: &str) -> UsageRecord {
        UsageRecord {
            at: 1,
            tenant: "a".into(),
            task: "t".into(),
            provider: "p".into(),
            model: "m".into(),
            input: 1_000_000,
            output: 500_000,
            cache_read: 0,
            cache_write: 0,
            request_id: id.into(),
        }
    }

    #[test]
    fn a_duplicate_request_id_is_queued_once() {
        let ledger = CapturingLedger::new();
        ledger.record(record("a:t:1"));
        ledger.record(record("a:t:1"));
        ledger.record(record("a:t:2"));
        assert_eq!(ledger.pending(), 2);
        let drained = ledger.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(ledger.pending(), 0);
        ledger.requeue(drained);
        assert_eq!(ledger.pending(), 2);
        assert_eq!(ledger.sum(&SumQuery::tenant("a")).requests, 2);
    }

    #[test]
    fn the_cost_is_an_exact_integer() {
        // 1M input at 3_000_000 and 0.5M output at 15_000_000 micro per million.
        let price = Price::new(3_000_000, 15_000_000);
        assert_eq!(record_cost(&record("x"), price), 10_500_000);
    }

    #[test]
    fn periods_start_on_the_first_day() {
        // 2026-11-21 12:00 UTC.
        let now = 1_795_262_400_000u64;
        let day = period_start_day(Period::Day, now);
        assert_eq!(day, now / 86_400_000);
        let month = period_start_day(Period::Month, now);
        assert_eq!(day - month, 20);
        assert_eq!(period_start_day(Period::Month, month * 86_400_000), month);
    }
}
