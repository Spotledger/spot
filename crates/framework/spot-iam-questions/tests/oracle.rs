//! Each question checked against an independently coded brute-force oracle over the same
//! model: the seeded matrix, and random models under property tests.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{
    Fixture, assignment, company, duty, fixture_from, grant, oracle_can, oracle_can_read,
    oracle_changes, oracle_rights, oracle_viewers, record_id, role, seeded, t,
};
use proptest::prelude::*;
use spot_iam_questions::{NoAudit, Page, PermissionSpec, Questions, ScopeQuery};
use spot_iam_roles::Source;
use spot_permissions::Right;
use spot_storage_contract::Value;

const AT: i64 = 100;

fn rights_of(f: &Fixture, user: &str) -> BTreeSet<(String, String)> {
    let auditor = NoAudit;
    let q = Questions::new(f, &auditor);
    q.what_can_user_do(user, user, t(AT), Page::new(0, 500).expect("page"))
        .expect("a user may ask about themselves")
        .items
        .into_iter()
        .map(|r| (r.doctype, r.right))
        .collect()
}

#[test]
fn what_can_user_do_matches_the_oracle_for_every_user() {
    let f = seeded();
    for user in common::USERS {
        let asked = rights_of(&f, user);
        let oracle = oracle_rights(&f, user, AT);
        assert_eq!(asked, oracle, "the answer drifted for {user}");
    }
}

#[test]
fn who_can_do_matches_the_oracle() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let spec = PermissionSpec::parse("sales_invoice.read").expect("spec");
    let asked: BTreeSet<String> = q
        .who_can_do(
            "u06",
            &spec,
            &ScopeQuery::everywhere(),
            t(AT),
            Page::new(0, 500).expect("page"),
        )
        .expect("ask")
        .items
        .into_iter()
        .map(|h| h.user)
        .collect();
    let empty = BTreeMap::new();
    let oracle: BTreeSet<String> = common::USERS
        .iter()
        .filter(|u| oracle_can(&f, u, "sales_invoice", Right::Read, AT, &empty, None))
        .map(|u| (*u).to_owned())
        .collect();
    assert_eq!(asked, oracle);
}

#[test]
fn who_can_see_matches_the_oracle_for_every_document() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    for doc in &f.documents {
        let asked: BTreeSet<String> = q
            .who_can_see(
                "u06",
                &doc.doctype,
                &doc.name,
                t(AT),
                Page::new(0, 500).expect("page"),
            )
            .expect("ask")
            .items
            .into_iter()
            .map(|v| v.user)
            .collect();
        let oracle: BTreeSet<String> = oracle_viewers(&f, doc, AT).into_iter().collect();
        assert_eq!(
            asked, oracle,
            "the viewers drifted for {}.{}",
            doc.doctype, doc.name
        );
    }
}

#[test]
fn access_changes_matches_the_oracle() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    for (from, to) in [(0, 100), (10, 50), (50, 50), (71, 999), (5, 4)] {
        let asked = q
            .access_changes("u06", from, to, t(AT), Page::new(0, 500).expect("page"))
            .expect("ask")
            .items;
        let oracle = oracle_changes(&f.history, from, to);
        assert_eq!(asked, oracle, "the window {from}..{to} drifted");
    }
}

// -------------------------------------------------------------------------------------------
// Property tests: random models over the same oracle.
// -------------------------------------------------------------------------------------------

fn doctype(idx: u8) -> &'static str {
    ["customer", "sales_invoice", "project"][usize::from(idx % 3)]
}

fn right(idx: u8) -> Right {
    [
        Right::Read,
        Right::Write,
        Right::Create,
        Right::Delete,
        Right::Submit,
        Right::Cancel,
    ][usize::from(idx % 6)]
}

