//! Golden plans for the release scenarios, and the golden text and Markdown renderings.
//!
//! Regenerate the golden files with `BLESS=1 cargo test -p spot-migration-plan --test golden`.
//! Without `BLESS`, every rendering is compared byte for byte, so an accidental change to a
//! description, an id or the order of changes fails the test.
#![allow(missing_docs)]

mod common;

use common::{SCENARIOS, scenario};
use spot_migration_plan::plan;

/// Compares `actual` with `tests/golden/<name>.<ext>`, or writes it when `BLESS` is set.
fn check(name: &str, ext: &str, actual: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let path = dir.join(format!("{name}.{ext}"));
    if std::env::var_os("BLESS").is_some() {
        if let Err(error) = std::fs::create_dir_all(&dir) {
            panic!("create {}: {error}", dir.display());
        }
        if let Err(error) = std::fs::write(&path, actual) {
            panic!("write {}: {error}", path.display());
        }
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(expected, actual, "golden mismatch for {}", path.display());
}

#[test]
fn golden_plans_render_text_and_markdown() {
    for name in SCENARIOS {
        let s = scenario(name);
        let release = plan(&s.from, &s.to, &s.schema);
        assert!(!release.plan_digest.is_empty(), "{name}: no digest");
        check(name, "json", &release.json());
        check(name, "txt", &release.render_text());
        check(name, "md", &release.render_markdown());
    }
}

#[test]
fn the_digest_binds_the_plan() {
    let a = scenario("add_field");
    let release = plan(&a.from, &a.to, &a.schema);
    // The same inputs give the same digest.
    let again = plan(&a.from, &a.to, &a.schema);
    assert_eq!(release.plan_digest, again.plan_digest);
    // A different target gives a different digest.
    let b = scenario("remove_field");
    let other = plan(&b.from, &b.to, &b.schema);
    assert_ne!(release.plan_digest, other.plan_digest);
}

#[test]
fn a_plan_with_no_changes_is_empty() {
    let empty = spot_migration_plan::SystemDescription::new();
    let release = plan(&empty, &empty, &spot_migration_plan::SchemaPlan::default());
    assert!(release.is_empty());
    assert!(release.steps().is_empty());
    assert_eq!(release.risk, spot_migration_plan::Risk::Additive);
}
