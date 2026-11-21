//! The 1,000,000-row export: peak memory stays bounded (an `#[ignore]` load test for L9).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use common::{DOCTYPE, build, principal};
use spot_bulk_data::{
    BoxFuture, BulkLimits, ExportPlan, ExportSpec, ExportStream, PageReader, StreamContext,
};
use spot_storage_contract::{Query, RecordId, Rows, StorageResult, Value};

/// A reader that fabricates `total` rows, one page at a time, without storing them: it proves
/// the stream's memory is bounded by the page and the chunk, not the dataset.
struct SyntheticReader {
    plan: Arc<ExportPlan>,
    total: u64,
    delivered: AtomicU64,
    max_page: AtomicUsize,
}

impl SyntheticReader {
    fn row(&self, i: u64) -> Vec<Value> {
        let plan = &self.plan;
        let mut row = vec![Value::Null; plan.columns.len().max(2)];
        if let Some(at) = plan.parent_index("id") {
            row[at] = Value::Id(counter_id(i));
        }
        if let Some(at) = plan.parent_index("modified") {
            row[at] = Value::DateTime(
                time::OffsetDateTime::from_unix_timestamp(1_700_000_000 + i as i64).unwrap(),
            );
        }
        if let Some(at) = plan.parent_index("title") {
            row[at] = Value::Text(format!("row {i}"));
        }
        if let Some(at) = plan.parent_index("amount") {
            row[at] = Value::Decimal(rust_decimal::Decimal::new(100, 2));
        }
        row
    }
}

/// A UUIDv7-shaped id from a counter, so ids are unique and time-ordered without the uuid
/// generator.
fn counter_id(i: u64) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[8..].copy_from_slice(&i.to_be_bytes());
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    RecordId::from_bytes(bytes).unwrap()
}

impl PageReader for SyntheticReader {
    fn read<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>> {
        Box::pin(async move {
            let limit = u64::from(query.limit.unwrap_or(1_000));
            let delivered = self.delivered.load(Ordering::SeqCst);
            let take = limit.min(self.total.saturating_sub(delivered));
            self.delivered.store(delivered + take, Ordering::SeqCst);
            self.max_page.fetch_max(take as usize, Ordering::SeqCst);
            let columns = query.select.iter().map(|s| s.alias.clone()).collect();
            let rows = (delivered..delivered + take).map(|i| self.row(i)).collect();
            Ok(Rows { columns, rows })
        })
    }
}

#[tokio::test]
#[ignore = "1,000,000-row load test; run with --ignored for L9"]
async fn one_million_rows_move_within_bounded_memory() {
    let total = 1_000_000u64;
    let world = build(1).await;
    let spec = ExportSpec::csv(DOCTYPE).with_columns(&["id", "modified", "title", "amount"]);
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));
    let limits = BulkLimits {
        rows_per_chunk: 50_000,
        bytes_per_chunk: 8 * 1024 * 1024,
        ..BulkLimits::default()
    };
    let reader = Arc::new(SyntheticReader {
        plan: plan.clone(),
        total,
        delivered: AtomicU64::new(0),
        max_page: AtomicUsize::new(0),
    });
    let ctx = StreamContext {
        reader: reader.clone(),
        ..world.ctx("analyst", limits)
    };
    let started = Instant::now();
    let mut stream = ExportStream::start(plan, ctx).unwrap();
    let mut rows = 0u64;
    let mut max_chunk = 0usize;
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        rows += chunk.rows as u64;
        max_chunk = max_chunk.max(chunk.bytes.len());
        // The chunk never exceeds its byte bound by more than the one row that crossed it.
        assert!(
            chunk.bytes.len() <= limits.bytes_per_chunk + 512,
            "chunk of {} bytes exceeded the bound",
            chunk.bytes.len()
        );
    }
    let elapsed = started.elapsed();
    // Integer arithmetic only (ADR-12): rows per second without a float.
    let micros = u64::try_from(elapsed.as_micros())
        .unwrap_or(u64::MAX)
        .max(1);
    let rows_per_second = rows.saturating_mul(1_000_000) / micros;
    let page_rows = reader.max_page.load(Ordering::SeqCst);
    // A stated bound: one page of values plus one chunk of bytes, well under 64 MiB.
    let bound = limits.rows_per_chunk as usize * 256 + limits.bytes_per_chunk;
    let peak_estimate = page_rows * 256 + max_chunk;
    println!(
        "1,000,000 rows in {elapsed:?}: {rows_per_second} rows/s, \
         max chunk {max_chunk} bytes, max page {page_rows} rows, peak ~{peak_estimate} bytes"
    );
    assert_eq!(rows, total);
    assert_eq!(page_rows, limits.rows_per_chunk as usize);
    assert!(
        peak_estimate <= bound,
        "peak memory estimate {peak_estimate} over {bound}"
    );
}
