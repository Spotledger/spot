//! The in-memory stores pass the same contract suites as the PostgreSQL ones.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_mail_intake::contract::{cursor_store_contract, seen_store_contract};
use spot_mail_intake::memory::{MemoryCursorStore, MemorySeenStore};

#[tokio::test]
async fn memory_cursor_store_meets_the_contract() {
    cursor_store_contract(&MemoryCursorStore::new(), "mem").await;
}

#[tokio::test]
async fn memory_seen_store_meets_the_contract() {
    seen_store_contract(&MemorySeenStore::new(), "mem").await;
}
