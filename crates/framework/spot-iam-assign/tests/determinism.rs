//! Idempotence, stable order, and speed.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout
)]

mod common;

use common::{T0, counting_ids, later, roles, rule_set, settle};
use proptest::prelude::*;
use spot_iam_assign::{Assignment, Facts, RuleSet, RuleSpec, plan_rules_guarded};

const DEPTS: [&str; 3] = ["Sales", "Finance", "Legal"];
const POSITIONS: [&str; 3] = ["Accountant", "Senior Accountant", "Clerk"];
const COMPANIES: [&str; 2] = ["C1", "C2"];

fn facts(dept: usize, pos: usize, company: Option<usize>, audit: bool) -> Facts {
    let mut f = Facts::new()
        .text("department", DEPTS[dept])
        .text("position", POSITIONS[pos])
        .texts("projects", if audit { &["audit"] } else { &[] });
    if let Some(c) = company {
        f = f.text("company", COMPANIES[c]);
    }
    f
}

fn population() -> impl Strategy<Value = Vec<(usize, usize, Option<usize>, bool)>> {
    prop::collection::vec(
        (
            0..3_usize,
            0..3_usize,
            prop::option::of(0..2_usize),
            any::<bool>(),
        ),
        1..12,
    )
}

fn users_of(p: &[(usize, usize, Option<usize>, bool)]) -> Vec<(String, Facts)> {
    p.iter()
        .enumerate()
        .map(|(i, (d, po, c, a))| (format!("u{i:02}"), facts(*d, *po, *c, *a)))
        .collect()
}

proptest! {
    #[test]
    fn planning_twice_from_the_result_changes_nothing(first in population(), second in population()) {
        let mut ids = counting_ids();
        let set = rule_set();
        let one = users_of(&first);
        let s1 = settle(&plan_rules_guarded(&set, &mut ids, &[], &one, T0, &spot_iam_assign::OutsideGuard::none()), &[]);
        prop_assert!(plan_rules_guarded(&set, &mut ids, &s1, &one, T0, &spot_iam_assign::OutsideGuard::none()).changes.is_empty());
        // Move people about, settle, and it is again stable.
        let two = users_of(&second);
        let s2 = settle(&plan_rules_guarded(&set, &mut ids, &s1, &two, later(1), &spot_iam_assign::OutsideGuard::none()), &s1);
        let again = plan_rules_guarded(&set, &mut ids, &s2, &two, later(1), &spot_iam_assign::OutsideGuard::none());
        prop_assert!(again.changes.is_empty(), "{:?}", again.changes);
    }

    #[test]
    fn the_plan_is_the_same_whatever_order_the_input_comes_in(p in population(), seed in any::<u64>()) {
        let mut ids = counting_ids();
        let set = rule_set();
        let users = users_of(&p);
        let base = settle(&plan_rules_guarded(&set, &mut ids, &[], &users, T0, &spot_iam_assign::OutsideGuard::none()), &[]);
        let mut moved = users_of(&p);
        for u in &mut moved { u.1 = u.1.clone().text("department", "Legal"); }
        let reference = plan_rules_guarded(&set, &mut counting_ids(), &base, &moved, later(1), &spot_iam_assign::OutsideGuard::none());
        let again = plan_rules_guarded(&set, &mut counting_ids(), &shuffle(base.clone(), seed), &shuffle(moved.clone(), seed), later(1), &spot_iam_assign::OutsideGuard::none());
        prop_assert_eq!(reference, again);
    }
}

fn shuffle<T>(mut v: Vec<T>, seed: u64) -> Vec<T> {
    shuffled(&mut v, seed);
    v
}

fn shuffled<T>(v: &mut [T], mut seed: u64) {
    // A small xorshift: no clock, no randomness crate.
    for i in (1..v.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = usize::try_from(seed % (i as u64 + 1)).unwrap_or(0);
        v.swap(i, j);
    }
}

#[test]
fn changes_come_in_user_then_role_then_id_order() {
    let mut ids = counting_ids();
    let users: Vec<(String, Facts)> = (0..6)
        .rev()
        .map(|i| (format!("u{i}"), facts(0, 0, Some(0), true)))
        .collect();
    let plan = plan_rules_guarded(
        &rule_set(),
        &mut ids,
        &[],
        &users,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    let keys: Vec<(String, String, String)> = plan
        .changes
        .iter()
        .map(|c| {
            (
                c.user().to_owned(),
                c.role().as_str().to_owned(),
                c.id().to_owned(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

/// 5,000 users x 40 rules. Run with:
/// `cargo test -p spot-iam-assign --release -- --ignored --nocapture plans_five_thousand`
#[test]
#[ignore = "benchmark: run in release mode"]
fn plans_five_thousand_users_by_forty_rules_in_under_a_second() {
    let mut ids = counting_ids();
    let specs: Vec<RuleSpec> = (0..40)
        .map(|i| {
            toml::from_str(&format!(
                "id='r{i:02}'\nname='n'\nrole='Manager'\nwhen=\"facts.department == '{}' and facts.n % 40 == {i}\"\n\
                 [[scope]]\nlink='Company'\nallowed='facts.company'\n",
                DEPTS[i % 3]
            ))
            .unwrap()
        })
        .collect();
    let set = RuleSet::load(specs, &roles()).unwrap();
    let users: Vec<(String, Facts)> = (0..5000_i64)
        .map(|i| {
            (
                format!("u{i:05}"),
                Facts::new()
                    .text("department", DEPTS[usize::try_from(i % 3).unwrap_or(0)])
                    .text("company", COMPANIES[usize::try_from(i % 2).unwrap_or(0)])
                    .set_value("n", spot_iam_assign::FactValue::Int(i)),
            )
        })
        .collect();
    let started = std::time::Instant::now();
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &[],
        &users,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    let took = started.elapsed();
    println!(
        "planned {} users x {} rules in {took:?}: {}",
        users.len(),
        set.rules().len(),
        plan.summary()
    );
    let current: Vec<Assignment> = plan.apply(&[]);
    let started = std::time::Instant::now();
    let again = plan_rules_guarded(
        &set,
        &mut ids,
        &current,
        &users,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    println!(
        "second pass over {} assignments in {:?}",
        current.len(),
        started.elapsed()
    );
    assert!(again.changes.is_empty());
    assert!(took < std::time::Duration::from_secs(1), "took {took:?}");
}
