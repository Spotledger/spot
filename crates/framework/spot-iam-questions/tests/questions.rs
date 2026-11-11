//! The four answerable questions on the seeded matrix: the why-chains, scope, shares, field
//! deny, the permission check, the audit trail, paging and the no-secrets rule.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Fixture, USERS, company, seeded, t};
use spot_iam_questions::{
    ACCESS_GOVERNANCE_DUTY, AskPolicy, MemoryAuditor, NoAudit, Page, PermissionSpec, Questions,
    QuestionsError, ScopeQuery,
};

const AT: i64 = 200;
const EXPIRY: i64 = 50;
const EXPIRED: i64 = 200;

fn rows_for(f: &Fixture, user: &str, at: i64) -> Vec<(String, String, String, Option<String>)> {
    let auditor = NoAudit;
    let q = Questions::new(f, &auditor);
    q.what_can_user_do(user, user, t(at), Page::default())
        .expect("the user may ask about themselves")
        .items
        .into_iter()
        .map(|r| (r.doctype, r.right, r.assignment, r.duty))
        .collect()
}

#[test]
fn what_can_user_do_lists_each_permission_with_its_why_chain() {
    let f = seeded();
    let rows = rows_for(&f, "u02", AT);
    // The Invoice Clerk reads invoices through a duty and may create them directly.
    assert!(
        rows.contains(&(
            "sales_invoice".to_owned(),
            "read".to_owned(),
            "a-u02".to_owned(),
            Some("Read Invoices".to_owned())
        )),
        "the duty-granted read is missing: {rows:?}"
    );
    assert!(
        rows.contains(&(
            "sales_invoice".to_owned(),
            "create".to_owned(),
            "a-u02".to_owned(),
            None
        )),
        "the direct create grant is missing: {rows:?}"
    );
    // It has no customer rights.
    assert!(!rows.iter().any(|(d, ..)| d == "customer"), "{rows:?}");
}

#[test]
fn an_expiring_assignment_is_gone_after_its_window() {
    let f = seeded();
    let before = rows_for(&f, "u12", EXPIRY);
    assert!(
        before.iter().any(|(_, right, ..)| right == "submit"),
        "the grant is missing before expiry: {before:?}"
    );
    let after = rows_for(&f, "u12", EXPIRED);
    assert!(
        !after.iter().any(|(_, right, ..)| right == "submit"),
        "the expired grant is still listed: {after:?}"
    );
}

#[test]
fn who_can_do_respects_the_scope_it_is_asked_in() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let spec = PermissionSpec::parse("sales_invoice.post").expect("a valid spec");
    let company_a = ScopeQuery::at("company", &company(1).to_string());
    let a = q
        .who_can_do("u06", &spec, &company_a, t(AT), Page::default())
        .expect("the governance holder may ask");
    let users: Vec<String> = a.items.iter().map(|h| h.user.clone()).collect();
    assert!(users.contains(&"u03".to_owned()), "u03 posts in company A");
    assert!(
        users.contains(&"u09".to_owned()),
        "the tenant administrator posts anywhere"
    );
    assert!(
        !users.contains(&"u05".to_owned()),
        "u05 posts only in company B"
    );
    assert!(!users.contains(&"u02".to_owned()), "u02 may not post");

    let company_b = ScopeQuery::at("company", &company(2).to_string());
    let b = q
        .who_can_do("u06", &spec, &company_b, t(AT), Page::default())
        .expect("ask");
    let users_b: Vec<String> = b.items.iter().map(|h| h.user.clone()).collect();
    assert!(
        users_b.contains(&"u05".to_owned()),
        "u05 posts in company B"
    );
    assert!(
        !users_b.contains(&"u03".to_owned()),
        "u03 is not scoped to company B"
    );
}

#[test]
fn who_can_see_finds_roles_shares_and_the_tenant_administrator() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let answer = q
        .who_can_see("u06", "sales_invoice", "invoice-1", t(AT), Page::default())
        .expect("ask");
    let users: Vec<String> = answer.items.iter().map(|v| v.user.clone()).collect();
    assert!(
        users.contains(&"u03".to_owned()),
        "u03 reads invoices via a duty"
    );
    assert!(
        users.contains(&"u20".to_owned()),
        "u20 reads it through a share"
    );
    assert!(
        users.contains(&"u09".to_owned()),
        "the tenant administrator sees everything"
    );
    assert!(
        users.contains(&"u02".to_owned()),
        "the creator reads their own document"
    );
    assert!(
        !users.contains(&"u01".to_owned()),
        "u01 has no invoice right: {users:?}"
    );
    assert!(
        !users.contains(&"u10".to_owned()),
        "u10 is scoped to company C"
    );
    let shared = answer
        .items
        .iter()
        .find(|v| v.user == "u20")
        .expect("u20 sees it");
    assert!(shared.shared, "u20's reason is the share");
}

#[test]
fn a_field_deny_beats_a_grant_from_another_role() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let answer = q
        .who_can_see("u06", "sales_invoice", "invoice-1", t(AT), Page::default())
        .expect("ask");
    let level = |user: &str| {
        answer
            .items
            .iter()
            .find(|v| v.user == user)
            .and_then(|v| v.fields.iter().find(|f| f.field == "cost"))
            .map(|f| f.level.clone())
            .unwrap_or_default()
    };
    assert_eq!(level("u08"), "read", "the Cost Viewer may read the cost");
    assert_eq!(
        level("u22"),
        "hidden",
        "the Auditor deny closes the cost for u22"
    );
}

#[test]
fn the_answer_never_carries_a_value() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let answer = q
        .who_can_see("u06", "sales_invoice", "invoice-1", t(AT), Page::default())
        .expect("ask");
    let text = serde_json::to_string(&answer).expect("serialisable");
    assert!(!text.contains("SECRET-COST"), "a value leaked: {text}");
    assert!(!text.contains("cost\" : \""), "the field value leaked");
}

