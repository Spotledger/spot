//! Tests for holds.
mod common;

use common::{decodes, report};
use spot_sync_skew::{
    HoldReason, HoldSet, Incoming, RecordRef, on_undecodable, release_holds, triage,
};

fn rec(id: &str, version: u64) -> RecordRef {
    RecordRef {
        entity: "Invoice".into(),
        id: id.into(),
        version,
    }
}

fn incoming(id: &str, major: u32, meta: Option<u32>) -> Incoming {
    Incoming {
        record: rec(id, 1),
        topic: "invoice".into(),
        major,
        requires_metadata: meta,
    }
}

#[test]
fn hold_flags() {
    let h = on_undecodable(rec("a", 1), HoldReason::MetadataTooLow { required: 11 });
    assert!(h.rerequest_after_upgrade);
    assert!(!h.blocks_applied_prefix);
    assert!(!h.obligation_satisfied);
}

#[test]
fn one_undecodable_record_does_not_block_the_ones_after_it() {
    let node = decodes(report(5), "invoice", &[1]);
    let batch = [
        incoming("a", 1, None),
        incoming("b", 2, None),
        incoming("c", 1, Some(11)),
        incoming("d", 1, Some(10)),
    ];
    let t = triage(&batch, &node);
    let applied: Vec<_> = t.apply.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(applied, ["a", "d"]);
    let held: Vec<_> = t.held.iter().map(|h| h.record.id.as_str()).collect();
    assert_eq!(held, ["b", "c"]);
    assert_eq!(
        t.held[0].reason,
        HoldReason::UnsupportedMajor {
            topic: "invoice".into(),
            major: 2
        }
    );
}

#[test]
fn holds_release_after_upgrade() {
    let node = decodes(report(5), "invoice", &[1]);
    let t = triage(&[incoming("b", 2, None), incoming("c", 1, Some(11))], &node);
    let mut upgraded = decodes(report(6), "invoice", &[1, 2]);
    upgraded.metadata_version = 10;
    let r = release_holds(&t.held, &upgraded);
    assert_eq!(r.released.len(), 1);
    assert_eq!(r.released[0].record.id, "b");
    assert_eq!(r.still_held[0].record.id, "c");
    upgraded.metadata_version = 11;
    let r = release_holds(&t.held, &upgraded);
    assert_eq!((r.released.len(), r.still_held.len()), (2, 0));
}

#[test]
fn a_later_version_supersedes_an_undecodable_one() {
    let reason = HoldReason::MetadataTooLow { required: 11 };
    let mut set = HoldSet::new();
    set.insert(on_undecodable(rec("a", 2), reason.clone()));
    set.insert(on_undecodable(rec("a", 3), reason.clone()));
    assert_eq!((set.len(), set.holds()[0].record.version), (1, 3));
    // an older arrival does not replace the newer hold
    set.insert(on_undecodable(rec("a", 1), reason.clone()));
    assert_eq!(set.holds()[0].record.version, 3);
    // applying an older version leaves the newer hold; a newer or equal one clears it
    set.applied(&rec("a", 2));
    assert_eq!(set.len(), 1);
    set.applied(&rec("a", 3));
    assert!(set.is_empty());
}
