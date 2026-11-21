//! Snapshot tests: the generated source for the 3 sample DocTypes, one snapshot per DocType
//! (the house `snapshots__<name>.snap` naming under `tests/snapshots/`).
//!
//! The emitted file is deterministic: the same definitions render the same source, in the
//! order load_dir reads (`invoice`, `invoice_line`, `party`) — the fixture set of the brief:
//! a posted DocType with a child table and a select field, a master, a child.

use spot_meta::load_dir;
use spot_typed_views::generate;

/// The directory of the 3 sample DocTypes the brief fixes.
fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/doctypes")
}

/// Loads the fixtures and renders the generated source.
///
/// # Panics
/// A fixture that no longer loads or generates: the fixtures are checked-in and valid, so
/// either is a test-environment bug.
fn generated() -> String {
    let definitions = load_dir(&fixture_dir())
        .unwrap_or_else(|e| panic!("fixture definitions no longer load: {e:?}"));
    generate(&definitions).unwrap_or_else(|e| panic!("fixtures no longer generate: {e:?}"))
}

#[test]
fn snapshot_generated_source() {
    insta::assert_snapshot!("generated_source", generated());
}

/// The known bugs the review of the draft emitter called out, checked against the current
/// output so a regression cannot land quietly:
/// - field idents keep the storage snake_case verbatim (`posting_date`, never `postingDate`);
/// - the select getter routes through the option enum's `parse`, so an unknown stored value
///   is an `AppError`, never a panic;
/// - the table accessors' doc comments read as sentences of the child table.
#[test]
fn generated_source_keeps_the_reviewed_shapes() {
    let source = generated();
    // Field members verbatim snake_case, in both getter and setter names.
    assert!(
        source.contains("pub fn posting_date(&self)"),
        "field getter must keep the storage name verbatim:\n{source}"
    );
    assert!(
        source.contains("pub fn set_posting_date(&mut self"),
        "field setter must keep the storage name verbatim:\n{source}"
    );
    assert!(
        !source.contains("postingDate"),
        "camelCase member leaked into the generated source:\n{source}"
    );
    // The select getter parses into the enum; unknown values error, never panic.
    assert!(
        source.contains("pub fn status(&self) -> AppResult<Option<StatusOption>>"),
        "select getter must return the option enum:\n{source}"
    );
    assert!(
        source.contains("StatusOption::parse(t).map(Some)"),
        "select getter must route through the enum's parse:\n{source}"
    );
    // Doc comments carry one leading space, so prettyplease renders `/// The …`.
    assert!(
        !source.matches("///").any(|d| d.is_empty()),
        "every doc comment must start with a space:\n{source}"
    );
    assert!(
        source.contains("/// One row of the invoice Line child table (`invoice_line`)."),
        "row doc must word the child table:\n{source}"
    );
}