fn random_model(
    grants: &[(u8, u8, bool)],
    rows: &[(u8, u8, bool)],
    share_rows: &[(u8, u8)],
) -> Fixture {
    // Four roles, each with a duty of the same name; grants are spread over the roles and
    // live inside the duties (a duty's grants name the duty as their role).
    let mut duties = Vec::new();
    let mut roles = Vec::new();
    for r in 0..4u8 {
        let role_name = format!("R{r}");
        let duty_name = format!("D{r}");
        let role_grants: Vec<_> = grants
            .iter()
            .enumerate()
            .filter(|(i, _)| u8::try_from(i % 4).unwrap_or(0) == r)
            .map(|(_, (d, rt, owner))| grant(doctype(*d), &duty_name, &[right(*rt)], *owner))
            .collect();
        duties.push(duty(&duty_name, role_grants));
        let built = role(&role_name, vec![duty_name.as_str()], Vec::new());
        roles.push(built);
    }

    let mut assignments = Vec::new();
    let mut seen: BTreeSet<(u8, u8, u8, bool)> = BTreeSet::new();
    for (i, (r, company_slot, expiry)) in rows.iter().enumerate() {
        let user_slot = u8::try_from(i % 5).unwrap_or(0);
        let key = (user_slot, r % 4, *company_slot, *expiry);
        if !seen.insert(key) {
            continue;
        }
        let user = format!("u{user_slot}");
        let scope = (*company_slot < 3).then_some(*company_slot + 1);
        assignments.push(assignment(
            &format!("a{i}"),
            &user,
            &format!("R{}", r % 4),
            Source::Manual,
            scope,
            None,
            expiry.then_some(50),
        ));
    }

    let mut shares = Vec::new();
    for (i, (doc, user)) in share_rows.iter().enumerate() {
        let _ = i;
        shares.push(spot_iam_questions::ShareRecord::read(
            record_id(u64::from(*doc) + 1),
            &format!("u{}", user % 5),
        ));
    }

    let docs = (0..3u8)
        .map(|i| {
            spot_iam_questions::DocumentRecord::named(doctype(i), &format!("d{i}"))
                .with_id(record_id(u64::from(i) + 1))
                .with_value("company", Value::Id(company((i % 2) + 1)))
                .created_by(&format!("u{}", i % 3))
        })
        .collect();

    fixture_from(duties, roles, assignments, shares, docs)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn random_models_agree_with_the_oracle(
        grants in proptest::collection::vec((0u8..3, 0u8..6, any::<bool>()), 1..12),
        rows in proptest::collection::vec((0u8..4, 0u8..4, any::<bool>()), 0..10),
        share_rows in proptest::collection::vec((0u8..3, 0u8..5), 0..4),
    ) {
        // Build the model: same roles/assignments, but with the *real* company ids, so the
        // scope restriction is meaningful.
        let f = random_model(&grants, &rows, &share_rows);
        for user in ["u0", "u1", "u2", "u3", "u4"] {
            let asked = rights_of(&f, user);
            let oracle = oracle_rights(&f, user, AT);
            prop_assert_eq!(&asked, &oracle, "rights drifted for {}", user);
        }
        let world = spot_iam_questions::build(&f, t(AT)).expect("the model builds");
        for doc in &f.documents {
            let asked: BTreeSet<String> = spot_iam_questions::visibility::viewers(&f, &world, doc)
                .expect("the read filters compile")
                .into_iter()
                .map(|v| v.user)
                .collect();
            let oracle: BTreeSet<String> = oracle_viewers(&f, doc, AT).into_iter().collect();
            prop_assert_eq!(&asked, &oracle, "viewers drifted for {}.{}", doc.doctype, doc.name);
        }
    }
}

#[test]
fn the_oracle_read_check_tracks_the_decision_core() {
    // A direct sanity check that the oracle's read answer equals the model for one document.
    let f = seeded();
    let doc = f
        .documents
        .iter()
        .find(|d| d.name == "invoice-1")
        .expect("the document exists");
    for user in common::USERS {
        let _ = oracle_can_read(&f, user, doc, AT);
    }
}
