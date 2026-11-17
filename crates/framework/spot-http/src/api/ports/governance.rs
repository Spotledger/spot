//! The access-governance seam of `/api/v1/governance/*` (contract 0.19.0, WP-CON-03.T42;
//! WP-IAM-08.T8).
//!
//! `spot-http` mounts the 22 operations as thin, bounded handlers; **every decision** (the
//! platform permission, tenant scope, self-approval, segregation of duties, reviewer rights,
//! the audit record of each outcome, refusals included) belongs to the port. The layer passes
//! the port only what it validated: the operation, the path ids, the filters, a page request, a
//! bounded body, the `If-Match` and `Idempotency-Key` headers, and the caller through
//! [`RequestCtx`] (the credential and the request id, which is the correlation id of the audit
//! record). It passes **no approver, reviewer, actor or tenant**: a body that names one is
//! refused as an unknown field, and the port derives the actor from the credential.
//!
//! The port is split by [`Area`] (grants, SoD, reviews, questions, emergency): an area the host
//! has not wired answers `501 service.not_configured` through [`GovernancePort::serves`], and
//! the other areas keep working. One generic [`GovernancePort::call`] carries every operation so
//! the surface stays small and each operation is exercised the same way in tests.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`GovernancePort`].
pub const KEY: PortKey = PortKey::of::<dyn GovernancePort>();

/// A part of the governance surface a host may leave unwired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Area {
    /// Grant requests and their decisions.
    Grants,
    /// Segregation-of-duties rules, analysis and overrides.
    Sod,
    /// Access review campaigns.
    Reviews,
    /// The four access questions.
    Questions,
    /// Emergency access.
    Emergency,
}

/// One operation of the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    /// `governance_grant_request_list`.
    GrantRequestList,
    /// `governance_grant_request_create`.
    GrantRequestCreate,
    /// `governance_grant_request_decide`.
    GrantRequestDecide,
    /// `governance_sod_rules_get`.
    SodRulesGet,
    /// `governance_sod_rules_put`.
    SodRulesPut,
    /// `governance_sod_analysis`.
    SodAnalysis,
    /// `governance_sod_override_list`.
    SodOverrideList,
    /// `governance_sod_override_create`.
    SodOverrideCreate,
    /// `governance_review_list`.
    ReviewList,
    /// `governance_review_create`.
    ReviewCreate,
    /// `governance_review_get`.
    ReviewGet,
    /// `governance_review_items`.
    ReviewItems,
    /// `governance_review_item_decide`.
    ReviewItemDecide,
    /// `governance_review_close`.
    ReviewClose,
    /// `governance_review_report`.
    ReviewReport,
    /// `governance_question_user_permissions`.
    QuestionUserPermissions,
    /// `governance_question_who_can`.
    QuestionWhoCan,
    /// `governance_question_who_can_see`.
    QuestionWhoCanSee,
    /// `governance_question_changes`.
    QuestionChanges,
    /// `governance_emergency_create`.
    EmergencyCreate,
    /// `governance_emergency_list`.
    EmergencyList,
    /// `governance_emergency_review`.
    EmergencyReview,
}

impl Op {
    /// Every operation, in contract order.
    pub const ALL: [Op; 22] = [
        Op::GrantRequestList,
        Op::GrantRequestCreate,
        Op::GrantRequestDecide,
        Op::SodRulesGet,
        Op::SodRulesPut,
        Op::SodAnalysis,
        Op::SodOverrideList,
        Op::SodOverrideCreate,
        Op::ReviewList,
        Op::ReviewCreate,
        Op::ReviewGet,
        Op::ReviewItems,
        Op::ReviewItemDecide,
        Op::ReviewClose,
        Op::ReviewReport,
        Op::QuestionUserPermissions,
        Op::QuestionWhoCan,
        Op::QuestionWhoCanSee,
        Op::QuestionChanges,
        Op::EmergencyCreate,
        Op::EmergencyList,
        Op::EmergencyReview,
    ];

