//! Execution plans: ordering, every override kind and their combinations, and explanations.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use spot_app_api::OverrideKind::*;
use spot_app_api::{HookPoint, Operation, Phase, Target};
use spot_seams::Role;

fn core_names(b: &spot_seams::RegistryBuilder) -> Vec<String> {
    names(b.build().unwrap().plan("item", &before_save()))
}

#[test]
fn steps_run_in_priority_then_name_order() {
    let mut b = spot_seams::RegistryBuilder::new();
    b.add_controller(
        "a",
        ctrl(
            "item",
            vec![
                decl("a:late", before_save(), 20),
                decl("a:tie_b", before_save(), 10),
                decl("a:tie_a", before_save(), 10),
                decl("a:first", before_save(), -5),
            ],
        ),
    );
    assert_eq!(core_names(&b), ["a:first", "a:tie_a", "a:tie_b", "a:late"]);
}

#[test]
fn an_unknown_doctype_or_point_has_an_empty_plan() {
    let r = base().build().unwrap();
    assert!(r.plan("nothing", &before_save()).is_empty());
    assert!(
        r.plan("item", &HookPoint::FieldChanged("qty".into()))
            .is_empty()
    );
}

#[test]
fn before_and_after_are_adjacent() {
    let mut b = base();
    b.add_override("ext", ov("core:b", Before, Some("ext:x")));
    b.add_override("ext", ov("core:b", After, Some("ext:y")));
    assert_eq!(
        core_names(&b),
        ["core:a", "ext:x", "core:b", "ext:y", "core:c"]
    );
}

#[test]
fn several_befores_run_in_priority_then_name_order_of_their_by_steps() {
    let mut b = spot_seams::RegistryBuilder::new();
    b.add_controller(
        "a",
        ctrl(
            "item",
            vec![
                decl("a:t", before_save(), 0),
                decl("a:p9", before_save(), 9),
                decl("a:p1b", before_save(), 1),
                decl("a:p1a", before_save(), 1),
            ],
        ),
    );
    // Registered in the "wrong" order on purpose.
    for by in ["a:p9", "a:p1b", "a:p1a"] {
        b.add_override("x", ov("a:t", Before, Some(by)));
        b.add_override("x", ov("a:t", After, Some(by)));
    }
    assert_eq!(
        core_names(&b),
        ["a:p1a", "a:p1b", "a:p9", "a:t", "a:p1a", "a:p1b", "a:p9"]
    );
}

#[test]
fn wrap_puts_the_wrapper_in_the_targets_slot() {
    let mut b = base();
    b.add_override("ext", ov("core:b", Wrap, Some("ext:x")));
    let r = b.build().unwrap();
    let plan = r.plan("item", &before_save());
    assert_eq!(names(plan), ["core:a", "ext:x", "core:c"]);
    assert_eq!(plan[1].wrapped.as_ref().unwrap().step.as_str(), "core:b");
    assert_eq!(plan[1].wrapped.as_ref().unwrap().app, "core");
    assert!(plan[0].wrapped.is_none());
}

#[test]
fn replace_substitutes_and_disable_removes() {
    let mut b = base();
    b.add_override("ext", ov("core:a", Replace, Some("ext:x")));
    b.add_override("ext", ov("core:c", Disable, None));
    assert_eq!(core_names(&b), ["ext:x", "core:b"]);
}

#[test]
fn a_by_step_is_detached_from_its_own_point() {
    // ext:x is declared at `load`, so it runs there on its own...
    let r = base().build().unwrap();
    assert_eq!(
        names(r.plan("item", &HookPoint::Load)),
        ["ext:x", "ext:y", "ext:z"]
    );
    // ...until an override uses it: then it runs only where the override puts it.
    let mut b = base();
    b.add_override("ext", ov("core:b", Before, Some("ext:x")));
    let r = b.build().unwrap();
    assert_eq!(names(r.plan("item", &HookPoint::Load)), ["ext:y", "ext:z"]);
    assert_eq!(
        names(r.plan("item", &before_save())),
        ["core:a", "ext:x", "core:b", "core:c"]
    );
}

