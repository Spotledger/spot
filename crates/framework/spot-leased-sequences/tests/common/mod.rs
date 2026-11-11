//! Shared fixtures for the spot-leased-sequences tests: the same shape as the hub-side
//! spot-naming fixtures, minus the storage backend (a leased allocator has none).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_app_api::Document;
use spot_app_api::prelude::{Date, Month, Name, OffsetDateTime, RecordId, SiteId};
use spot_naming::{Namer, NamingContext, StaticPlans};
use spot_partition::{BranchSeries, LowWater, MemorySeriesStore, RangeLease};
use spot_plan::NamingPlan;
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;

/// The series DocType almost every test names.
pub const SERIES_DOCTYPE: &str = "sales_invoice";

/// A calendar date.
pub fn date(year: i32, month: Month, day: u8) -> Date {
    Date::from_calendar_date(year, month, day).unwrap()
}

/// An instant in UTC on the given date and time.
pub fn at(year: i32, month: Month, day: u8, hour: u8, minute: u8) -> OffsetDateTime {
    date(year, month, day)
        .with_hms(hour, minute, 0)
        .unwrap()
        .assume_utc()
}

/// The test site.
pub fn site() -> SiteId {
    SiteId::new("acme.spotledger.app").unwrap()
}

/// Parses a naming pattern the way `spot-plan` does.
pub fn plan(pattern: &str) -> NamingPlan {
    NamingPlan::parse(pattern).unwrap()
}

/// A registry with the series DocType the tests name.
pub fn plans() -> StaticPlans {
    StaticPlans::new().with(SERIES_DOCTYPE, plan("INV-.YYYY-.#####"))
}

/// A namer over [`plans`].
pub fn namer() -> Namer {
    Namer::new(Arc::new(plans()))
}

/// A naming context for `now` in the instant's own (UTC) zone, as the hub-side fixtures do.
pub fn ctx(now: OffsetDateTime) -> NamingContext {
    NamingContext::new(site(), now, now.offset())
}

/// A new, unsaved document of `doctype`.
pub fn doc(doctype: &str) -> Document {
    Document::new(name(doctype), RecordId::new_v7())
}

/// A storage [`Name`] for a DocType.
pub fn name(doctype: &str) -> Name {
    Name::new(doctype).unwrap()
}

/// A series key.
pub fn series(key: &str) -> SeriesKey {
    SeriesKey::new(key).unwrap()
}

/// A node identity for a lease.
pub fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

/// A branch series over `lease`, with a fresh high-water store and a half-low threshold.
pub fn branch(lease: RangeLease) -> BranchSeries {
    BranchSeries::install(lease, LowWater::half(), Arc::new(MemorySeriesStore::new())).unwrap()
}

/// Drives a future to completion.
///
/// Every future this crate returns is ready on the first poll (there is no I/O and no lock is
/// held across an await), and the namer only awaits those, so a no-op waker is enough and the
/// tests need no async runtime dependency. The loop yields rather than spinning the CPU should
/// a future ever return `Pending`.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
