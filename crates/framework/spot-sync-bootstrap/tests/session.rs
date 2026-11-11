#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Sessions: a fresh node, resume, trust, tenancy, scope, limits.

mod common;

use common::*;
use serde_json::Value;
use spot_journal_snapshot::{ImageRecord, SnapshotInfo};
use spot_sync_bootstrap::{
    Bootstrap, BootstrapError, Budget, Chunk, Cursor, MemorySource, RecordKey, Seams,
    SnapshotSource, StartOptions,
};
use spot_sync_closure::Limits;
use std::collections::{BTreeMap, BTreeSet};

fn limits(max_records: usize) -> Limits {
    Limits {
        max_records,
        max_bytes: 1 << 20,
        hard_limit_records: 1_000,
    }
}

fn drain(session: &mut spot_sync_bootstrap::Session<'_>, budget: Budget) -> Vec<Chunk> {
    let mut out = Vec::new();
    while let Some(c) = session.next_chunk(budget).unwrap() {
        out.push(c);
    }
    out
}

/// 10 customers, 10 items, 45 invoices (each linking one customer and one item), and 7 records of
/// an entity outside the scope.
fn world() -> Vec<ImageRecord> {
    let mut v = Vec::new();
    for i in 0..10 {
        v.push(rec("Customer", &format!("c{i:02}"), 1, &[]));
        v.push(rec("Item", &format!("i{i:02}"), 1, &[]));
    }
    for i in 0..45 {
        let c = format!("Customer/c{:02}", i % 10);
        let it = format!("Item/i{:02}", (i * 3) % 10);
        v.push(rec("Invoice", &format!("inv{i:02}"), 2, &[&c, &it]));
    }
    for i in 0..7 {
        v.push(rec("PayrollRun", &format!("p{i}"), 1, &[]));
    }
    v
}

#[test]
fn a_fresh_node_gets_every_in_scope_record_once_targets_first() {
    let src = source(world());
    let counting = Counting::new(&src);
    let mut s = Bootstrap::start(
        &counting,
        seams(&SCOPE),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(s.records_planned(), 65);
    assert_eq!(s.records_out_of_scope(), 7);
    assert_eq!(s.resume_from(), POSITION + 1);
    let chunks = drain(&mut s, Budget::of(&limits(10)));
    assert_eq!(chunks.len(), 7);
    let mut seen: BTreeSet<RecordKey> = BTreeSet::new();
    for c in &chunks {
        assert!(c.bytes <= 1 << 20);
        for b in &c.bundles {
            for r in &b.records {
                // Every link points to an already delivered record.
                for l in Links_of(&r.image) {
                    assert!(seen.contains(&l), "{:?} before its target {l:?}", r.key);
                }
            }
            for r in &b.records {
                assert!(seen.insert(r.key.clone()), "{:?} sent twice", r.key);
            }
        }
    }
    assert_eq!(seen.len(), 65);
    // Only the last chunk carries the handover position.
    for c in &chunks[..6] {
        assert_eq!(c.resume_from, None);
        assert!(!c.cursor.to_json().unwrap().is_empty());
    }
    assert_eq!(chunks[6].resume_from, Some(POSITION + 1));
    assert!(s.next_chunk(Budget::of(&limits(10))).unwrap().is_none());
    // One planning pass, then keyed fetches no larger than a chunk.
    assert_eq!(counting.scans.get(), 1);
    assert!(counting.max_fetch.get() <= 10);
    assert_eq!(counting.fetched.get(), 65);
}

#[allow(non_snake_case)]
fn Links_of(image: &Value) -> Vec<RecordKey> {
    image["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let (e, i) = s.as_str().unwrap().split_once('/').unwrap();
            RecordKey::new(e, i)
        })
        .collect()
}

#[test]
fn resume_after_chunk_three_of_seven_repeats_nothing() {
    let src = source(world());
    let l = limits(10);
    let node = node("n1");
    let mut first = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node,
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let all = drain(&mut first, Budget::of(&l));
    assert_eq!(all.len(), 7);

    // The node applied three chunks and persisted the cursor as text; the process restarted.
    let saved = all[2].cursor.to_json().unwrap();
    let mut again = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node,
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    again.resume(&Cursor::from_json(&saved).unwrap()).unwrap();
    let rest = drain(&mut again, Budget::of(&l));
    assert_eq!(rest.len(), 4);
    assert_eq!(rest, all[3..].to_vec());
    let before: BTreeSet<_> = all[..3]
        .iter()
        .flat_map(|c| {
            c.bundles
                .iter()
                .flat_map(|b| b.records.iter().map(|r| r.key.clone()))
        })
        .collect();
    for c in &rest {
        for b in &c.bundles {
            for r in &b.records {
                assert!(!before.contains(&r.key));
            }
        }
    }
}

