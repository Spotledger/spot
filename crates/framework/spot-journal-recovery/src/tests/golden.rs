//! The stable JSON form and the no-tenant-data rule.

use spot_journal_audit::{BlobCrossCheck, KeyBackupStatus};

use super::{Fixture, NOW, defaults, tampering};
use crate::{ChainFacts, ChainState, assess};

/// A report with a few blockers of each kind, so the golden shows every shape.
fn fixture() -> Fixture {
    let mut fx = Fixture::healthy();
    fx.key_backup = KeyBackupStatus::Confirmed {
        verified_at_millis: NOW - 100 * crate::DAY_MILLIS,
    };
    fx.chain = ChainFacts::new(
        "acme",
        ChainState::Complete,
        [tampering("journal_verify.hash_mismatch", 10, 20)],
    );
    fx.blobs = Some(BlobCrossCheck::new(12, ["blobhash-9f3a", "blobhash-77c1"]));
    fx
}

#[test]
fn the_json_form_is_pinned() {
    let actual = serde_json::to_string_pretty(&assess(&fixture().inputs(), NOW, &defaults()))
        .expect("the assessment serialises");
    let golden = include_str!("golden/assessment.json");
    assert_eq!(actual.trim(), golden.trim());
}

#[test]
fn the_json_form_round_trips() {
    let assessment = assess(&fixture().inputs(), NOW, &defaults());
    let text = serde_json::to_string(&assessment).expect("serialises");
    let back: crate::Assessment = serde_json::from_str(&text).expect("deserialises");
    assert_eq!(back, assessment);
}

#[test]
fn no_tenant_data_reaches_the_report() {
    let fx = fixture();
    let healthy = serde_json::to_string(&assess(&Fixture::healthy().inputs(), NOW, &defaults()))
        .expect("serialises");
    let blocked =
        serde_json::to_string(&assess(&fx.inputs(), NOW, &defaults())).expect("serialises");
    let golden = include_str!("golden/assessment.json");
    for value in &fx.tenant_data {
        for text in [healthy.as_str(), blocked.as_str(), golden] {
            assert!(!text.contains(value.as_str()), "{value} leaked");
        }
    }
    // The blob hashes are not carried either, only their count.
    assert!(!blocked.contains("blobhash-"));
}
