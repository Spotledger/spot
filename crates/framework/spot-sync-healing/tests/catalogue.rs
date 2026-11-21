//! Every code this crate raises is catalogued (the completeness rule of
//! `crates/framework/spot-errors/README.md`). Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_sync_healing::HealingError;

#[test]
fn every_code_has_a_catalogue_entry() {
    let catalogue = spot_errors::Catalogue::load();
    let errors = [
        HealingError::InvalidSettings { reason: "x".into() },
        HealingError::DirectionAmbiguous {
            node: "n".into(),
            scope: "s".into(),
        },
        HealingError::MirrorAhead {
            node: "n".into(),
            scope: "s".into(),
        },
    ];
    for error in errors {
        assert!(
            catalogue.lookup(error.code()).is_some(),
            "code {} has no catalogue entry",
            error.code()
        );
    }
}