#[test]
fn a_cursor_after_the_final_chunk_yields_nothing_more() {
    let src = source(world());
    let l = limits(10);
    let node = node("n1");
    let mut a = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node,
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let all = drain(&mut a, Budget::of(&l));
    let mut b = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node,
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    b.resume(&all[6].cursor).unwrap();
    assert!(b.next_chunk(Budget::of(&l)).unwrap().is_none());
    assert_eq!(b.resume_from(), POSITION + 1);
}

#[test]
fn a_cursor_is_bound_to_snapshot_node_scope_and_plan() {
    let src = source(world());
    let l = limits(10);
    let mut a = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let cursor = drain(&mut a, Budget::of(&l))[1].cursor.clone();

    // Another node.
    let mut other = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node("n2"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(
        other.resume(&cursor).unwrap_err().code(),
        "sync_bootstrap.cursor_mismatch"
    );

    // Another scope.
    let narrower = Entities(&["Customer", "Item"]);
    let mut other = Bootstrap::start(
        &src,
        seams(&narrower),
        l,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(
        other.resume(&cursor).unwrap_err().code(),
        "sync_bootstrap.cursor_mismatch"
    );

    // Same node and scope but the snapshot has one more record: the plan differs.
    let mut more = world();
    more.push(rec("Customer", "c99", 1, &[]));
    let src2 = source(more);
    let mut other = Bootstrap::start(
        &src2,
        seams(&SCOPE),
        l,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(
        other.resume(&cursor).unwrap_err().code(),
        "sync_bootstrap.cursor_mismatch"
    );

    // Text that is not a cursor.
    assert!(Cursor::from_json("{\"anything\":1}").is_err());
}

#[test]
fn the_cursor_holds_no_record_content() {
    let src = source(world());
    let l = limits(10);
    let mut a = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node("node-identity-xyz"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let text = drain(&mut a, Budget::of(&l))[0].cursor.to_json().unwrap();
    for needle in [
        "Customer",
        "Invoice",
        "c00",
        "secret_note",
        "node-identity-xyz",
        TENANT,
    ] {
        assert!(!text.contains(needle), "{needle} in {text}");
    }
}

#[test]
fn out_of_scope_records_never_appear_in_any_chunk() {
    let src = source(world());
    let mut s = Bootstrap::start(
        &src,
        seams(&SCOPE),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let chunks = drain(&mut s, Budget::of(&limits(10)));
    let text = serde_json::to_string(&chunks).unwrap();
    assert!(!text.contains("PayrollRun"));
    assert!(!text.contains("\"p3\""));
}

#[test]
fn a_link_to_a_record_outside_the_scope_is_not_followed() {
    let mut w = world();
    w.push(rec(
        "Invoice",
        "inv99",
        1,
        &["PayrollRun/p1", "Customer/c00"],
    ));
    let src = source(w);
    let mut s = Bootstrap::start(
        &src,
        seams(&SCOPE),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(s.dangling_links(), 1);
    let chunks = drain(&mut s, Budget::of(&limits(10)));
    let keys: Vec<_> = chunks
        .iter()
        .flat_map(|c| {
            c.bundles
                .iter()
                .flat_map(|b| b.records.iter().map(|r| r.key.entity.clone()))
        })
        .collect();
    assert!(!keys.iter().any(|e| e == "PayrollRun"));
}

#[test]
fn a_cycle_travels_whole_in_one_bundle() {
    let mut w = world();
    w.push(rec("Invoice", "ca", 1, &["Invoice/cb"]));
    w.push(rec("Invoice", "cb", 1, &["Invoice/cc"]));
    w.push(rec("Invoice", "cc", 1, &["Invoice/ca", "Customer/c01"]));
    let src = source(w);
    // Tiny chunks: one record per chunk would split the cycle if the session did not hold it whole.
    let l = limits(1);
    let mut s = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let chunks = drain(&mut s, Budget::of(&l));
    let cycle: Vec<_> = chunks
        .iter()
        .flat_map(|c| c.bundles.iter())
        .filter(|b| b.records.iter().any(|r| r.key.id == "ca"))
        .collect();
    assert_eq!(cycle.len(), 1);
    let ids: BTreeSet<_> = cycle[0].records.iter().map(|r| r.key.id.as_str()).collect();
    assert_eq!(ids, BTreeSet::from(["ca", "cb", "cc"]));
}

#[test]
fn a_cycle_over_the_byte_limit_is_refused_at_start() {
    let w = vec![
        rec("Invoice", "ca", 1, &["Invoice/cb"]),
        rec("Invoice", "cb", 1, &["Invoice/ca"]),
    ];
    let src = source(w);
    let l = Limits {
        max_records: 10,
        max_bytes: 50,
        hard_limit_records: 100,
    };
    let err = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .err()
    .unwrap();
    assert_eq!(err.code(), "sync_bootstrap.component_too_large");
}

#[test]
fn a_chunk_never_exceeds_the_limits_or_the_budget() {
    let src = source(world());
    let l = Limits {
        max_records: 500,
        max_bytes: 2_000,
        hard_limit_records: 1_000,
    };
    let mut s = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    // A budget above the limits is clamped to the limits.
    let chunks = drain(
        &mut s,
        Budget {
            max_bytes: usize::MAX,
            max_records: usize::MAX,
        },
    );
    assert!(chunks.len() > 1);
    for c in &chunks {
        assert!(c.bytes <= 2_000, "{}", c.bytes);
        assert_eq!(c.bytes, c.bundles.iter().map(|b| b.bytes).sum::<usize>());
    }
    // A smaller budget yields more chunks, and the content is the same.
    let mut t = Bootstrap::start(
        &src,
        seams(&SCOPE),
        l,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let small = drain(
        &mut t,
        Budget {
            max_bytes: 400,
            max_records: 3,
        },
    );
    assert!(small.len() > chunks.len());
    let flat = |cs: &[Chunk]| -> Vec<RecordKey> {
        cs.iter()
            .flat_map(|c| {
                c.bundles
                    .iter()
                    .flat_map(|b| b.records.iter().map(|r| r.key.clone()))
            })
            .collect()
    };
    assert_eq!(flat(&small), flat(&chunks));
}

#[test]
fn an_empty_scope_still_hands_over() {
    let src = source(world());
    let none = Entities(&[]);
    let mut s = Bootstrap::start(
        &src,
        seams(&none),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let chunks = drain(&mut s, Budget::of(&limits(10)));
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].bundles.is_empty());
    assert_eq!(chunks[0].resume_from, Some(POSITION + 1));
}

#[test]
fn excluded_fields_are_removed_when_serving() {
    let src = source(world());
    let seams = Seams {
        scope: &SCOPE,
        references: &Links,
        redactor: &DropSecret,
    };
    let mut s = Bootstrap::start(
        &src,
        seams,
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let chunks = drain(&mut s, Budget::of(&limits(10)));
    assert!(
        !serde_json::to_string(&chunks)
            .unwrap()
            .contains("secret_note")
    );
}

#[test]
fn trust_tenant_generation_and_head_are_checked() {
    let l = limits(10);
    let start = |src: &dyn SnapshotSource, n: &spot_sync_bootstrap::NodeState, opts| {
        Bootstrap::start(src, seams(&SCOPE), l, n, hub(), opts)
            .err()
            .map(|e| e.code())
    };
    let unverified = MemorySource::new(TENANT, info(false), world());
    assert_eq!(
        start(&unverified, &node("n"), StartOptions::default()),
        Some("sync_bootstrap.untrusted_snapshot")
    );
    assert_eq!(
        start(
            &unverified,
            &node("n"),
            StartOptions {
                allow_untrusted: true
            }
        ),
        None
    );

    let mut foreign = node("n");
    foreign.tenant = "t2".into();
    assert_eq!(
        start(&source(world()), &foreign, StartOptions::default()),
        Some("sync_bootstrap.tenant_mismatch")
    );

    let wrong_gen = SnapshotInfo {
        generation: GENERATION + 1,
        ..info(true)
    };
    assert_eq!(
        start(
            &MemorySource::new(TENANT, wrong_gen, world()),
            &node("n"),
            StartOptions::default()
        ),
        Some("sync_bootstrap.snapshot_mismatch")
    );
    let beyond = SnapshotInfo {
        position: hub().head_position + 1,
        ..info(true)
    };
    assert_eq!(
        start(
            &MemorySource::new(TENANT, beyond, world()),
            &node("n"),
            StartOptions::default()
        ),
        Some("sync_bootstrap.snapshot_mismatch")
    );
    let top = SnapshotInfo {
        position: u64::MAX,
        ..info(true)
    };
    let big_hub = spot_sync_bootstrap::HubHead {
        generation: GENERATION,
        head_position: u64::MAX,
    };
    let err = Bootstrap::start(
        &MemorySource::new(TENANT, top, world()),
        seams(&SCOPE),
        l,
        &node("n"),
        big_hub,
        StartOptions::default(),
    )
    .err()
    .unwrap();
    assert_eq!(err, BootstrapError::PositionOverflow);

    let bad = Limits {
        max_records: 0,
        ..l
    };
    let err = Bootstrap::start(
        &source(world()),
        seams(&SCOPE),
        bad,
        &node("n"),
        hub(),
        StartOptions::default(),
    )
    .err()
    .unwrap();
    assert_eq!(err.code(), "sync_bootstrap.invalid_settings");
}

/// A source that returns a record nobody asked for.
struct Leaky(MemorySource);

impl SnapshotSource for Leaky {
    fn tenant(&self) -> &str {
        self.0.tenant()
    }
    fn info(&self) -> SnapshotInfo {
        self.0.info()
    }
    fn scan(
        &self,
        visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
    ) -> Result<(), BootstrapError> {
        self.0.scan(visit)
    }
    fn fetch(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError> {
        let mut v = self.0.fetch(keys)?;
        v.extend(self.0.fetch(&[RecordKey::new("PayrollRun", "p1")])?);
        Ok(v)
    }
}

#[test]
fn a_source_that_returns_more_than_asked_is_refused() {
    let src = Leaky(source(world()));
    let mut s = Bootstrap::start(
        &src,
        seams(&SCOPE),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let err = s.next_chunk(Budget::of(&limits(10))).unwrap_err();
    assert_eq!(err.code(), "sync_bootstrap.scope_violation");
}

/// A source whose scan is out of order.
#[test]
fn an_unsorted_scan_is_refused() {
    struct Backwards(MemorySource);
    impl SnapshotSource for Backwards {
        fn tenant(&self) -> &str {
            self.0.tenant()
        }
        fn info(&self) -> SnapshotInfo {
            self.0.info()
        }
        fn scan(
            &self,
            visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
        ) -> Result<(), BootstrapError> {
            let mut all = Vec::new();
            self.0.scan(&mut |r| {
                all.push(r);
                Ok(())
            })?;
            for r in all.into_iter().rev() {
                visit(r)?;
            }
            Ok(())
        }
        fn fetch(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError> {
            self.0.fetch(keys)
        }
    }
    let err = Bootstrap::start(
        &Backwards(source(world())),
        seams(&SCOPE),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .err()
    .unwrap();
    assert_eq!(err.code(), "sync_bootstrap.source_failed");
}

#[test]
fn a_record_that_changed_after_planning_is_refused() {
    struct Shrinks(MemorySource);
    impl SnapshotSource for Shrinks {
        fn tenant(&self) -> &str {
            self.0.tenant()
        }
        fn info(&self) -> SnapshotInfo {
            self.0.info()
        }
        fn scan(
            &self,
            visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
        ) -> Result<(), BootstrapError> {
            self.0.scan(visit)
        }
        fn fetch(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError> {
            let mut v = self.0.fetch(keys)?;
            for r in &mut v {
                r.image = serde_json::json!({"links": []});
            }
            Ok(v)
        }
    }
    let shrinks = Shrinks(source(world()));
    let mut s = Bootstrap::start(
        &shrinks,
        seams(&SCOPE),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(
        s.next_chunk(Budget::of(&limits(10))).unwrap_err().code(),
        "sync_bootstrap.snapshot_changed"
    );
}

#[test]
fn versions_travel_with_the_records() {
    let src = source(world());
    let mut s = Bootstrap::start(
        &src,
        seams(&SCOPE),
        limits(10),
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    let versions: BTreeMap<String, u64> = drain(&mut s, Budget::of(&limits(10)))
        .iter()
        .flat_map(|c| {
            c.bundles
                .iter()
                .flat_map(|b| b.records.iter().map(|r| (r.key.entity.clone(), r.version)))
        })
        .collect();
    assert_eq!(versions["Customer"], 1);
    assert_eq!(versions["Invoice"], 2);
}