#[test]
fn before_wrap_and_after_combine_on_one_step() {
    let mut b = base();
    b.add_override("ext", ov("core:b", Before, Some("ext:x")));
    b.add_override("ext", ov("core:b", Wrap, Some("ext:y")));
    b.add_override("ext", ov("core:b", After, Some("ext:z")));
    let r = b.build().unwrap();
    let plan = r.plan("item", &before_save());
    assert_eq!(names(plan), ["core:a", "ext:x", "ext:y", "ext:z", "core:c"]);
    assert_eq!(plan[2].wrapped.as_ref().unwrap().step.as_str(), "core:b");
}

#[test]
fn before_and_after_survive_a_disabled_or_replaced_target() {
    let mut b = base();
    b.add_override("ext", ov("core:b", Disable, None));
    b.add_override("ext", ov("core:b", Before, Some("ext:x")));
    b.add_override("ext", ov("core:a", Replace, Some("ext:y")));
    b.add_override("ext", ov("core:a", After, Some("ext:z")));
    assert_eq!(core_names(&b), ["ext:y", "ext:z", "ext:x", "core:c"]);
}

#[test]
fn save_points_and_operation_points_are_planned_apart() {
    let mut b = spot_seams::RegistryBuilder::new();
    b.add_controller(
        "a",
        ctrl(
            "item",
            vec![
                decl("a:on_save", before_save(), 1),
                decl("a:on_create", before_create(), 1),
                decl(
                    "a:validate",
                    HookPoint::of(Phase::Validate, Target::Op(Operation::Create)),
                    1,
                ),
                decl("a:field", HookPoint::FieldChanged("qty".into()), 1),
            ],
        ),
    );
    let r = b.build().unwrap();
    assert_eq!(names(r.plan("item", &before_save())), ["a:on_save"]);
    assert_eq!(names(r.plan("item", &before_create())), ["a:on_create"]);
    assert_eq!(
        names(r.plan("item", &HookPoint::FieldChanged("qty".into()))),
        ["a:field"]
    );
    assert!(
        r.plan("item", &HookPoint::FieldChanged("rate".into()))
            .is_empty()
    );
}

#[test]
fn doctypes_are_planned_apart() {
    let mut b = spot_seams::RegistryBuilder::new();
    b.add_controller("a", ctrl("item", vec![decl("a:x", before_save(), 1)]));
    b.add_controller("a", ctrl("customer", vec![decl("a:y", before_save(), 1)]));
    let r = b.build().unwrap();
    assert_eq!(names(r.plan("item", &before_save())), ["a:x"]);
    assert_eq!(names(r.plan("customer", &before_save())), ["a:y"]);
}

#[test]
fn explain_lists_each_step_with_its_overrides_and_reasons() {
    let mut b = base();
    b.add_override("ext", ov("core:b", Before, Some("ext:x")));
    b.add_override("ext", ov("core:b", Wrap, Some("ext:y")));
    b.add_override("ext", ov("core:c", Disable, None));
    let r = b.build().unwrap();
    let lines = r.explain("item", &before_save());
    let summary: Vec<(String, bool)> = lines.iter().map(|l| (l.step.to_string(), l.runs)).collect();
    assert_eq!(
        summary,
        [
            ("core:a".into(), true),
            ("ext:x".into(), true),
            ("ext:y".into(), true),
            ("core:c".into(), false),
        ]
    );
    assert_eq!(lines[0].role, Role::Declared);
    assert!(lines[0].overrides.is_empty());
    assert_eq!(
        lines[1].role,
        Role::Before {
            target: sn("core:b")
        }
    );
    assert_eq!(lines[1].overrides.len(), 1);
    assert_eq!(
        lines[2].role,
        Role::Wrapper {
            target: sn("core:b")
        }
    );
    assert_eq!(lines[2].overrides.len(), 2);
    assert_eq!(lines[2].overrides[0].reason, "Wrap core:b");
    assert_eq!(lines[2].overrides[0].declared_by, "ext");
    assert_eq!(lines[3].overrides[0].reason, "Disable core:c");
    // The planned lines match the plan one to one.
    let running: Vec<String> = lines
        .iter()
        .filter(|l| l.runs)
        .map(|l| l.step.to_string())
        .collect();
    assert_eq!(running, names(r.plan("item", &before_save())));
    assert_eq!(r.overrides().len(), 3);
}
