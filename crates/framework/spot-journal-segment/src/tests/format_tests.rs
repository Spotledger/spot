use super::{build, line, pos};
use crate::{
    ByteSource, FORMAT_VERSION, SegmentConfig, SegmentError, SegmentReader, SegmentWriter,
};
use proptest::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn collect(r: &SegmentReader<&[u8]>) -> Vec<(u64, String)> {
    r.entries()
        .map(|e| e.map(|e| (e.position.seq, e.line)))
        .collect::<Result<_, _>>()
        .expect("entries")
}

#[test]
fn round_trip_of_zero_one_and_many_entries() {
    for n in [0u64, 1, 100_000] {
        let s = build("t", 3, 1, n, None, 256 * 1024);
        let r = SegmentReader::open(s.bytes.as_slice()).expect("open");
        assert_eq!(r.header(), &s.header);
        assert_eq!(r.header().entry_count, n);
        let got = collect(&r);
        assert_eq!(got.len() as u64, n);
        for (i, (seq, l)) in got.iter().enumerate() {
            assert_eq!(*seq, 1 + i as u64);
            assert_eq!(l, &line(*seq));
        }
        r.verify_all().expect("verify");
    }
}

#[test]
fn the_writer_refuses_bad_positions_and_lines() {
    let new = || SegmentWriter::new(SegmentConfig::first("t", 1)).expect("cfg");
    let mut w = new();
    w.push(pos(1, 5), b"{}").expect("first");
    assert_eq!(
        w.push(pos(1, 5), b"{}"),
        Err(SegmentError::PositionOutOfOrder {
            generation: 1,
            seq: 5
        })
    );
    assert_eq!(
        w.push(pos(1, 4), b"{}"),
        Err(SegmentError::PositionOutOfOrder {
            generation: 1,
            seq: 4
        })
    );
    assert_eq!(
        w.push(pos(1, 7), b"{}"),
        Err(SegmentError::PositionGap {
            generation: 1,
            seq: 7
        })
    );
    assert_eq!(
        w.push(pos(2, 6), b"{}"),
        Err(SegmentError::GenerationMismatch {
            expected: 1,
            offered: 2
        })
    );
    assert!(matches!(
        w.push(pos(1, 6), b"a\nb"),
        Err(SegmentError::EntryInvalid(_))
    ));
    assert!(matches!(
        w.push(pos(1, 6), b"a\rb"),
        Err(SegmentError::EntryInvalid(_))
    ));
    assert!(matches!(
        w.push(pos(1, 6), &[0xff, 0xfe]),
        Err(SegmentError::EntryInvalid(_))
    ));
    assert_eq!(
        w.entry_count(),
        1,
        "a refused entry leaves the writer unchanged"
    );
    w.push(pos(1, 6), b"{}").expect("next");
    assert_eq!(w.entry_count(), 2);
}

#[test]
fn configuration_is_validated() {
    let mut c = SegmentConfig::first("t", 1);
    c.block_target = 0;
    assert!(matches!(
        SegmentWriter::new(c),
        Err(SegmentError::ConfigInvalid(_))
    ));
    assert!(matches!(
        SegmentWriter::new(SegmentConfig::first("", 1)),
        Err(SegmentError::ConfigInvalid(_))
    ));
    let mut c = SegmentConfig::first("t", 1);
    c.zstd_level = 1000;
    assert!(matches!(
        SegmentWriter::new(c),
        Err(SegmentError::ConfigInvalid(_))
    ));
}

#[test]
fn schema_versions_are_sorted_and_unique() {
    let mut w = SegmentWriter::new(SegmentConfig::first("t", 1)).expect("cfg");
    w.push(pos(1, 1), b"{}").expect("push");
    let s = w.seal(["b/2", "a/1", "b/2"]).expect("seal");
    assert_eq!(
        s.header.schema_versions,
        vec!["a/1".to_owned(), "b/2".to_owned()]
    );
}

