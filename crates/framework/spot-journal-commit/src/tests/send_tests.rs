//! The futures of the driver can be spawned on a multi-threaded runtime.

use spot_journal_relay::Relay;
use spot_journal_store::MemoryStore;

use super::support::{REMOTE_DEGRADE, rig};
use crate::{ChunkRelay, CommitDriver, Runtime};

fn assert_send<T: Send>(_: &T) {}

/// Compiles only when `run` and `step` are `Send` for every relay and runtime, which needs the
/// `Send` futures of [`ChunkRelay`].
fn futures_are_send_for_any_relay<R: ChunkRelay, T: Runtime>(driver: &CommitDriver<R, T>) {
    assert_send(&driver.run());
    assert_send(&driver.step());
}

fn is_a_chunk_relay<R: ChunkRelay>() {}

#[test]
fn the_run_future_is_send_for_a_test_relay() {
    let rig = rig(REMOTE_DEGRADE);
    futures_are_send_for_any_relay(&rig.driver);
    assert_send(&rig.driver.run());
}

#[test]
fn the_real_relay_over_any_store_is_a_send_chunk_relay() {
    is_a_chunk_relay::<Relay<MemoryStore>>();
}
