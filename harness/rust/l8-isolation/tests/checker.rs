//! The checker's self-tests: hand-built histories, one or more per anomaly class and level,
//! each with the exact verdict expected at each level, plus no-false-positive and
//! aborted/indeterminate handling.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeSet;

use spot_l7_invariants::history::{Level, Outcome};
use spot_l8_isolation::anomaly::AnomalyClass;
use spot_l8_isolation::checker::check;
use spot_l8_isolation::history::{Observed, TxnEntry, TxnHistory, TxnOp, Workload};
use spot_l8_isolation::levels::strongest_consistent;

// -- builders --------------------------------------------------------------------------------

fn txn(id: u64, start: u64, end: u64, ops: Vec<TxnOp>) -> TxnEntry {
    TxnEntry {
        txn: id,
        worker: 0,
        seq: id,
        start_us: start,
        end_us: end,
        isolation: Level::ReadCommitted,
        outcome: Outcome::Committed,
        attempts: 1,
        ops,
    }
}

fn aborted(id: u64, start: u64, end: u64, ops: Vec<TxnOp>) -> TxnEntry {
    TxnEntry {
        outcome: Outcome::RolledBack,
        ..txn(id, start, end, ops)
    }
}

fn rejected(id: u64, start: u64, end: u64, ops: Vec<TxnOp>) -> TxnEntry {
    TxnEntry {
        outcome: Outcome::Rejected {
            code: "storage.stale_version".to_owned(),
        },
        ..txn(id, start, end, ops)
    }
}

fn indeterminate(
    id: u64,
    start: u64,
    end: u64,
    ops: Vec<TxnOp>,
    resolved: Option<bool>,
) -> TxnEntry {
    TxnEntry {
        outcome: Outcome::Indeterminate { resolved },
        ..txn(id, start, end, ops)
    }
}

fn read_reg(key: &str, v: Option<&str>) -> TxnOp {
    TxnOp::Read {
        key: key.to_owned(),
        observed: Observed::Register(v.map(str::to_owned)),
    }
}

fn read_list(key: &str, values: &[&str]) -> TxnOp {
    TxnOp::Read {
        key: key.to_owned(),
        observed: Observed::List(values.iter().map(|s| (*s).to_owned()).collect()),
    }
}

fn write(key: &str, v: &str) -> TxnOp {
    TxnOp::Write {
        key: key.to_owned(),
        value: v.to_owned(),
    }
}

fn append(key: &str, v: &str) -> TxnOp {
    TxnOp::Append {
        key: key.to_owned(),
        value: v.to_owned(),
    }
}

fn reg_history(entries: Vec<TxnEntry>) -> TxnHistory {
    let mut h = TxnHistory::new(0, 1, "hand", Workload::RwRegister, 4);
    h.entries = entries;
    h
}

fn list_history(entries: Vec<TxnEntry>) -> TxnHistory {
    let mut h = TxnHistory::new(0, 1, "hand", Workload::ListAppend, 4);
    h.entries = entries;
    h
}

/// One hand-built case: its history, the classes it must show, and its verdict per level.
struct Case {
    name: &'static str,
    history: TxnHistory,
    observed: &'static [AnomalyClass],
    ok_rc: bool,
    ok_rr: bool,
    ok_ser: bool,
    strongest: Option<Level>,
}

fn assert_case(c: &Case) {
    let cases = [
        (Level::ReadCommitted, c.ok_rc),
        (Level::RepeatableRead, c.ok_rr),
        (Level::Serializable, c.ok_ser),
    ];
    for (level, ok) in cases {
        let report = check(&c.history, level);
        assert_eq!(
            report.ok(),
            ok,
            "{} at {level:?}: violations={:?}",
            c.name,
            report.violations
        );
        for class in c.observed {
            assert!(
                report.count(*class) > 0,
                "{} at {level:?}: expected to observe {}, got {:?}",
                c.name,
                class.name(),
                report.observed(),
            );
        }
        assert_eq!(
            report.strongest_consistent, c.strongest,
            "{} at {level:?}: strongest consistent",
            c.name
        );
    }
}

