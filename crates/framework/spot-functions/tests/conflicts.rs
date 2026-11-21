//! Builder conflicts: one test per `function.*` conflict kind, all reported at once, and
//! registration order never changing the result.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use spot_functions::{Conflict, FunctionRegistryBuilder, codes};
use std::sync::Arc;

/// A fake handler without keeping its log (the conflict tests never dispatch).
fn fake() -> Arc<dyn spot_functions::FunctionHandler> {
    Arc::new(FakeHandler::new().0)
}

/// A declaration of `app/mod/name`, not guest-callable, registered the manifest way through
/// a `FunctionSub` (module and name validated as the manifest validates them).
fn sub_of(_app: &str, module: &str, name: &str) -> spot_functions::FunctionSub {
    use spot_functions::is_storage_name;
    debug_assert!(is_storage_name(module) && is_storage_name(name));
    spot_manifest::FunctionSub {
        module: spot_app_api::prelude::Name::new(module).unwrap(),
        name: spot_app_api::prelude::Name::new(name).unwrap(),
        http: None,
        guest: false,
    }
}

/// The builder with one function `app1/mod1/fn1` declared the manifest way.
fn builder() -> FunctionRegistryBuilder {
    let mut b = FunctionRegistryBuilder::new();
    b.register("app1", sub_of("app1", "mod1", "fn1"), fake(), None, None);
    b
}

/// The conflict codes of a build, sorted, with the conflicts themselves.
fn conflicts_of(builder: &mut FunctionRegistryBuilder) -> Vec<Conflict> {
    match builder.build() {
        Ok(_) => Vec::new(),
        Err(conflicts) => conflicts,
    }
}

#[test]
fn a_clean_set_builds() {
    let mut b = builder();
    assert!(conflicts_of(&mut b).is_empty());
}

#[test]
fn duplicate_key_names_both_apps() {
    // Two declarations of one key: `app1`'s own, and a host bug declaring another app's
    // function through a hand-built `FunctionDecl` (the app segment of a key always belongs
    // to the declaring app, so this is the only way two apps collide on one key).
    let mut b = FunctionRegistryBuilder::new();
    b.register("app1", sub_of("app1", "mod1", "fn1"), fake(), None, None);
    b.register_decl(
        "app2",
        spot_functions::FunctionDecl {
            key: key("app1", "mod1", "fn1"),
            verbs: None,
            guest: false,
            limit: None,
            arguments: None,
        },
        fake(),
    );
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::DUPLICATE);
    assert_eq!(conflicts[0].apps, vec!["app1", "app2"]);
    assert!(conflicts[0].message.contains("app1/mod1/fn1"));
}

#[test]
fn the_same_key_twice_from_one_app_is_a_duplicate_too() {
    // A manifest bug: the same function twice. Refused the same way, with one app named.
    let mut b = FunctionRegistryBuilder::new();
    b.register("app1", sub_of("app1", "mod1", "fn1"), fake(), None, None);
    b.register("app1", sub_of("app1", "mod1", "fn1"), fake(), None, None);
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::DUPLICATE);
    assert_eq!(conflicts[0].apps, vec!["app1"]);
}

#[test]
fn override_of_unknown_function_is_reported() {
    let mut b = builder();
    b.override_function("other", key("app1", "mod1", "nope"), fake(), "why");
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::OVERRIDE_UNKNOWN);
    assert_eq!(conflicts[0].apps, vec!["other"]);
}

#[test]
fn override_without_reason_is_reported() {
    let mut b = builder();
    b.override_function("other", ok_key(), fake(), "");
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::OVERRIDE_UNKNOWN);
    assert!(conflicts[0].message.contains("reason"));
}

#[test]
fn two_overrides_of_one_function_are_reported() {
    let mut b = builder();
    b.override_function("other1", ok_key(), fake(), "why");
    b.override_function("other2", ok_key(), fake(), "why");
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::OVERRIDE_DUPLICATE);
    assert_eq!(conflicts[0].apps, vec!["other1", "other2"]);
}

#[test]
fn an_app_overriding_its_own_function_is_the_chain() {
    let mut b = builder();
    b.override_function("app1", ok_key(), fake(), "why");
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::OVERRIDE_CHAIN);
    assert_eq!(conflicts[0].apps, vec!["app1"]);
}

#[test]
fn an_override_of_an_override_is_the_chain_from_the_other_side() {
    // app2 overrides app1's fn1; app1 then tries to override its own fn1: its attempt is the
    // chain rule (an app never goes through the override table for its own function). The
    // installed override of app2 stands.
    let mut b = builder();
    b.override_function("app2", ok_key(), fake(), "why");
    b.override_function("app1", ok_key(), fake(), "why");
    let conflicts = conflicts_of(&mut b);
    let codes_found: Vec<&str> = conflicts.iter().map(|c| c.code).collect();
    assert_eq!(codes_found, vec![codes::OVERRIDE_CHAIN], "{codes_found:?}");
}

