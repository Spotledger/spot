//! Grant request, approval, agent binding, the duration cap and the record shape.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use spot_support_grants::{Grant, GrantPolicy, GrantStore, codes};
use spot_support_session::{SubjectKind, Tier};

#[test]
fn approval_requires_a_registered_customer_admin() {
    let mut h = Harness::new();
    let pending = h.pending(
        "g-admin",
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    let error = h
        .plane()
        .approve(pending, "stranger", 100)
        .expect_err("a stranger is not an administrator");
    assert_eq!(error.code(), codes::NOT_ADMIN);
    assert!(h.grants.list(TENANT).unwrap().is_empty());
}

#[test]
fn nobody_approves_their_own_grant() {
    let mut h = Harness::new();
    // The requester is made an administrator: they still may not approve their own request.
    h.admins.add(TENANT, REQUESTER, true);
    let pending = h.pending("g-self", Tier::T1, scope(&["Factura"], &["read"]), 100, 200);
    let error = h
        .plane()
        .approve(pending, REQUESTER, 100)
        .expect_err("the requester cannot approve");
    assert_eq!(error.code(), codes::SELF_APPROVAL);

    // Nor may the subject (the agent) approve the grant made to it.
    h.admins.add(TENANT, AGENT, true);
    let pending = h.pending(
        "g-self2",
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    let error = h
        .plane()
        .approve(pending, AGENT, 100)
        .expect_err("the subject cannot approve");
    assert_eq!(error.code(), codes::SELF_APPROVAL);
}

#[test]
fn a_high_tier_grant_needs_a_tenant_owner() {
    let mut h = Harness::new();
    // admin-2 is a customer administrator but not a tenant owner.
    let pending = h.pending("g-t2", Tier::T2, scope(&["Factura"], &["read"]), 100, 200);
    let error = h
        .plane()
        .approve(pending, ADMIN2, 100)
        .expect_err("T2 needs an owner");
    assert_eq!(error.code(), codes::SESSION_REFUSED);

    let grant = h.grant(
        "g-t2-owner",
        Tier::T2,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    assert_eq!(grant.approved_by(), ADMIN);
    assert_eq!(grant.tier(), Tier::T2);
}

#[test]
fn agents_must_be_registered_support_agents_at_the_ceiling() {
    let mut h = Harness::new();

    let ops = spec(
        "g-ops",
        "ops-agent",
        SubjectKind::Agent,
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    let error = h.plane().request(ops, 100).expect_err("not support");
    assert_eq!(error.code(), codes::AGENT_REFUSED);

    let low = spec(
        "g-low",
        "low-agent",
        SubjectKind::Agent,
        Tier::T2,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    let error = h
        .plane()
        .request(low, 100)
        .expect_err("above the agent's tier");
    assert_eq!(error.code(), codes::AGENT_REFUSED);

    let suspended = spec(
        "g-suspended",
        "suspended-agent",
        SubjectKind::Agent,
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    let error = h.plane().request(suspended, 100).expect_err("suspended");
    assert_eq!(error.code(), codes::AGENT_REFUSED);

    let unknown = spec(
        "g-unknown",
        "nobody",
        SubjectKind::Agent,
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    let error = h.plane().request(unknown, 100).expect_err("not enrolled");
    assert_eq!(error.code(), codes::AGENT_REFUSED);

    // A support person is not an agent and needs no registry entry.
    let person = spec(
        "g-person",
        "person-1",
        SubjectKind::Person,
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        200,
    );
    h.plane()
        .request(person, 100)
        .expect("a person may request");
}

#[test]
fn no_grant_can_exceed_the_configured_duration_or_live_forever() {
    let mut h = Harness::new();
    let five_hours = spec(
        "g-long",
        AGENT,
        SubjectKind::Agent,
        Tier::T1,
        scope(&["Factura"], &["read"]),
        0,
        5 * 3600,
    );
    let error = h.plane().request(five_hours, 0).expect_err("over 4 hours");
    assert_eq!(error.code(), codes::DURATION_OVER_POLICY);

    // There is no "permanent" grant: the end is a mandatory instant, and the longest
    // expressible end is still refused by the cap.
    let forever = spec(
        "g-forever",
        AGENT,
        SubjectKind::Agent,
        Tier::T1,
        scope(&["Factura"], &["read"]),
        0,
        i64::MAX,
    );
    let error = h
        .plane()
        .request(forever, 0)
        .expect_err("no permanent grant");
    assert_eq!(error.code(), codes::DURATION_OVER_POLICY);

    // Exactly the cap is allowed.
    h.plane()
        .request(
            spec(
                "g-cap",
                AGENT,
                SubjectKind::Agent,
                Tier::T1,
                scope(&["Factura"], &["read"]),
                0,
                4 * 3600,
            ),
            0,
        )
        .expect("exactly the cap");

    // A deployment may be stricter than the session model's default.
    h.policy = GrantPolicy {
        max_duration_secs: 3600,
        ..GrantPolicy::default()
    };
    h.grant("g-hour", Tier::T1, scope(&["Factura"], &["read"]), 0, 3600);
    let two_hours = spec(
        "g-two-hours",
        AGENT,
        SubjectKind::Agent,
        Tier::T1,
        scope(&["Factura"], &["read"]),
        0,
        7200,
    );
    let error = h.plane().request(two_hours, 0).expect_err("over 1 hour");
    assert_eq!(error.code(), codes::DURATION_OVER_POLICY);
}

#[test]
fn blank_fields_and_conditions_are_refused() {
    let mut h = Harness::new();
    let mut blank_org = agent_spec("g-org", Tier::T1, scope(&["Factura"], &["read"]), 0, 100);
    blank_org.support_org = "  ".to_owned();
    assert_eq!(
        h.plane().request(blank_org, 0).unwrap_err().code(),
        codes::INVALID_REQUEST
    );

    let mut blank_condition =
        agent_spec("g-cond", Tier::T1, scope(&["Factura"], &["read"]), 0, 100);
    blank_condition.conditions.push(" ".to_owned());
    assert_eq!(
        h.plane().request(blank_condition, 0).unwrap_err().code(),
        codes::INVALID_REQUEST
    );

    let mut too_many = agent_spec("g-many", Tier::T1, scope(&["Factura"], &["read"]), 0, 100);
    too_many.conditions = (0..9).map(|n| format!("condition {n}")).collect();
    assert_eq!(
        h.plane().request(too_many, 0).unwrap_err().code(),
        codes::INVALID_REQUEST
    );
}

#[test]
fn a_grant_round_trips_through_json_with_its_conditions() {
    let mut h = Harness::new();
    let mut spec = agent_spec("g-json", Tier::T1, scope(&["Factura"], &["read"]), 100, 200);
    spec.conditions = vec!["read-only outside incidents".to_owned()];
    let pending = h.plane().request(spec, 100).unwrap();
    let grant = h.plane().approve(pending, ADMIN, 100).unwrap();

    let json = serde_json::to_string(&grant).expect("serialises");
    let back: Grant = serde_json::from_str(&json).expect("reads back");
    assert_eq!(back, grant);
    assert_eq!(back.conditions, ["read-only outside incidents"]);
    assert!(back.summary(150).contains("support-org"));
    assert!(back.summary(150).contains("read-only outside incidents"));
    assert_eq!(back.valid_from(), 100);
    assert_eq!(back.valid_until(), 200);
    assert!(back.is_running(150));
}

#[test]
fn duplicate_ids_are_refused_by_the_store() {
    let mut h = Harness::new();
    h.grant("g-dup", Tier::T1, scope(&["Factura"], &["read"]), 100, 200);
    let pending = h.pending("g-dup", Tier::T1, scope(&["Factura"], &["read"]), 100, 200);
    let error = h
        .plane()
        .approve(pending, ADMIN, 100)
        .expect_err("the id is taken");
    assert_eq!(error.code(), codes::DUPLICATE_GRANT);
}
