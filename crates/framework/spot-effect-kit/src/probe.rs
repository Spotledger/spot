//! How the kit looks inside an app's append-only stores (the `StoreProbe`).
//!
//! The engine never reads a balance or movement store (ADR-080 6.9), so the kit cannot either;
//! the app implements this small read-only trait over its own stores, and the kit measures with
//! it before and after each operation.

use std::collections::BTreeMap;

use spot_app_api::prelude::{Decimal, RecordId};
use spot_app_api::{AppResult, BoxFuture, OpaqueRef, StoreName};

/// Which rows of a store belong to the document under test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreFilter {
    /// The DocType of the source document.
    pub doctype: String,
    /// The source document.
    pub id: RecordId,
}

/// Read access to the append-only stores an app declares effects on.
pub trait StoreProbe: Send + Sync {
    /// Every store the app keeps. The kit watches all of them, so a write to a store that no
    /// step declared is caught too.
    fn stores(&self) -> Vec<StoreName>;

    /// The net aggregate of the filtered rows: for a balance store, the sum of its signed
    /// amounts. A store with no amount reports zero.
    fn sum<'a>(
        &'a self,
        store: &'a StoreName,
        filter: &'a StoreFilter,
    ) -> BoxFuture<'a, AppResult<Decimal>>;

    /// How many filtered rows the store holds. Rows are append-only, so this only ever grows.
    ///
    /// The kit detects a deletion by the count falling, so an operation that deletes one row and
    /// inserts another leaves the count unchanged and is not caught by it (the aggregate and the
    /// receipt checks still see a changed sum or an unreported write). That is inherent to a
    /// count-based probe.
    fn count<'a>(
        &'a self,
        store: &'a StoreName,
        filter: &'a StoreFilter,
    ) -> BoxFuture<'a, AppResult<u64>>;

    /// The rows that the reversing (cancel) rows of the filtered document point at. `None`
    /// (the default) means the app does not record the link and the kit skips the check that a
    /// cancel references the records it reverses, and that the cancel receipt names the rows it
    /// appended: both checks are opt-in through this method.
    fn reversal_targets<'a>(
        &'a self,
        store: &'a StoreName,
        filter: &'a StoreFilter,
    ) -> BoxFuture<'a, AppResult<Option<Vec<OpaqueRef>>>> {
        let _ = (store, filter);
        Box::pin(async { Ok(None) })
    }
}

/// What a store held at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) sum: Decimal,
    pub(crate) rows: u64,
}

/// Every store's reading at one moment.
pub(crate) type Snapshot = BTreeMap<StoreName, Reading>;

pub(crate) async fn snapshot(probe: &dyn StoreProbe, filter: &StoreFilter) -> AppResult<Snapshot> {
    let mut out = Snapshot::new();
    for store in probe.stores() {
        let sum = probe.sum(&store, filter).await?;
        let rows = probe.count(&store, filter).await?;
        out.insert(store, Reading { sum, rows });
    }
    Ok(out)
}
