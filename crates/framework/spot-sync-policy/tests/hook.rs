//! The installer / metadata-save hook: resolve, lint, and return every problem at once.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{declared, doctype, master, text, transactional};
use spot_sync_policy::{
    PolicySet, QueuedRecords, ScopeKeyType, codes, resolve, verify, verify_change,
};

#[test]
fn a_consistent_install_verifies() {
    let meta = common::build(vec![doctype("item", vec![text("title")])]);
    let table = verify(&meta, &declared("item", master())).expect("a clean policy verifies");
    assert_eq!(table.policy("item").class, spot_sync_policy::Class::Master);
}

#[test]
fn every_problem_is_reported_at_once() {
    let meta = common::build(vec![doctype("item", vec![text("title")])]);
    let mut set = PolicySet::new();
    // A declaration for a DocType the metadata does not hold.
    set.declare("ghost", master());
    // An upward policy with no owner scope.
    set.declare("item", transactional(None));

    let problems = verify(&meta, &set).unwrap_err();
    assert!(problems.iter().any(|p| p.code() == codes::UNKNOWN_DOCTYPE));
    assert!(
        problems
            .iter()
            .any(|p| p.code() == codes::OWNER_SCOPE_MISSING)
    );
}

#[test]
fn a_change_that_orphans_queued_records_is_refused() {
    let meta = common::build(vec![doctype("till_sale", vec![text("total")])]);
    let previous = resolve(
        &meta,
        &declared(
            "till_sale",
            transactional(Some(ScopeKeyType::new("till").unwrap())),
        ),
    );
    let queued = QueuedRecords::with("till_sale", 3);

    let problems = verify_change(&meta, &PolicySet::new(), &previous, &queued).unwrap_err();
    assert!(problems.iter().any(|p| p.code() == codes::ORPHAN_QUEUED));
}

#[test]
fn a_problem_maps_onto_a_classed_app_error() {
    let meta = common::build(vec![doctype("item", vec![text("title")])]);
    let mut set = PolicySet::new();
    set.declare("ghost", master());
    let problem = verify(&meta, &set).unwrap_err().remove(0);
    let app = problem.to_error();
    assert_eq!(app.code, codes::UNKNOWN_DOCTYPE);
    assert_eq!(app.class, spot_app_api::ErrorClass::Validation);
}
