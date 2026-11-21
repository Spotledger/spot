//! The memory fake passes the same contract suite as every backend.
#![allow(missing_docs)]

use spot_sod::MemorySodStore;
use spot_sod::contract::{block_on, run_all};

#[test]
fn the_memory_store_passes_the_contract_suite() {
    block_on(run_all(MemorySodStore::new));
}
