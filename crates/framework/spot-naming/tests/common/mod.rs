//! Shared fixtures for the spot-naming tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_app_api::Document;
use spot_core::SiteId;
use spot_naming::{Namer, NamingContext, StaticPlans};
use spot_plan::NamingPlan;
use spot_storage_contract::contract_tests::catalog;
use spot_storage_contract::{Catalog, Isolation, Name, RecordId, StorageBackend, TxOptions};
use spot_storage_fake::{FakeBackend, FakeTx};
use time::{Date, Month, OffsetDateTime, UtcOffset};

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

/// A fixed UTC offset, for a site's time zone.
pub fn zone(hours: i8, minutes: i8) -> UtcOffset {
    UtcOffset::from_hms(hours, minutes, 0).unwrap()
}

/// The test site.
pub fn site() -> SiteId {
    SiteId::new("acme.spotledger.app").unwrap()
}

/// Parses a naming pattern the way `spot-plan` does.
pub fn plan(pattern: &str) -> NamingPlan {
    NamingPlan::parse(pattern).unwrap()
}

/// A registry with one DocType per strategy.
pub fn plans() -> StaticPlans {
    StaticPlans::new()
        .with(SERIES_DOCTYPE, plan("INV-.YYYY-.#####"))
        .with("by_field", plan("field:code"))
        .with("by_hash", plan("hash"))
        .with("by_prompt", plan("prompt"))
}

/// A namer over [`plans`].
pub fn namer() -> Namer {
    Namer::new(Arc::new(plans()))
}

/// A naming context for `now` in `zone`.
pub fn ctx(now: OffsetDateTime, zone: UtcOffset) -> NamingContext {
    NamingContext::new(site(), now, zone)
}

/// A new, unsaved document of `doctype`.
pub fn doc(doctype: &str) -> Document {
    Document::new(Name::new(doctype).unwrap(), RecordId::new_v7())
}

/// A storage [`Name`] for a DocType.
pub fn name(doctype: &str) -> Name {
    Name::new(doctype).unwrap()
}

/// The storage catalog the fake runs against.
pub fn cat() -> Arc<Catalog> {
    Arc::new(catalog().unwrap())
}

/// Read-write transaction options.
pub fn options() -> TxOptions {
    TxOptions::new(Name::new("naming").unwrap(), Isolation::ReadCommitted)
}

/// A fresh empty backend.
pub fn backend() -> FakeBackend {
    FakeBackend::new()
}

/// Opens a transaction on `backend`.
pub async fn begin(backend: &FakeBackend) -> FakeTx {
    backend.begin(cat(), options()).await.unwrap()
}

/// Sets a text field on a document.
pub fn set_text(doc: &mut Document, field: &str, value: &str) {
    doc.set(
        Name::new(field).unwrap(),
        spot_storage_contract::Value::Text(value.to_owned()),
    );
}
