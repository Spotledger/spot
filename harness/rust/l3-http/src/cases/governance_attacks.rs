//! The acceptance matrix of WP-IAM-08 and WP-SEC-02 over HTTP, with a hostile client (WP-IAM-08.T10):
//! a conflicting grant is blocked unless overridden with a reason and a mitigating control; an emergency
//! session is reviewed by someone other than its user; the four access questions are correct on a seeded
//! matrix; and the attacks: self-approval, a grant path that skips the gate, a replayed or late decision,
//! a reviewer deciding or forging someone else's item, and a cursor or id of another user or tenant.
//!
//! Every case here is **blocked**, and the report says why. The operations exist in the native API contract
//! (`governance_*` and `identity_assignment_*`, 0.26.0) and their rules are pinned by the unit tests of
//! `spot-iam-governance`, `spot-sod`, `spot-access-review` and the hosting modules (`grant_gate`,
//! `access_questions`, `access_review`), but **no route serves them yet**: the identity and governance
//! groups are not mounted over the hosted ports (WP-IAM-08.T7, WP-IAM-08.T9, WP-SEC-02.T4 host the logic
//! behind ports only), the matrix fixture needs a stored role and assignment model behind a `RoleSource`
//! adapter (WP-IAM-03.T2 follow-up) and the harness has no second tenant or second node. Each entry
//! states the exact status and code the case must see once it can run, taken from the contract's error
//! registry, so the blocked list is the checklist for the follow-up. A case that unexpectedly passes is a
//! security finding, never a case to adjust.

use super::CaseDef;

/// After the sessions, roles and offline identity cases.
pub const ORDER: u32 = 22;

const SECTION: &str = "Governance attacks";
const OWNER: &str = "WP-IAM-08.T10 follow-up: the governance and identity routes mounted over the hosted ports, and the seeded matrix fixture";
const NO_ROUTES: &str = "needs the governance and identity routes mounted over the hosted ports (no route serves them yet)";
const NO_MATRIX: &str = "needs the seeded role matrix behind a RoleSource adapter and the routes";
const NO_TENANT: &str = "needs a second tenant in the harness and the routes";