#[test]
fn access_changes_reads_the_window_inclusively() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let all = q
        .access_changes("u06", 0, 100, t(AT), Page::default())
        .expect("ask");
    assert_eq!(all.total, 3);
    let narrow = q
        .access_changes("u06", 0, 20, t(AT), Page::default())
        .expect("ask");
    assert_eq!(narrow.total, 1);
}

#[test]
fn asking_about_others_needs_the_governance_duty() {
    let f = seeded();
    let auditor = MemoryAuditor::new();
    let q = Questions::new(&f, &auditor);
    // u00 asks about themselves: allowed, no duty needed.
    q.what_can_user_do("u00", "u00", t(AT), Page::default())
        .expect("self is allowed");
    // u00 asks about u01: refused, and the refusal is audited.
    let err = q
        .what_can_user_do("u00", "u01", t(AT), Page::default())
        .expect_err("without the duty it is refused");
    assert_eq!(err.code(), spot_iam_questions::codes::NOT_PERMITTED);
    let last = auditor.last().expect("a refusal is audited");
    assert!(!last.allowed, "the refusal is recorded as refused");
    // u06 holds the duty: allowed.
    q.what_can_user_do("u06", "u01", t(AT), Page::default())
        .expect("the governance holder may ask");
}

#[test]
fn a_custom_governance_duty_name_is_honoured() {
    let f = seeded();
    let auditor = NoAudit;
    let policy = AskPolicy {
        governance_duty: "Read Invoices".to_owned(),
    };
    let q = Questions::with_policy(&f, &auditor, policy);
    // u03 holds Read Invoices, so it may now ask about others.
    q.what_can_user_do("u03", "u01", t(AT), Page::default())
        .expect("the configured duty allows it");
    // u01 has no invoice duty: refused.
    let err = q
        .what_can_user_do("u01", "u02", t(AT), Page::default())
        .expect_err("refused");
    assert_eq!(err.code(), spot_iam_questions::codes::NOT_PERMITTED);
}

#[test]
fn every_answered_question_is_audited() {
    let f = seeded();
    let auditor = MemoryAuditor::new();
    let q = Questions::new(&f, &auditor);
    q.what_can_user_do("u06", "u00", t(AT), Page::default())
        .expect("ask");
    let last = auditor.last().expect("an audit row");
    assert_eq!(last.question, "what_can_user_do");
    assert_eq!(last.asker, "u06");
    assert_eq!(last.subject, "u00");
    assert!(last.allowed);
    assert!(last.total > 0);
}

#[test]
fn paging_walks_a_deterministic_answer() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let full = q
        .what_can_user_do("u09", "u09", t(AT), Page::new(0, 500).expect("page"))
        .expect("ask");
    let mut walked = Vec::new();
    let mut offset = 0;
    loop {
        let page = q
            .what_can_user_do("u09", "u09", t(AT), Page::new(offset, 7).expect("page"))
            .expect("ask");
        walked.extend(page.items.iter().map(|r| r.doctype.clone()));
        match page.next {
            Some(next) => offset = next,
            None => break,
        }
    }
    let expected: Vec<String> = full.items.iter().map(|r| r.doctype.clone()).collect();
    assert_eq!(walked, expected, "paging changed the answer");
    assert!(walked.len() > 7, "the matrix is large enough to page");
}

#[test]
fn a_blank_asker_or_user_is_refused() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    assert!(matches!(
        q.what_can_user_do("  ", "u00", t(AT), Page::default()),
        Err(QuestionsError::RequestInvalid { .. })
    ));
    assert!(matches!(
        q.what_can_user_do("u00", "", t(AT), Page::default()),
        Err(QuestionsError::RequestInvalid { .. })
    ));
}

#[test]
fn an_unknown_doctype_or_permission_is_named() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    assert!(matches!(
        PermissionSpec::parse("sales_invoice.frobnicate"),
        Err(QuestionsError::UnknownPermission { .. })
    ));
    let spec = PermissionSpec::new("unknown_doc", "read");
    let err = q
        .who_can_do(
            "u06",
            &spec,
            &ScopeQuery::everywhere(),
            t(AT),
            Page::default(),
        )
        .expect_err("an unknown DocType is refused");
    assert_eq!(err.code(), spot_iam_questions::codes::UNKNOWN_DOCTYPE);
}

#[test]
fn a_missing_document_is_refused() {
    let f = seeded();
    let auditor = NoAudit;
    let q = Questions::new(&f, &auditor);
    let err = q
        .who_can_see("u06", "sales_invoice", "nope", t(AT), Page::default())
        .expect_err("there is no such document");
    assert_eq!(err.code(), spot_iam_questions::codes::REQUEST_INVALID);
}

#[test]
fn the_matrix_has_the_promised_shape() {
    let f = seeded();
    assert_eq!(f.registry.roles().count(), 12, "twelve roles");
    assert_eq!(USERS.len(), 40, "forty users");
    assert!(!f.shares.is_empty(), "a share exists");
    assert!(
        f.assignments.iter().any(|a| a.valid_to.is_some()),
        "an expiring assignment exists"
    );
    let companies: std::collections::BTreeSet<String> = f
        .assignments
        .iter()
        .flat_map(|a| a.scope.restrictions.iter())
        .flat_map(|r| r.allowed.clone())
        .collect();
    assert_eq!(companies.len(), 4, "assignments span four companies");
    let policy_duty = f
        .registry
        .duty(&spot_iam_roles::DutyName::from(ACCESS_GOVERNANCE_DUTY))
        .is_some();
    assert!(policy_duty, "the governance duty exists");
}
