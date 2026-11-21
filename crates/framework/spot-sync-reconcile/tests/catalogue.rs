//! Every code this crate raises is catalogued (the completeness rule of
//! `crates/framework/spot-errors/README.md`).
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_sync_reconcile::ReconcileError;

fn all_errors() -> Vec<ReconcileError> {
    vec![
        ReconcileError::MirrorAsSource {
            entity: "item".into(),
            name: "A".into(),
        },
        ReconcileError::InvalidScope { reason: "x".into() },
        ReconcileError::InvalidSchedule { reason: "x".into() },
        ReconcileError::UnknownEntity {
            entity: "item".into(),
        },
        ReconcileError::Source {
            message: "x".into(),
        },
    ]
}

#[test]
fn every_code_has_a_catalogue_entry() {
    let catalogue = spot_errors::Catalogue::load();
    for error in all_errors() {
        let code = error.code();
        assert!(
            catalogue.lookup(code).is_some(),
            "code {code} has no catalogue entry"
        );
    }
}