pub fn cases() -> Vec<CaseDef> {
    vec![
        // ---- the acceptance sentences ----
        CaseDef::blocked(
            "governance_attacks: a_conflicting_grant_is_blocked_with_the_rule_ids_only",
            SECTION,
            "identity_assignment_create for a user who would conflict with a segregation rule answers 409 identity.sod_conflict with details.rule_ids and no other user's data, and no assignment exists afterwards",
            OWNER,
            NO_MATRIX,
        ),
        CaseDef::blocked(
            "governance_attacks: an_override_with_a_reason_and_a_control_lets_exactly_that_grant_through_until_it_expires",
            SECTION,
            "governance_sod_override_create (201) for one rule, user and scope with a reason and a mitigating control lets the same identity_assignment_create succeed; another user, another scope and the same grant after the expiry answer 409 identity.sod_conflict again",
            OWNER,
            NO_MATRIX,
        ),
        CaseDef::blocked(
            "governance_attacks: an_override_without_a_control_or_a_reason_or_an_expiry_is_refused",
            SECTION,
            "governance_sod_override_create without the mitigating control, without the reason, or without an expiry answers 422 governance.override_invalid and stores nothing",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: nobody_overrides_a_conflict_of_their_own_account",
            SECTION,
            "governance_sod_override_create naming the caller's own user answers 403 governance.self_approval, audited as a refusal",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: an_emergency_session_is_reviewed_by_someone_other_than_its_user",
            SECTION,
            "governance_emergency_create is time-boxed and notifies the named controller; governance_emergency_review by that controller (200) closes the loop; the same call by the beneficiary answers 403 governance.self_approval",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: the_controller_of_an_emergency_cannot_be_its_beneficiary",
            SECTION,
            "governance_emergency_create naming the caller as controller answers 403 governance.controller_is_beneficiary and creates no access",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: the_four_access_questions_are_correct_on_the_seeded_matrix",
            SECTION,
            "governance_question_user_permissions, governance_question_who_can, governance_question_who_can_see and governance_question_changes answer exactly what the seeded matrix of roles, scopes and assignments implies, and a user without governance.questions.use gets 403",
            OWNER,
            NO_MATRIX,
        ),
        // ---- the attacks ----
        CaseDef::blocked(
            "governance_attacks: a_requester_cannot_approve_their_own_grant_request",
            SECTION,
            "governance_grant_request_decide by the requester, and by the beneficiary, answers 403 governance.self_approval and the request stays pending",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: a_grant_request_id_of_another_tenant_answers_like_a_missing_one",
            SECTION,
            "governance_grant_request_decide with a guessed request id of another tenant answers 404 governance.not_found, byte-identical to an id that never existed",
            OWNER,
            NO_TENANT,
        ),
        CaseDef::blocked(
            "governance_attacks: a_decided_request_cannot_be_decided_again",
            SECTION,
            "a second governance_grant_request_decide for a request already approved or rejected (a replay) answers 409 governance.request_not_pending and changes nothing",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: an_expired_request_cannot_be_decided",
            SECTION,
            "governance_grant_request_decide after the request's expiry answers 409 governance.request_not_pending and no assignment is created",
            OWNER,
            "needs the routes and a controllable clock",
        ),
        CaseDef::blocked(
            "governance_attacks: the_assignment_rule_route_cannot_grant_across_a_conflict",
            SECTION,
            "identity_assignment_rules_run for a rule that would give a conflicting role skips that assignment with 409 identity.sod_conflict in its outcome and writes none",
            OWNER,
            NO_MATRIX,
        ),
        CaseDef::blocked(
            "governance_attacks: the_scim_route_cannot_grant_across_a_conflict",
            SECTION,
            "a SCIM group membership that would give a conflicting role is refused with the SCIM conflict answer (409) and no assignment exists",
            OWNER,
            "needs the SCIM route over the grant gate (the exact SCIM error body is an open question) and the matrix",
        ),
        CaseDef::blocked(
            "governance_attacks: the_emergency_route_cannot_grant_across_a_conflict",
            SECTION,
            "governance_emergency_create for a role that conflicts with the beneficiary's own roles answers 409 identity.sod_conflict unless an active override covers it",
            OWNER,
            NO_MATRIX,
        ),
        CaseDef::blocked(
            "governance_attacks: a_reviewer_cannot_decide_another_reviewers_item",
            SECTION,
            "governance_review_item_decide on an item whose reviewer is someone else answers 403 governance.not_reviewer and the item is unchanged",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: a_reviewer_cannot_name_themselves_or_forge_an_item",
            SECTION,
            "governance_review_item_decide with a reviewer field in the body, or an item id that is not in the campaign, answers 422 governance.invalid (field path only) or 404 governance.not_found; the reviewer is always the authenticated user",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: a_closed_campaign_takes_no_more_decisions",
            SECTION,
            "governance_review_item_decide after governance_review_close answers 409 governance.review_closed",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: a_cursor_of_another_user_is_refused",
            SECTION,
            "a list cursor issued to one user and presented by another (governance_review_list, governance_emergency_list, governance_grant_request_list) answers 422 governance.invalid with details.fields naming `cursor` and returns no row",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "governance_attacks: every_governance_operation_answers_a_cross_tenant_id_like_a_missing_one",
            SECTION,
            "each governance operation that takes an id answers 404 governance.not_found for an id of another tenant, byte-identical to one that never existed",
            OWNER,
            NO_TENANT,
        ),
        CaseDef::blocked(
            "governance_attacks: every_refusal_is_audited_and_names_no_secret_or_other_user",
            SECTION,
            "each refusal above writes an audit record with who, what and the outcome, and no error body, log line or audit record carries a token, a reason text or another user's data",
            OWNER,
            "needs the routes and the audit chain read back",
        ),
        CaseDef::blocked(
            "governance_attacks: a_consumer_audience_session_reaches_no_governance_operation",
            SECTION,
            "a consumer-audience session on any governance operation is refused (ADR-083: staff only) with the audience refusal of the contract, never a 200",
            OWNER,
            "needs the routes and a consumer session fixture",
        ),
    ]
}