#[test]
fn golden_three_entry_segment() {
    let mut w = SegmentWriter::new(SegmentConfig::first("tenant-a", 1)).expect("cfg");
    for (seq, l) in [(1, "{\"n\":1}"), (2, "{\"n\":2}"), (3, "{\"n\":3}")] {
        w.push(pos(1, seq), l.as_bytes()).expect("push");
    }
    let s = w.seal(["event/1"]).expect("seal");
    assert_eq!(&s.bytes[..8], b"SPOTJSEG");
    assert_eq!(&s.bytes[8..10], &FORMAT_VERSION.to_be_bytes());
    assert_eq!(&s.bytes[s.bytes.len() - 4..], b"SJND");
    assert_eq!(
        hex(&s.bytes),
        GOLDEN_HEX,
        "the segment bytes changed: this is a format break"
    );
    assert_eq!(hex(&s.hash), GOLDEN_HASH);
}

const GOLDEN_HEX: &str = concat!(
    "53504f544a53454700010000005b0000000874656e616e742d6100000000000000010000000000000001000000000000",
    "0003000000000000000300000001000000076576656e742f31eda08b418332369d7fe7a7d8912df00eb36b246dd6cba2",
    "b58885587753f168d380cf308c2a88dd7f431bbbaae075b8c7ec631060383a7865ab5c452a108a947a0000002a000000",
    "3928b52ffd20390d0100a0000001000000077b226e223a317d00023203337d0500a0082067b3b060fe05b011775eeebc",
    "360c9694e01a7f059c08a04e16e058bf9df6ecc038c9a1a3233661000000010000000000000001000000000000008900",
    "0000000000005200000003173d1614e23cabd39857db71b2cacf2e8385bd2fa5ade7f25cfb491d557a3d380000002053",
    "4a4e44",
);
const GOLDEN_HASH: &str = "6cf4f97c84ee4a0dc04567f7074b03400939830870830c1e5485d9e2498e80a7";

