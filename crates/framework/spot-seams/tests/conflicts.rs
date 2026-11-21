//! Install-time conflict detection: one test per conflict kind, and all at once.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use spot_app_api::OverrideKind::*;
use spot_seams::{RegistryBuilder, codes};
use std::sync::Arc;

fn build_codes(b: &RegistryBuilder) -> Vec<&'static str> {
    codes_of(b.build())
}

fn codes_of(r: Result<spot_seams::Registry, Vec<spot_seams::Conflict>>) -> Vec<&'static str> {
    codes(r)
}

#[test]
fn a_clean_registry_builds() {
    let mut b = base();
    b.add_override("ext", ov("core:b", Before, Some("ext:x")));
    assert!(b.build().is_ok());
}

#[test]
fn duplicate_step_name_names_both_apps() {
    let mut b = RegistryBuilder::new();
    b.add_controller("core", ctrl("item", vec![decl("core:a", before_save(), 1)]));
    b.add_controller(
        "core",
        ctrl("other", vec![decl("core:a", before_save(), 1)]),
    );
    let err = b.build().err().unwrap();
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].code, codes::STEP_DUPLICATE);
    assert!(err[0].message.contains("core:a"));
}

#[test]
fn a_step_must_belong_to_the_registering_app() {
    let mut b = RegistryBuilder::new();
    b.add_controller("ext", ctrl("item", vec![decl("core:a", before_save(), 1)]));
    let err = b.build().err().unwrap();
    assert_eq!(err[0].code, codes::STEP_FOREIGN_APP);
    assert_eq!(err[0].apps, ["core", "ext"]);
}

#[test]
fn unknown_target_and_unknown_by() {
    let mut b = base();
    b.add_override("ext", ov("core:nope", Before, Some("ext:x")));
    b.add_override("ext", ov("core:a", After, Some("ext:nope")));
    let mut c = build_codes(&b);
    c.sort();
    assert_eq!(
        c,
        [codes::OVERRIDE_UNKNOWN_BY, codes::OVERRIDE_UNKNOWN_TARGET]
    );
}

#[test]
fn a_by_step_of_another_doctype_is_refused() {
    let mut b = base();
    b.add_controller(
        "other",
        ctrl("customer", vec![decl("other:q", before_save(), 1)]),
    );
    b.add_override("ext", ov("core:a", Before, Some("other:q")));
    let err = b.build().err().unwrap();
    assert_eq!(err[0].code, codes::OVERRIDE_DOCTYPE_MISMATCH);
    assert_eq!(err[0].apps, ["core", "ext", "other"]);
}

#[test]
fn disable_with_by_and_others_without_by_are_malformed() {
    let mut b = base();
    b.add_override("ext", ov("core:a", Disable, Some("ext:x")));
    b.add_override("ext", ov("core:b", Replace, None));
    assert_eq!(
        build_codes(&b),
        [codes::OVERRIDE_BAD_SHAPE, codes::OVERRIDE_BAD_SHAPE]
    );
}

#[test]
fn two_replaces_of_one_step() {
    let mut b = base();
    b.add_override("one", ov("core:a", Replace, Some("ext:x")));
    b.add_override("two", ov("core:a", Replace, Some("ext:y")));
    let err = b.build().err().unwrap();
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].code, codes::OVERRIDE_DUPLICATE_REPLACE);
    assert_eq!(err[0].apps, ["core", "one", "two"]);
}

#[test]
fn two_wraps_of_one_step() {
    let mut b = base();
    b.add_override("one", ov("core:a", Wrap, Some("ext:x")));
    b.add_override("two", ov("core:a", Wrap, Some("ext:y")));
    assert_eq!(build_codes(&b), [codes::OVERRIDE_DUPLICATE_WRAP]);
}

#[test]
fn replace_and_wrap_of_one_step() {
    let mut b = base();
    b.add_override("one", ov("core:a", Replace, Some("ext:x")));
    b.add_override("two", ov("core:a", Wrap, Some("ext:y")));
    assert_eq!(build_codes(&b), [codes::OVERRIDE_REPLACE_AND_WRAP]);
}

#[test]
fn disable_with_replace_or_wrap() {
    let mut b = base();
    b.add_override("one", ov("core:a", Disable, None));
    b.add_override("two", ov("core:a", Replace, Some("ext:x")));
    b.add_override("three", ov("core:b", Disable, None));
    b.add_override("four", ov("core:b", Wrap, Some("ext:y")));
    assert_eq!(
        build_codes(&b),
        [
            codes::OVERRIDE_DISABLED_AND_CHANGED,
            codes::OVERRIDE_DISABLED_AND_CHANGED
        ]
    );
}

