use crate::{SealLimits, SealState, should_seal};

fn st(entries: u64, bytes: u64, opened: i64) -> SealState {
    SealState {
        entries,
        bytes,
        opened_at_millis: opened,
    }
}

#[test]
fn defaults_are_64_mib_and_five_minutes() {
    let l = SealLimits::default();
    assert_eq!(l.max_bytes, 64 * 1024 * 1024);
    assert_eq!(l.max_age_millis, 300_000);
}

#[test]
fn truth_table() {
    let l = SealLimits::default();
    let t0 = 1_000_000;
    // (state, now, expected)
    let rows = [
        (st(0, 0, t0), t0, false),                        // empty, fresh
        (st(0, 0, t0), t0 + 10_000_000, false),           // empty, very old: never seals
        (st(0, l.max_bytes, t0), t0 + 10_000_000, false), // empty with bytes (inconsistent): still no
        (st(1, 10, t0), t0, false),                       // one entry, fresh
        (st(1, 10, t0), t0 + 299_999, false),             // just under the age
        (st(1, 10, t0), t0 + 300_000, true),              // exactly the age
        (st(1, 10, t0), t0 + 400_000, true),              // older
        (st(5, l.max_bytes - 1, t0), t0, false),          // just under the size
        (st(5, l.max_bytes, t0), t0, true),               // exactly the size
        (st(5, l.max_bytes + 1, t0), t0, true),           // over the size
        (st(1, 10, t0), t0 - 1_000_000, false),           // clock went backwards
        (st(1, 10, i64::MIN), i64::MAX, true),            // extreme values do not overflow
    ];
    for (i, (s, now, want)) in rows.iter().enumerate() {
        assert_eq!(should_seal(s, *now, &l), *want, "row {i}");
    }
}