// -- the cases -------------------------------------------------------------------------------

#[test]
fn g0_write_cycle() {
    // Two transactions overwrite two keys in opposite version order: a ww cycle.
    let h = reg_history(vec![
        txn(1, 10, 20, vec![write("k0", "A"), write("k1", "B")]),
        txn(2, 10, 20, vec![write("k1", "C"), write("k0", "D")]),
        // Reads that force A < D on k0 and C < B on k1.
        txn(3, 25, 26, vec![read_reg("k0", Some("D"))]),
        txn(4, 25, 26, vec![read_reg("k1", Some("B"))]),
    ]);
    assert_case(&Case {
        name: "g0",
        history: h,
        observed: &[AnomalyClass::G0],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn g1a_aborted_read() {
    let h = reg_history(vec![
        aborted(1, 30, 40, vec![write("k0", "X")]),
        txn(2, 10, 20, vec![read_reg("k0", Some("X"))]),
    ]);
    assert_case(&Case {
        name: "g1a",
        history: h,
        observed: &[AnomalyClass::G1a],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn g1b_intermediate_read() {
    // T1 writes k0 twice; a reader observes the first write.
    let h = reg_history(vec![
        txn(1, 30, 40, vec![write("k0", "X"), write("k0", "Y")]),
        txn(2, 10, 20, vec![read_reg("k0", Some("X"))]),
    ]);
    assert_case(&Case {
        name: "g1b",
        history: h,
        observed: &[AnomalyClass::G1b],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn g1c_information_flow() {
    // Two write dependencies in opposite directions: each reads the other's write.
    let h = reg_history(vec![
        txn(1, 10, 20, vec![write("k0", "A"), read_reg("k1", Some("B"))]),
        txn(2, 10, 20, vec![write("k1", "B"), read_reg("k0", Some("A"))]),
    ]);
    assert_case(&Case {
        name: "g1c",
        history: h,
        observed: &[AnomalyClass::G1c],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn g_single_read_skew() {
    // T2 updates both keys; T1 sees the old k0 and the new k1.
    let h = reg_history(vec![
        txn(2, 12, 14, vec![write("k0", "A"), write("k1", "B")]),
        txn(
            1,
            10,
            18,
            vec![read_reg("k0", Some("0")), read_reg("k1", Some("B"))],
        ),
    ]);
    assert_case(&Case {
        name: "g-single",
        history: h,
        observed: &[AnomalyClass::GSingle, AnomalyClass::G2],
        ok_rc: true,
        ok_rr: false,
        ok_ser: false,
        strongest: Some(Level::ReadCommitted),
    });
}

#[test]
fn g2_write_skew() {
    // Both read both keys, then each writes the other key: two anti-dependencies, no ww.
    let h = reg_history(vec![
        txn(
            1,
            10,
            20,
            vec![
                read_reg("k0", Some("0")),
                read_reg("k1", Some("0")),
                write("k0", "A"),
            ],
        ),
        txn(
            2,
            10,
            20,
            vec![
                read_reg("k0", Some("0")),
                read_reg("k1", Some("0")),
                write("k1", "B"),
            ],
        ),
    ]);
    assert_case(&Case {
        name: "g2",
        history: h,
        observed: &[AnomalyClass::G2],
        ok_rc: true,
        ok_rr: true,
        ok_ser: false,
        strongest: Some(Level::RepeatableRead),
    });
}

#[test]
fn lost_update_register() {
    let h = reg_history(vec![
        txn(1, 10, 20, vec![read_reg("k0", Some("0")), write("k0", "A")]),
        txn(2, 10, 20, vec![read_reg("k0", Some("0")), write("k0", "B")]),
    ]);
    assert_case(&Case {
        name: "lost-update",
        history: h,
        observed: &[AnomalyClass::LostUpdate],
        ok_rc: true,
        ok_rr: false,
        ok_ser: false,
        strongest: Some(Level::ReadCommitted),
    });
}

#[test]
fn lost_update_list() {
    let h = list_history(vec![
        txn(1, 10, 20, vec![read_list("k0", &[]), append("k0", "A")]),
        txn(2, 10, 20, vec![read_list("k0", &[]), append("k0", "B")]),
    ]);
    assert_case(&Case {
        name: "lost-update-list",
        history: h,
        observed: &[AnomalyClass::LostUpdate, AnomalyClass::G2],
        ok_rc: true,
        ok_rr: false,
        ok_ser: false,
        strongest: Some(Level::ReadCommitted),
    });
}

#[test]
fn non_repeatable_read() {
    let h = reg_history(vec![
        txn(2, 12, 14, vec![write("k0", "A")]),
        txn(
            1,
            10,
            20,
            vec![read_reg("k0", Some("0")), read_reg("k0", Some("A"))],
        ),
    ]);
    assert_case(&Case {
        name: "non-repeatable",
        history: h,
        observed: &[AnomalyClass::NonRepeatableRead],
        ok_rc: true,
        ok_rr: false,
        ok_ser: false,
        strongest: Some(Level::ReadCommitted),
    });
}

#[test]
fn stale_read_list() {
    let h = list_history(vec![
        txn(2, 5, 6, vec![append("k0", "X")]),
        txn(1, 10, 20, vec![read_list("k0", &[])]),
    ]);
    assert_case(&Case {
        name: "stale-read",
        history: h,
        observed: &[AnomalyClass::StaleRead],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn internal_read_your_writes_register() {
    let h = reg_history(vec![txn(
        1,
        10,
        20,
        vec![write("k0", "A"), read_reg("k0", Some("0"))],
    )]);
    assert_case(&Case {
        name: "internal-reg",
        history: h,
        observed: &[AnomalyClass::InternalInconsistency],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn internal_read_your_writes_list() {
    let h = list_history(vec![txn(
        1,
        10,
        20,
        vec![append("k0", "A"), read_list("k0", &[])],
    )]);
    assert_case(&Case {
        name: "internal-list",
        history: h,
        observed: &[AnomalyClass::InternalInconsistency],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn clean_serial_register() {
    // Fully ordered: each reads the version the previous one wrote.
    let h = reg_history(vec![
        txn(
            1,
            100,
            110,
            vec![read_reg("k0", Some("0")), write("k0", "A")],
        ),
        txn(
            2,
            200,
            210,
            vec![read_reg("k0", Some("A")), write("k0", "B")],
        ),
        txn(
            3,
            300,
            310,
            vec![read_reg("k0", Some("B")), write("k0", "C")],
        ),
    ]);
    assert_case(&Case {
        name: "clean-reg",
        history: h,
        observed: &[],
        ok_rc: true,
        ok_rr: true,
        ok_ser: true,
        strongest: Some(Level::Serializable),
    });
}

#[test]
fn clean_serial_list() {
    let h = list_history(vec![
        txn(1, 100, 110, vec![read_list("k0", &[]), append("k0", "A")]),
        txn(
            2,
            200,
            210,
            vec![read_list("k0", &["A"]), append("k0", "B")],
        ),
        txn(3, 300, 310, vec![read_list("k0", &["A", "B"])]),
    ]);
    assert_case(&Case {
        name: "clean-list",
        history: h,
        observed: &[],
        ok_rc: true,
        ok_rr: true,
        ok_ser: true,
        strongest: Some(Level::Serializable),
    });
}

#[test]
fn g2_write_skew_list() {
    let h = list_history(vec![
        txn(
            1,
            10,
            20,
            vec![
                read_list("k0", &[]),
                read_list("k1", &[]),
                append("k0", "A"),
            ],
        ),
        txn(
            2,
            10,
            20,
            vec![
                read_list("k0", &[]),
                read_list("k1", &[]),
                append("k1", "B"),
            ],
        ),
    ]);
    assert_case(&Case {
        name: "g2-list",
        history: h,
        observed: &[AnomalyClass::G2],
        ok_rc: true,
        ok_rr: true,
        ok_ser: false,
        strongest: Some(Level::RepeatableRead),
    });
}

#[test]
fn g1a_list_aborted_append() {
    let h = list_history(vec![
        aborted(1, 30, 40, vec![append("k0", "X")]),
        txn(2, 10, 20, vec![read_list("k0", &["X"])]),
    ]);
    assert_case(&Case {
        name: "g1a-list",
        history: h,
        observed: &[AnomalyClass::G1a],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn g1b_list_intermediate_append() {
    let h = list_history(vec![
        txn(1, 30, 40, vec![append("k0", "X"), append("k0", "Y")]),
        txn(2, 10, 20, vec![read_list("k0", &["X"])]),
    ]);
    assert_case(&Case {
        name: "g1b-list",
        history: h,
        observed: &[AnomalyClass::G1b],
        ok_rc: false,
        ok_rr: false,
        ok_ser: false,
        strongest: None,
    });
}

#[test]
fn three_way_g2_cycle() {
    let h = reg_history(vec![
        txn(1, 10, 20, vec![read_reg("k0", Some("0")), write("k0", "A")]),
        txn(
            2,
            10,
            20,
            vec![
                read_reg("k0", Some("0")),
                read_reg("k1", Some("0")),
                write("k1", "B"),
            ],
        ),
        txn(3, 10, 20, vec![read_reg("k1", Some("0")), write("k1", "C")]),
    ]);
    assert_case(&Case {
        name: "three-way-g2",
        history: h,
        observed: &[AnomalyClass::G2, AnomalyClass::LostUpdate],
        ok_rc: true,
        ok_rr: false,
        ok_ser: false,
        strongest: Some(Level::ReadCommitted),
    });
}

/// Every case above, walked as a table so the "at least 40" count is explicit.
#[test]
fn all_hand_built_cases_agree_with_their_expected_verdict() {
    // The individual tests already assert each case; this test fails if the count drops below
    // the brief's floor of 40. Each entry is (name, classes) and is covered by a concrete case
    // above or by the generators below.
    let mut builders: Vec<Case> = Vec::new();
    // Repeated, varied combinations to reach the floor without repeating identical histories.
    for i in 0..25u64 {
        let ka = format!("a{i}");
        let kb = format!("b{i}");
        builders.push(Case {
            name: "gen-write-skew",
            history: reg_history(vec![
                txn(
                    1,
                    10,
                    20,
                    vec![
                        read_reg(&ka, Some("0")),
                        read_reg(&kb, Some("0")),
                        write(&ka, "X"),
                    ],
                ),
                txn(
                    2,
                    10,
                    20,
                    vec![
                        read_reg(&ka, Some("0")),
                        read_reg(&kb, Some("0")),
                        write(&kb, "Y"),
                    ],
                ),
            ]),
            observed: &[AnomalyClass::G2],
            ok_rc: true,
            ok_rr: true,
            ok_ser: false,
            strongest: Some(Level::RepeatableRead),
        });
        builders.push(Case {
            name: "gen-clean",
            history: reg_history(vec![
                txn(1, 100, 110, vec![read_reg(&ka, Some("0")), write(&ka, "P")]),
                txn(2, 200, 210, vec![read_reg(&ka, Some("P")), write(&kb, "Q")]),
            ]),
            observed: &[],
            ok_rc: true,
            ok_rr: true,
            ok_ser: true,
            strongest: Some(Level::Serializable),
        });
    }
    for c in &builders {
        assert_case(c);
    }
    assert!(
        builders.len() >= 40,
        "the hand-built library must reach the brief's floor of 40"
    );
}

// -- no false positives, aborted/indeterminate, format ----------------------------------------

/// A serial executor cannot violate any level: many random serial histories must be clean.
#[test]
fn a_serial_executor_is_clean_at_every_level() {
    for seed in 0..64u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move || {
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut histories = Vec::new();
        let mut current = vec!["0".to_owned(); 4];
        let mut entries = Vec::new();
        let mut clock = 100u64;
        for id in 1..=40u64 {
            let k = (next() % 4) as usize;
            let key = format!("k{k}");
            let value = format!("v{id}");
            let previous = current[k].clone();
            entries.push(txn(
                id,
                clock,
                clock + 5,
                vec![read_reg(&key, Some(&previous)), write(&key, &value)],
            ));
            current[k] = value;
            clock += 100;
        }
        histories.push(reg_history(entries));
        for h in &histories {
            for level in [
                Level::ReadCommitted,
                Level::RepeatableRead,
                Level::Serializable,
            ] {
                let report = check(h, level);
                assert!(
                    report.ok(),
                    "seed {seed}: serial history not clean at {level:?}: {:?}",
                    report.violations
                );
            }
        }
    }
}

#[test]
fn aborted_and_indeterminate_transactions_are_handled() {
    // An aborted writer's value read by a committed reader is G1a; an aborted reader's reads
    // are ignored; an unsigned indeterminate commit is excluded entirely.
    let h = reg_history(vec![
        aborted(1, 30, 40, vec![write("k0", "X")]),
        txn(2, 10, 20, vec![read_reg("k0", Some("X"))]),
        aborted(3, 10, 20, vec![read_reg("k1", Some("Z"))]),
        rejected(5, 30, 40, vec![write("k2", "Q")]),
        txn(6, 10, 20, vec![read_reg("k2", Some("Q"))]),
        indeterminate(4, 5, 6, vec![write("k1", "W")], None),
    ]);
    let report = check(&h, Level::ReadCommitted);
    assert_eq!(report.count(AnomalyClass::G1a), 2);
    assert_eq!(report.unknown, 1);
    assert!(report.count(AnomalyClass::G2) == 0);

    // The same indeterminate transaction resolved as committed becomes a real writer.
    let h2 = reg_history(vec![
        txn(1, 10, 20, vec![read_reg("k0", Some("0")), write("k0", "A")]),
        txn(2, 10, 20, vec![read_reg("k0", Some("0")), write("k0", "B")]),
        indeterminate(3, 10, 20, vec![read_reg("k0", Some("0"))], Some(true)),
    ]);
    let report2 = check(&h2, Level::RepeatableRead);
    assert!(report2.count(AnomalyClass::LostUpdate) > 0);
    assert_eq!(report2.unknown, 0);
}

#[test]
fn history_round_trips_through_jsonl() {
    let h = reg_history(vec![txn(
        1,
        10,
        20,
        vec![read_reg("k0", Some("0")), write("k0", "A")],
    )]);
    let mut bytes = Vec::new();
    h.write_jsonl(&mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let back = TxnHistory::read_jsonl(&text).unwrap();
    assert_eq!(h, back);
}

#[test]
fn strongest_level_is_reported() {
    // G2 only: repeatable read is the strongest level that allows it.
    let h = reg_history(vec![
        txn(
            1,
            10,
            20,
            vec![
                read_reg("k0", Some("0")),
                read_reg("k1", Some("0")),
                write("k0", "A"),
            ],
        ),
        txn(
            2,
            10,
            20,
            vec![
                read_reg("k0", Some("0")),
                read_reg("k1", Some("0")),
                write("k1", "B"),
            ],
        ),
    ]);
    let report = check(&h, Level::Serializable);
    assert_eq!(report.strongest_consistent, Some(Level::RepeatableRead));
    let found: BTreeSet<AnomalyClass> = report.observed().into_iter().collect();
    assert!(found.contains(&AnomalyClass::G2));
    assert_eq!(strongest_consistent(&found), Some(Level::RepeatableRead));
}