#[test]
fn reader_rejects_a_truncated_or_foreign_input() {
    let s = build("t", 1, 1, 20, None, 100);
    for cut in [0usize, 1, 13, 50, s.bytes.len() - 1, s.bytes.len() - 41] {
        assert!(
            SegmentReader::open(&s.bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }
    let mut longer = s.bytes.clone();
    longer.push(0);
    assert!(SegmentReader::open(longer.as_slice()).is_err());
    assert!(
        SegmentReader::open(&b"not a segment at all, but long enough to be looked at......"[..])
            .is_err()
    );
    let mut v = s.bytes.clone();
    v[9] = 2; // version 2
    assert!(matches!(
        SegmentReader::open(v.as_slice()),
        Err(SegmentError::UnsupportedVersion(_))
    ));
}

/// Opening and fully verifying a segment must fail after any single flipped byte.
fn flips_are_detected(bytes: &[u8]) {
    for i in 0..bytes.len() {
        for bit in [0x01u8, 0x80] {
            let mut v = bytes.to_vec();
            v[i] ^= bit;
            let ok = SegmentReader::open(v.as_slice()).and_then(|r| r.verify_all());
            assert!(
                ok.is_err(),
                "flip of bit {bit:#x} at byte {i} of {} went unnoticed",
                bytes.len()
            );
        }
    }
}

#[test]
fn flipping_any_single_byte_is_reported() {
    flips_are_detected(&build("t", 1, 1, 5, None, 40).bytes);
    flips_are_detected(&build("t", 1, 1, 0, None, 40).bytes);
    flips_are_detected(&build("t", 7, 100, 1, None, 40).bytes);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn any_byte_flip_in_a_random_segment_is_detected(n in 1u64..40, target in 16usize..400, at in any::<prop::sample::Index>(), bit in 0u8..8) {
        let s = build("tenant", 2, 10, n, None, target);
        let mut v = s.bytes.clone();
        let i = at.index(v.len());
        v[i] ^= 1 << bit;
        let ok = SegmentReader::open(v.as_slice()).and_then(|r| r.verify_all());
        prop_assert!(ok.is_err());
    }

    #[test]
    fn random_segments_round_trip(lines in prop::collection::vec("[ -~]{0,60}", 0..60), target in 1usize..300, first in 0u64..1000) {
        let mut cfg = SegmentConfig::first("s", 4);
        cfg.block_target = target;
        let mut w = SegmentWriter::new(cfg).expect("cfg");
        for (i, l) in lines.iter().enumerate() {
            w.push(pos(4, first + i as u64), l.as_bytes()).expect("push");
        }
        let s = w.seal(Vec::<String>::new()).expect("seal");
        let r = SegmentReader::open(s.bytes.as_slice()).expect("open");
        let got: Vec<String> = r.entries().map(|e| e.expect("entry").line).collect();
        prop_assert_eq!(got, lines.clone());
        if let (Some(a), Some(b)) = (lines.len().checked_div(3), lines.len().checked_sub(1)) {
            let part = r.read_range(first + a as u64, first + b as u64).expect("range");
            prop_assert_eq!(part.len(), b + 1 - a);
        }
    }
}

/// A source that counts which blocks were read.
struct Counting<'a> {
    data: &'a [u8],
    reads: RefCell<Vec<(u64, u64)>>,
    calls: Cell<usize>,
}

impl ByteSource for Counting<'_> {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&self, offset: u64, len: u64) -> Result<Cow<'_, [u8]>, SegmentError> {
        self.calls.set(self.calls.get() + 1);
        self.reads.borrow_mut().push((offset, len));
        let end =
            usize::try_from(offset + len).map_err(|_| SegmentError::FormatInvalid("range"))?;
        let start = usize::try_from(offset).map_err(|_| SegmentError::FormatInvalid("range"))?;
        self.data
            .get(start..end)
            .map(Cow::Borrowed)
            .ok_or(SegmentError::FormatInvalid("truncated"))
    }
}

#[test]
fn read_range_reads_only_the_blocks_it_needs() {
    let s = build("t", 1, 1, 400, None, 300);
    let src = Counting {
        data: &s.bytes,
        reads: RefCell::new(Vec::new()),
        calls: Cell::new(0),
    };
    let r = SegmentReader::open(src).expect("open");
    let index = r.block_index().to_vec();
    assert!(
        index.len() >= 10,
        "need several blocks, got {}",
        index.len()
    );
    let opened_calls = r.src_calls();
    // A range inside one block.
    let b = &index[4];
    let got = r.read_range(b.first_seq, b.first_seq).expect("range");
    assert_eq!(got.len(), 1);
    assert_eq!(
        r.block_reads_since(opened_calls),
        vec![(b.offset, b.length)]
    );
    // A range across two neighbouring blocks.
    let before = r.src_calls();
    let (b1, b2) = (&index[6], &index[7]);
    let got = r
        .read_range(b1.first_seq + u64::from(b1.count) - 1, b2.first_seq)
        .expect("range");
    assert_eq!(got.len(), 2);
    assert_eq!(
        r.block_reads_since(before),
        vec![(b1.offset, b1.length), (b2.offset, b2.length)]
    );
    // Nothing at all outside the segment.
    let before = r.src_calls();
    assert!(r.read_range(10_000, 20_000).expect("range").is_empty());
    assert!(r.block_reads_since(before).is_empty());
    assert_eq!(r.read_range(5, 4), Err(SegmentError::RangeInvalid));
}

impl SegmentReader<Counting<'_>> {
    fn src_calls(&self) -> usize {
        self.test_source().calls.get()
    }
    fn block_reads_since(&self, calls: usize) -> Vec<(u64, u64)> {
        self.test_source().reads.borrow()[calls..].to_vec()
    }
}
