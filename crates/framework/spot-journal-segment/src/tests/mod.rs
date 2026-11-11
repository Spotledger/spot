//! Tests of the segment format.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod chain_tests;
mod format_tests;
mod seal_tests;

use crate::{SealedSegment, SegmentConfig, SegmentWriter};
use spot_events_contract::envelope::Position;

pub(crate) fn pos(generation: u64, seq: u64) -> Position {
    Position { generation, seq }
}

pub(crate) fn line(seq: u64) -> String {
    format!("{{\"seq\":{seq},\"kind\":\"doc_state\",\"data\":\"entry number {seq}\"}}")
}

/// A segment holding `first..first + n` of `generation`.
pub(crate) fn build(
    source: &str,
    generation: u64,
    first: u64,
    n: u64,
    prev: Option<[u8; 32]>,
    block_target: usize,
) -> SealedSegment {
    let mut cfg = match prev {
        Some(p) => SegmentConfig::after(source, generation, p),
        None => SegmentConfig::first(source, generation),
    };
    cfg.block_target = block_target;
    let mut w = SegmentWriter::new(cfg).expect("config");
    for s in first..first + n {
        w.push(pos(generation, s), line(s).as_bytes())
            .expect("push");
    }
    w.seal(["event/1", "event/2"]).expect("seal")
}
