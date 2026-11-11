//! List results equal the decision core, document by document.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod world;

use spot_perm_query::{PermissionFilter, ReadFilter};
use spot_storage_contract::{CmpOp, Expr, Value};
use world::{DOCTYPES, World, build, target};

/// Every difference between a compiler's filters and the decision core, as text.
async fn mismatches(
    world: &World,
    compile: impl Fn(&World, &spot_permissions::Principal, &str) -> ReadFilter,
) -> Vec<String> {
    let mut out = Vec::new();
    for principal in &world.principals {
        for doctype in DOCTYPES {
            let got = match compile(world, principal, doctype) {
                ReadFilter::All => world.run(doctype, None).await,
                ReadFilter::None => Default::default(),
                ReadFilter::Filter(f) => world.run(doctype, Some(f)).await,
            };
            let want = world.allowed(principal, doctype);
            if got != want {
                out.push(format!(
                    "{} on {doctype}: {} rows listed, {} allowed",
                    principal.user,
                    got.len(),
                    want.len()
                ));
            }
        }
    }
    out
}

fn real(world: &World, p: &spot_permissions::Principal, doctype: &str) -> ReadFilter {
    PermissionFilter::for_read(&world.engine, p, doctype, &target()).unwrap()
}

#[tokio::test]
async fn lists_equal_the_decision_core_over_twenty_seeds() {
    for seed in 1..=20 {
        let world = build(seed).await;
        assert_eq!(world.docs.len(), 500);
        let bad = mismatches(&world, real).await;
        assert!(bad.is_empty(), "seed {seed}: {bad:#?}");
    }
}

#[tokio::test]
async fn the_world_has_every_kind_of_reader() {
    let world = build(7).await;
    let kind = |user: &str, doctype: &str| {
        let p = world.principals.iter().find(|p| p.user == user).unwrap();
        real(&world, p, doctype)
    };
    assert_eq!(kind("alice", "project"), ReadFilter::All);
    assert!(matches!(kind("bob", "project"), ReadFilter::Filter(_)));
    assert!(matches!(kind("dave", "task"), ReadFilter::Filter(_)));
    assert!(matches!(kind("frank", "task"), ReadFilter::Filter(_)));
    assert_eq!(kind("grace", "task"), ReadFilter::None);
    let system = world.principals.iter().find(|p| p.is_system).unwrap();
    assert_eq!(real(&world, system, "task"), ReadFilter::All);
    assert!(
        !world
            .allowed(
                world.principals.iter().find(|p| p.user == "frank").unwrap(),
                "task"
            )
            .is_empty(),
        "the share-only reader reads something"
    );
    let ken = world.principals.iter().find(|p| p.user == "ken").unwrap();
    assert!(
        world.allowed(ken, "task").len()
            < world
                .allowed(
                    world.principals.iter().find(|p| p.user == "alice").unwrap(),
                    "task"
                )
                .len()
    );
}

/// Rewrites every sub-expression `f` returns a replacement for.
fn rewrite(e: &Expr, f: &dyn Fn(&Expr) -> Option<Expr>) -> Expr {
    if let Some(r) = f(e) {
        return r;
    }
    let sub = |x: &Expr| Box::new(rewrite(x, f));
    match e {
        Expr::And(v) => Expr::And(v.iter().map(|x| rewrite(x, f)).collect()),
        Expr::Or(v) => Expr::Or(v.iter().map(|x| rewrite(x, f)).collect()),
        Expr::Not(x) => Expr::Not(sub(x)),
        Expr::Cmp(a, op, b) => Expr::Cmp(sub(a), *op, sub(b)),
        other => other.clone(),
    }
}

fn mutated(
    world: &World,
    p: &spot_permissions::Principal,
    doctype: &str,
    m: &dyn Fn(&Expr) -> Option<Expr>,
) -> ReadFilter {
    match real(world, p, doctype) {
        ReadFilter::Filter(f) => ReadFilter::Filter(rewrite(&f, m)),
        other => other,
    }
}

#[tokio::test]
async fn the_suite_bites_when_one_clause_is_broken() {
    let world = build(3).await;
    assert!(mismatches(&world, real).await.is_empty());

    // 1. The owner clause is dropped: owner-only readers read everything their scope covers.
    let no_owner = |e: &Expr| match e {
        Expr::Cmp(_, CmpOp::Eq, b) if matches!(**b, Expr::Value(Value::Text(_))) => {
            Some(Expr::And(vec![]))
        }
        _ => None,
    };
    // 2. The null allowance is dropped: rows without a company or region disappear.
    let no_null = |e: &Expr| match e {
        Expr::IsNull(_) => Some(Expr::Or(vec![])),
        _ => None,
    };
    // 3. The share list is emptied: shared documents disappear.
    let no_shares = |e: &Expr| match e {
        Expr::In(x, v)
            if v.iter().all(|v| matches!(v, Value::Id(_)))
                && matches!(**x, Expr::Field(ref r) if r.field.as_str() == "id") =>
        {
            Some(Expr::In(x.clone(), vec![]))
        }
        _ => None,
    };
    for (name, m) in [
        (
            "owner clause dropped",
            &no_owner as &dyn Fn(&Expr) -> Option<Expr>,
        ),
        ("null allowance dropped", &no_null),
        ("shares emptied", &no_shares),
    ] {
        let bad = mismatches(&world, |w, p, d| mutated(w, p, d, m)).await;
        assert!(!bad.is_empty(), "the suite did not notice: {name}");
    }
}