    /// The contract `operationId`.
    pub fn id(self) -> &'static str {
        match self {
            Op::GrantRequestList => "governance_grant_request_list",
            Op::GrantRequestCreate => "governance_grant_request_create",
            Op::GrantRequestDecide => "governance_grant_request_decide",
            Op::SodRulesGet => "governance_sod_rules_get",
            Op::SodRulesPut => "governance_sod_rules_put",
            Op::SodAnalysis => "governance_sod_analysis",
            Op::SodOverrideList => "governance_sod_override_list",
            Op::SodOverrideCreate => "governance_sod_override_create",
            Op::ReviewList => "governance_review_list",
            Op::ReviewCreate => "governance_review_create",
            Op::ReviewGet => "governance_review_get",
            Op::ReviewItems => "governance_review_items",
            Op::ReviewItemDecide => "governance_review_item_decide",
            Op::ReviewClose => "governance_review_close",
            Op::ReviewReport => "governance_review_report",
            Op::QuestionUserPermissions => "governance_question_user_permissions",
            Op::QuestionWhoCan => "governance_question_who_can",
            Op::QuestionWhoCanSee => "governance_question_who_can_see",
            Op::QuestionChanges => "governance_question_changes",
            Op::EmergencyCreate => "governance_emergency_create",
            Op::EmergencyList => "governance_emergency_list",
            Op::EmergencyReview => "governance_emergency_review",
        }
    }

    /// The area the operation belongs to.
    pub fn area(self) -> Area {
        match self {
            Op::GrantRequestList | Op::GrantRequestCreate | Op::GrantRequestDecide => Area::Grants,
            Op::SodRulesGet
            | Op::SodRulesPut
            | Op::SodAnalysis
            | Op::SodOverrideList
            | Op::SodOverrideCreate => Area::Sod,
            Op::ReviewList
            | Op::ReviewCreate
            | Op::ReviewGet
            | Op::ReviewItems
            | Op::ReviewItemDecide
            | Op::ReviewClose
            | Op::ReviewReport => Area::Reviews,
            Op::QuestionUserPermissions
            | Op::QuestionWhoCan
            | Op::QuestionWhoCanSee
            | Op::QuestionChanges => Area::Questions,
            Op::EmergencyCreate | Op::EmergencyList | Op::EmergencyReview => Area::Emergency,
        }
    }
}

/// A page request: at most 100 rows, with the opaque cursor of the previous page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    /// The `next_cursor` of the previous page (1 to 2048 characters), if any.
    pub cursor: Option<String>,
    /// The page size, 1 to 100.
    pub limit: u32,
    /// A digest (lower-case hex SHA-256) of the operation, the path ids and the filters. The
    /// port **must** bind every cursor it issues to the caller **and** this digest: a cursor
    /// replayed by another user, on another operation or with other filters answers exactly
    /// like a malformed one (`422 governance.invalid`), so a cursor is never an oracle.
    pub query_digest: String,
}

/// What the layer asks of the port: one validated operation.
#[derive(Clone, Debug, PartialEq)]
pub struct GovernanceCall {
    /// The operation.
    pub op: Op,
    /// The path ids in path order (`id`, then `item`); each 1 to 200 visible ASCII characters.
    pub path: Vec<String>,
    /// The validated filters by name (`status`, `rule`, `user`, `permission`, `scope`,
    /// `doctype`, `id`, `from`, `to`); cursor and limit are in [`GovernanceCall::page`].
    pub query: BTreeMap<String, String>,
    /// The page request of a listing.
    pub page: Option<PageRequest>,
    /// The bounded body (`additionalProperties: false` already enforced); text such as a reason
    /// or a comment is passed through unchanged for the port to store.
    pub body: Option<Map<String, Json>>,
    /// The `If-Match` value, quoted, for an operation that requires it.
    pub if_match: Option<String>,
    /// The `Idempotency-Key`, 1 to 128 visible ASCII characters, for an operation that requires
    /// it.
    pub idempotency_key: Option<String>,
}

/// A successful answer: the `data` member, and for a listing the `next_cursor` member.
#[derive(Clone, Debug, PartialEq)]
pub struct GovernanceReply {
    /// The `data` member.
    pub data: Json,
    /// `Some(None)` for the last page (`null`), `Some(Some(cursor))` for another, `None` when
    /// the operation is not a listing.
    pub next_cursor: Option<Option<String>>,
    /// The version for the `ETag` header (the SoD rule set), quoted, at most 40 characters.
    pub etag: Option<String>,
}

/// The governance operations.
pub trait GovernancePort: Send + Sync + 'static {
    /// Whether the host wired `area`. An unwired area answers `501 service.not_configured`.
    fn serves(&self, area: Area) -> bool;

    /// Whether the caller's session carries a recent step-up marker (ADR-070 section 3).
    /// Asked by the layer **before** an operation that needs one; `false` is
    /// `403 auth.step_up_required` and the operation is not called. An error is a refusal.
    fn step_up_current<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>>;

    /// One operation. The port answers for the caller `ctx.credential` names: it checks the
    /// platform permission (fail closed), scopes to the caller's tenant (another tenant's id is
    /// `404 governance.not_found`), refuses self-approval, and writes the audit record of every
    /// outcome with `ctx.request_id` as the correlation id.
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a GovernanceCall,
    ) -> BoxFuture<'a, Result<GovernanceReply, ApiError>>;
}
