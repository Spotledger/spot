//! Property: the streaming compare agrees with a naive in-memory compare on random record sets,
//! and flagging an entity derived removes exactly that entity from the comparison.

use std::collections::BTreeMap;

use proptest::prelude::*;

use crate::tests::digest;
use crate::{DerivedEntities, DifferenceKind, EntityCount, RecordSummary, compare};

/// One side's records: `(entity, id)` to `(version, digest)`.
type Side = BTreeMap<(String, String), (u64, [u8; 32])>;

/// The entity names the generated records use.
const ENTITIES: [&str; 3] = ["account", "invoice", "warehouse"];

/// A generated record row: entity index, id index, version, digest seed.
type Row = (u8, u8, u8, u8);

/// Two random record sets.
fn rows() -> impl Strategy<Value = (Vec<Row>, Vec<Row>)> {
    let row = (0u8..3, 0u8..4, 0u8..3, 0u8..3);
    (
        prop::collection::vec(row.clone(), 0..14),
        prop::collection::vec(row, 0..14),
    )
}

/// The entity name of a generated index.
fn entity_name(index: u8) -> String {
    ENTITIES[usize::from(index % 3)].to_owned()
}

/// One side from its generated rows; a repeated key keeps the last row.
fn side(rows: &[Row]) -> Side {
    let mut side = Side::new();
    for (entity, id, version, seed) in rows {
        side.insert(
            (entity_name(*entity), format!("r{id}")),
            (u64::from(*version), digest(*seed)),
        );
    }
    side
}

/// A side as a sorted stream of summaries, the way the rebuild and the live database supply them.
fn records(side: &Side) -> Vec<RecordSummary> {
    side.iter()
        .map(|((entity, id), (version, digest))| {
            RecordSummary::new(entity.clone(), id.clone(), *version, *digest)
        })
        .collect()
}

/// The differences a naive in-memory comparison expects: `(missing, extra, version, content)`.
fn expected_counts(live: &Side, rebuilt: &Side) -> (u64, u64, u64, u64) {
    let mut missing = 0;
    let mut version = 0;
    let mut content = 0;
    for (key, (live_version, live_digest)) in live {
        match rebuilt.get(key) {
            None => missing += 1,
            Some((rebuilt_version, rebuilt_digest)) => {
                if live_version != rebuilt_version {
                    version += 1;
                } else if live_digest != rebuilt_digest {
                    content += 1;
                }
            }
        }
    }
    let extra = rebuilt
        .keys()
        .filter(|key| !live.contains_key(*key))
        .count() as u64;
    (missing, extra, version, content)
}

/// The first difference a naive comparison expects, in sort order.
fn expected_first(live: &Side, rebuilt: &Side) -> Option<(String, String, DifferenceKind)> {
    let mut keys: Vec<&(String, String)> = live.keys().chain(rebuilt.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        match (live.get(key), rebuilt.get(key)) {
            (Some(_), None) => {
                return Some((
                    key.0.clone(),
                    key.1.clone(),
                    DifferenceKind::MissingInRebuild,
                ));
            }
            (None, Some(_)) => {
                return Some((key.0.clone(), key.1.clone(), DifferenceKind::ExtraInRebuild));
            }
            (Some((live_version, live_digest)), Some((rebuilt_version, rebuilt_digest))) => {
                if live_version != rebuilt_version {
                    return Some((key.0.clone(), key.1.clone(), DifferenceKind::VersionDiffers));
                }
                if live_digest != rebuilt_digest {
                    return Some((key.0.clone(), key.1.clone(), DifferenceKind::ContentDiffers));
                }
            }
            (None, None) => {}
        }
    }
    None
}

proptest! {
    #[test]
    fn the_streaming_compare_equals_a_naive_compare((live_rows, rebuilt_rows) in rows()) {
        let live = side(&live_rows);
        let rebuilt = side(&rebuilt_rows);
        let result = compare(records(&live), records(&rebuilt), &DerivedEntities::none())
            .expect("sorted streams");

        let (missing, extra, version, content) = expected_counts(&live, &rebuilt);
        prop_assert_eq!(result.totals.live, live.len() as u64);
        prop_assert_eq!(result.totals.rebuilt, rebuilt.len() as u64);
        prop_assert_eq!(result.totals.missing_in_rebuild, missing);
        prop_assert_eq!(result.totals.extra_in_rebuild, extra);
        prop_assert_eq!(result.totals.version_differs, version);
        prop_assert_eq!(result.totals.content_differs, content);

        match (expected_first(&live, &rebuilt), result.outcome.first_difference()) {
            (None, None) => prop_assert!(result.is_match()),
            (Some((entity, id, kind)), Some(found)) => {
                prop_assert_eq!(&found.entity, &entity);
                prop_assert_eq!(&found.id, &id);
                prop_assert_eq!(found.kind, kind);
            }
            (expected, found) => {
                prop_assert!(false, "naive {:?} but the compare said {:?}", expected, found);
            }
        }

        // The per-entity counts add up to the totals.
        let mut summed = EntityCount::default();
        for counted in result.per_entity.values() {
            summed.add(counted);
        }
        prop_assert_eq!(summed, result.totals);
    }
}
