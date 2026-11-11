//! Adding an assignment never removes the rights another assignment gave.
#![allow(missing_docs, clippy::unwrap_used)]

mod common;

use common::{action, at, customers, engine, invoice, only_customers, registry};
use proptest::prelude::*;
use spot_iam_roles::{Assignment, Scope, build_snapshot_input};

const ROLES: [&str; 5] = [
    "Accounting Viewer",
    "Accounting Manager",
    "Senior Accountant",
    "Clerk",
    "Own Invoices",
];
const ACTIONS: [&str; 4] = ["read", "update", "create", "post"];

/// (role index, scope: 0 none, 1 c1, 2 c2, 3 c1+c2)
fn spec() -> impl Strategy<Value = (usize, usize)> {
    (0..ROLES.len(), 0..4_usize)
}

fn assignment(n: usize, (role, scope): (usize, usize)) -> Assignment {
    let c = customers();
    let ids = match scope {
        1 => vec![c["c1"]],
        2 => vec![c["c2"]],
        3 => vec![c["c1"], c["c2"]],
        _ => vec![],
    };
    Assignment {
        scope: Scope {
            restrictions: if ids.is_empty() {
                vec![]
            } else {
                vec![only_customers("u", &ids)]
            },
        },
        ..Assignment::new(&format!("a{n}"), "u", ROLES[role])
    }
}

proptest! {
    #[test]
    fn adding_an_assignment_never_removes_a_right(
        existing in proptest::collection::vec(spec(), 0..4),
        extra in spec(),
    ) {
        let c = customers();
        // The same (role, scope) twice is a duplicate assignment, which the crate refuses.
        let mut distinct = existing.clone();
        distinct.sort_unstable();
        distinct.dedup();
        prop_assume!(!distinct.contains(&extra));
        let base: Vec<Assignment> = distinct.iter().enumerate().map(|(i, s)| assignment(i, *s)).collect();
        let mut more = base.clone();
        more.push(assignment(99, extra));
        let r = registry();
        let before = build_snapshot_input(&r, &base, at(0)).unwrap();
        let after = build_snapshot_input(&r, &more, at(0)).unwrap();
        let (e1, e2) = (engine(&before), engine(&after));
        let (p1, p2) = (before.principal("u"), after.principal("u"));
        for customer in ["c1", "c2", "c3"] {
            for act in ACTIONS {
                let doc = invoice(c[customer], "draft");
                let was = e1.can(&p1, action(act), &doc).allowed;
                let now = e2.can(&p2, action(act), &doc).allowed;
                prop_assert!(!was || now, "{act} on {customer} was allowed and is not any more");
            }
        }
    }
}