#[test]
fn disable_with_before_and_after_is_allowed() {
    let mut b = base();
    b.add_override("one", ov("core:a", Disable, None));
    b.add_override("two", ov("core:a", Before, Some("ext:x")));
    assert!(b.build().is_ok());
}

#[test]
fn the_same_before_twice_is_a_duplicate() {
    let mut b = base();
    b.add_override("one", ov("core:a", Before, Some("ext:x")));
    b.add_override("two", ov("core:a", Before, Some("ext:x")));
    assert_eq!(build_codes(&b), [codes::OVERRIDE_DUPLICATE]);
}

#[test]
fn a_wraps_b_and_b_wraps_a_is_a_cycle() {
    let mut b = base();
    b.add_override("ext", ov("core:a", Wrap, Some("core:b")));
    b.add_override("ext", ov("core:b", Wrap, Some("core:a")));
    let err = b.build().err().unwrap();
    assert_eq!(err.len(), 1, "{err:?}");
    assert_eq!(err[0].code, codes::OVERRIDE_CYCLE);
}

#[test]
fn a_longer_loop_and_a_self_override_are_cycles() {
    let mut b = base();
    b.add_override("ext", ov("core:a", Before, Some("core:b")));
    b.add_override("ext", ov("core:b", Before, Some("core:c")));
    b.add_override("ext", ov("core:c", Before, Some("core:a")));
    b.add_override("ext", ov("ext:x", After, Some("ext:x")));
    assert_eq!(
        build_codes(&b),
        [codes::OVERRIDE_CYCLE, codes::OVERRIDE_CYCLE]
    );
}

#[test]
fn a_chain_that_does_not_loop_is_refused_as_chained() {
    let mut b = base();
    b.add_override("ext", ov("core:a", Wrap, Some("ext:x")));
    b.add_override("ext", ov("ext:x", Before, Some("ext:y")));
    assert_eq!(build_codes(&b), [codes::OVERRIDE_CHAINED]);
}

#[test]
fn two_providers_of_one_seam_need_an_explicit_replacement() {
    let mut b = base();
    b.add_seam_provider("tax_a", "tax:line", 1, Arc::new(Const(1.into())));
    b.add_seam_provider("tax_b", "tax:line", 1, Arc::new(Const(2.into())));
    let err = b.build().err().unwrap();
    assert_eq!(err[0].code, codes::SEAM_DUPLICATE_PROVIDER);
    assert_eq!(err[0].apps, ["tax_a", "tax_b"]);
}

#[test]
fn different_versions_of_a_seam_do_not_conflict() {
    let mut b = base();
    b.add_seam_provider("tax_a", "tax:line", 1, Arc::new(Const(1.into())));
    b.add_seam_provider("tax_b", "tax:line", 2, Arc::new(Const(2.into())));
    assert!(b.build().is_ok());
}

#[test]
fn replacing_a_seam_nobody_provides() {
    let mut b = base();
    b.replace_seam("tax_b", "tax:line", 1, Arc::new(Const(2.into())), "why");
    assert_eq!(build_codes(&b), [codes::SEAM_REPLACE_UNKNOWN]);
}

#[test]
fn two_replacements_of_one_seam() {
    let mut b = base();
    b.add_seam_provider("tax_a", "tax:line", 1, Arc::new(Const(1.into())));
    b.replace_seam("r1", "tax:line", 1, Arc::new(Const(2.into())), "why");
    b.replace_seam("r2", "tax:line", 1, Arc::new(Const(3.into())), "why");
    assert_eq!(build_codes(&b), [codes::SEAM_DUPLICATE_REPLACEMENT]);
}

#[test]
fn every_conflict_is_reported_at_once_in_a_stable_order() {
    let mut b = base();
    b.add_override("one", ov("core:a", Replace, Some("ext:x")));
    b.add_override("two", ov("core:a", Replace, Some("ext:y")));
    b.add_override("ext", ov("core:zzz", After, Some("ext:z")));
    b.add_seam_provider("tax_a", "tax:line", 1, Arc::new(Const(1.into())));
    b.add_seam_provider("tax_b", "tax:line", 1, Arc::new(Const(2.into())));
    b.replace_seam("r", "tax:none", 1, Arc::new(Const(3.into())), "why");
    let first = codes_of(b.build());
    assert_eq!(
        first,
        [
            codes::OVERRIDE_DUPLICATE_REPLACE,
            codes::OVERRIDE_UNKNOWN_TARGET,
            codes::SEAM_DUPLICATE_PROVIDER,
            codes::SEAM_REPLACE_UNKNOWN
        ]
    );
}
