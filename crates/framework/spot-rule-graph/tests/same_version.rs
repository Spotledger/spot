//! Same-version reload tests (WP-MET-03.T5): `Registry::reload` decides by
//! compiled content, never by the metadata version.
//!
//! Every metadata set here reports version 1 — two `Registry::new` builds over
//! different definitions each stamp version 1 — so the version is equal across
//! all three reloads while their reports differ. A future `if meta.version() ==
//! current.version()` shortcut would turn the `reloaded` case into `unchanged`
//! (or the `failed` case into a silent keep) and this test would go red.

// Test code may unwrap what the test itself built (spot-dag house pattern).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;

use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry as MetaRegistry};
use spot_rule_graph::error::UNKNOWN_PREDICATE;
use spot_rule_graph::{PredKind, Registry};

fn field(name: &str, ty: FieldType, options: Option<&str>) -> FieldSpec {
    let mut spec = FieldSpec::new(name, ty);
    spec.options = options.map(str::to_owned);
    if ty == FieldType::Decimal {
        spec.scale = Some(2);
    }
    spec
}

/// `account` with `parent_account` and `amount`, and `code` in the type given
/// (`None` removes it).
fn account(code: Option<FieldType>) -> Definition {
    let mut d = Definition::new("account");
    d.fields
        .push(field("parent_account", FieldType::Link, Some("account")));
    d.fields.push(field("amount", FieldType::Decimal, None));
    if let Some(ty) = code {
        let options = (ty == FieldType::Link).then_some("account");
        d.fields.push(field("code", ty, options));
    }
    d
}

fn metadata(code: Option<FieldType>) -> Arc<EffectiveMeta> {
    MetaRegistry::new(vec![account(code)])
        .unwrap_or_else(|errors| panic!("metadata builds: {errors:?}"))
        .effective()
}

#[test]
fn a_same_version_reload_recompiles_by_content_not_by_version() {
    let registered = metadata(Some(FieldType::Text));
    let registry = Registry::new();
    let first = registry
        .register("nodes", "node(A) :- account.code(A, _).", &registered)
        .expect("the rule compiles against a text field");
    let version = first.meta_version();
    assert_eq!(version, registered.version(), "the set records its version");

    // (1) Same version, same content: nothing to do. This is the only report a
    // version-only shortcut could ever produce, so it must stay the honest
    // answer for identical content only.
    let same = metadata(Some(FieldType::Text));
    assert_eq!(same.version(), version);
    let report = registry.reload(&same);
    assert_eq!(report.version, version);
    assert_eq!(report.unchanged, vec!["nodes".to_owned()]);
    assert!(report.reloaded.is_empty());
    assert!(report.failed.is_empty());

    // (2) Same version, `code` is now a link: the atom resolves to the edge
    // `account.code` instead of the field, so the set is recompiled and
    // swapped even though the version did not move.
    let relinked = metadata(Some(FieldType::Link));
    assert_eq!(relinked.version(), version);
    let before = registry
        .get("nodes")
        .expect("the set is registered")
        .hash()
        .clone();
    let report = registry.reload(&relinked);
    assert_eq!(report.version, version);
    assert_eq!(report.reloaded, vec!["nodes".to_owned()]);
    assert!(report.unchanged.is_empty());
    assert!(report.failed.is_empty());
    let swapped = registry.get("nodes").expect("the set is still registered");
    assert_ne!(swapped.hash(), &before, "the content hash must move");
    assert!(
        matches!(
            swapped
                .predicate("account.code")
                .expect("the predicate exists")
                .kind,
            PredKind::Edge
        ),
        "the swapped set reads the new metadata, not the old one"
    );
    assert_eq!(swapped.meta_version(), version);

    // (3) Same version, `code` is gone: the set no longer compiles, is
    // reported, and keeps the previous compiled form (case 2's) untouched.
    let removed = metadata(None);
    assert_eq!(removed.version(), version);
    let report = registry.reload(&removed);
    assert_eq!(report.version, version);
    assert!(report.reloaded.is_empty());
    assert!(report.unchanged.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].name, "nodes");
    assert_eq!(report.failed[0].error.code(), UNKNOWN_PREDICATE);
    let kept = registry
        .get("nodes")
        .expect("a failed set keeps its previous form");
    assert_eq!(kept.hash(), swapped.hash(), "the previous form was kept");
    assert!(matches!(
        kept.predicate("account.code")
            .expect("the predicate survives")
            .kind,
        PredKind::Edge
    ));
}
