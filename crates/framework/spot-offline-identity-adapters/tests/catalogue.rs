//! Every code this crate raises has a catalogue entry, and every catalogue entry is raised.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use spot_offline_identity_adapters::AdapterError;

fn codes() -> BTreeSet<&'static str> {
    let errors = [
        AdapterError::QuarantineRead,
        AdapterError::QuarantineWrite,
        AdapterError::QuarantineCorrupt,
        AdapterError::QuarantineConflict {
            node: "branch-1".to_owned(),
            journal_seq: 1,
        },
        AdapterError::ItemId,
        AdapterError::StateRead,
        AdapterError::StateWrite,
        AdapterError::StateTampered,
        AdapterError::StateRollback { stored: 1, seen: 2 },
        AdapterError::StateMalformed,
    ];
    let mut all = BTreeSet::new();
    for e in &errors {
        assert!(e.code().starts_with("offline_identity_adapters."), "{e}");
        assert!(all.insert(e.code()), "duplicate {}", e.code());
        assert!(e.to_string().contains(e.code()), "{e}");
    }
    all
}

#[test]
fn every_code_has_a_catalogue_entry_and_every_entry_is_raised() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(
        catalogue.load_error(),
        None,
        "the catalogue must load clean"
    );
    let raised = codes();
    for code in &raised {
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry for `{code}`"
        );
    }
    let file = include_str!("../../spot-errors/catalogue/offline_identity_adapters.toml");
    let listed: BTreeSet<&str> = file
        .lines()
        .filter_map(|l| l.strip_prefix("code = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();
    assert_eq!(
        listed, raised,
        "the catalogue file and the raised codes differ"
    );
}

#[test]
fn no_message_carries_a_key_a_blob_or_a_payload() {
    let e = AdapterError::QuarantineConflict {
        node: "branch-1".to_owned(),
        journal_seq: 7,
    };
    let text = e.to_string();
    assert!(text.contains("branch-1"));
    assert!(text.contains('7'));
    assert!(!text.contains("key="));
    assert!(!text.contains("payload="));
}