#[test]
fn http_with_empty_verb_set_is_reported() {
    let mut decl = sub_of("app1", "mod1", "fn1");
    decl.http = Some(Vec::new());
    let mut b = FunctionRegistryBuilder::new();
    b.register("app1", decl, fake(), None, None);
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::VERB_EMPTY);
}

#[test]
fn guest_function_without_verb_list_is_reported() {
    let mut decl = sub_of("app1", "mod1", "fn1");
    decl.guest = true;
    let mut b = FunctionRegistryBuilder::new();
    b.register("app1", decl, fake(), None, None);
    let conflicts = conflicts_of(&mut b);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, codes::GUEST_VERBS);
}

#[test]
fn every_problem_is_reported_at_once() {
    // A duplicate, an empty verb set, a guest without verbs and an unknown override: all
    // come back from one build, sorted.
    let mut dup = sub_of("app1", "mod1", "dup");
    dup.http = Some(Vec::new());
    let mut guesty = sub_of("app1", "mod1", "guesty");
    guesty.guest = true;
    let mut b = FunctionRegistryBuilder::new();
    b.register("app2", dup.clone(), fake(), None, None);
    b.register("app2", dup, fake(), None, None);
    b.register("app1", guesty, fake(), None, None);
    b.override_function("app2", key("app1", "mod1", "ghost"), fake(), "why");
    let conflicts = conflicts_of(&mut b);
    let codes_found: Vec<&str> = conflicts.iter().map(|c| c.code).collect();
    assert!(codes_found.contains(&codes::DUPLICATE), "{codes_found:?}");
    assert!(codes_found.contains(&codes::VERB_EMPTY), "{codes_found:?}");
    assert!(codes_found.contains(&codes::GUEST_VERBS), "{codes_found:?}");
    assert!(
        codes_found.contains(&codes::OVERRIDE_UNKNOWN),
        "{codes_found:?}"
    );
}

#[test]
fn registration_order_never_changes_the_conflict_report() {
    // The same two apps declaring one key and one unknown-target override, registered in
    // every permutation: exactly the same report.
    let make = |order: [u8; 3]| {
        let mut b = FunctionRegistryBuilder::new();
        for step in order {
            match step {
                0 => b.register("app1", sub_of("app1", "mod1", "fn1"), fake(), None, None),
                1 => b.register("app1", sub_of("app1", "mod1", "fn1"), fake(), None, None),
                _ => b.override_function("app3", key("app9", "mod1", "fn1"), fake(), "why"),
            };
        }
        conflicts_of(&mut b)
    };
    let first = make([0, 1, 2]);
    for order in [[1, 0, 2], [2, 1, 0], [2, 0, 1], [1, 2, 0], [0, 2, 1]] {
        assert_eq!(make(order), first, "order: {order:?}");
    }
    let codes_found: Vec<&str> = first.iter().map(|c| c.code).collect();
    assert!(codes_found.contains(&codes::DUPLICATE), "{codes_found:?}");
    assert!(
        codes_found.contains(&codes::OVERRIDE_UNKNOWN),
        "{codes_found:?}"
    );
}

#[test]
fn registration_order_never_changes_the_built_registry() {
    // The clean case: two functions and a valid override, registered in either order, give
    // the same listing and the same handler choice.
    let (replacer, _) = FakeHandler::new();
    let make = |swap: bool| {
        let mut b = FunctionRegistryBuilder::new();
        let a = sub_of("app1", "mod1", "fn1");
        let c = sub_of("app1", "mod2", "fn2");
        if swap {
            b.register("app1", c.clone(), fake(), None, None);
            b.register("app1", a.clone(), fake(), None, None);
            b.override_function(
                "app2",
                key("app1", "mod1", "fn1"),
                Arc::new(replacer.clone()),
                "why",
            );
        } else {
            b.register("app1", a.clone(), fake(), None, None);
            b.register("app1", c.clone(), fake(), None, None);
            b.override_function(
                "app2",
                key("app1", "mod1", "fn1"),
                Arc::new(replacer.clone()),
                "why",
            );
        }
        match b.build() {
            Ok(registry) => registry,
            Err(conflicts) => panic!("should build: {conflicts:?}"),
        }
    };
    let one = make(false);
    let two = make(true);
    // `FunctionInfo` carries a handler-free view, but it does not derive `Eq`; compare the
    // stable fields through the debug form, which is derived.
    assert_eq!(format!("{:?}", one.list()), format!("{:?}", two.list()));
}

#[test]
fn a_valid_override_builds_and_registers() {
    let mut b = builder();
    b.override_function("other", ok_key(), fake(), "replacing for the test");
    assert!(conflicts_of(&mut b).is_empty());
}
