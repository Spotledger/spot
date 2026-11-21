//! The data-policy and protected-group seams of `/api/v1/security/*` (contract 0.20.0,
//! WP-CON-03.T43; WP-SEC-05.T6): the data residency and retention policy, retention runs,
//! legal holds, destruction proofs, the residency check, the protected field groups, their sync
//! classes and the node keys.
//!
//! Two ports, both object safe with boxed futures:
//!
//! - [`DataPolicyPort`]: policy, retention runs, holds, proofs and the residency check.
//! - [`ProtectedGroupPort`]: protected groups, sync classes, node keys and their revocation.
//!
//! `spot-http` mounts the 13 operations as thin, bounded handlers; **every decision** (the
//! platform permission, tenant scope, the digest check of a real retention run, the key
//! rotation, the audit record of each outcome, refusals included) belongs to the ports. The
//! layer passes the validated operation, path ids, filters, page request, bounded body and
//! headers, and the caller through [`RequestCtx`] (credential and request id, the correlation id
//! of the audit record). It never passes an actor or a tenant. **No key material crosses these
//! seams**: a port answers names, versions and public fingerprints only, and the layer refuses
//! (`500 http.internal`) an answer that carries a field named like key material.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`DataPolicyPort`].
pub const KEY: PortKey = PortKey::of::<dyn DataPolicyPort>();

/// The port key of [`ProtectedGroupPort`] (one `KEY` per port file; the group names both in its
/// `required_ports`).
pub const PROTECTED_GROUP_KEY: PortKey = PortKey::of::<dyn ProtectedGroupPort>();

/// One operation of the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    /// `security_data_policy_get`.
    DataPolicyGet,
    /// `security_data_policy_put`.
    DataPolicyPut,
    /// `security_retention_run_list`.
    RetentionRunList,
    /// `security_retention_run_create`.
    RetentionRunCreate,
    /// `security_retention_hold_list`.
    RetentionHoldList,
    /// `security_retention_hold_create`.
    RetentionHoldCreate,
    /// `security_retention_hold_delete`.
    RetentionHoldDelete,
    /// `security_retention_proof_list`.
    RetentionProofList,
    /// `security_residency_check`.
    ResidencyCheck,
    /// `security_protected_group_list`.
    ProtectedGroupList,
    /// `security_protected_group_sync_class`.
    ProtectedGroupSyncClass,
    /// `security_node_key_list`.
    NodeKeyList,
    /// `security_node_key_revoke`.
    NodeKeyRevoke,
}

impl Op {
    /// Every operation, in contract order.
    pub const ALL: [Op; 13] = [
        Op::DataPolicyGet,
        Op::DataPolicyPut,
        Op::RetentionRunList,
        Op::RetentionRunCreate,
        Op::RetentionHoldList,
        Op::RetentionHoldCreate,
        Op::RetentionHoldDelete,
        Op::RetentionProofList,
        Op::ResidencyCheck,
        Op::ProtectedGroupList,
        Op::ProtectedGroupSyncClass,
        Op::NodeKeyList,
        Op::NodeKeyRevoke,
    ];

    /// The contract `operationId`.
    pub fn id(self) -> &'static str {
        match self {
            Op::DataPolicyGet => "security_data_policy_get",
            Op::DataPolicyPut => "security_data_policy_put",
            Op::RetentionRunList => "security_retention_run_list",
            Op::RetentionRunCreate => "security_retention_run_create",
            Op::RetentionHoldList => "security_retention_hold_list",
            Op::RetentionHoldCreate => "security_retention_hold_create",
            Op::RetentionHoldDelete => "security_retention_hold_delete",
            Op::RetentionProofList => "security_retention_proof_list",
            Op::ResidencyCheck => "security_residency_check",
            Op::ProtectedGroupList => "security_protected_group_list",
            Op::ProtectedGroupSyncClass => "security_protected_group_sync_class",
            Op::NodeKeyList => "security_node_key_list",
            Op::NodeKeyRevoke => "security_node_key_revoke",
        }
    }

    /// Whether the protected-group port (not the data-policy port) answers the operation.
    pub fn is_protected_group(self) -> bool {
        matches!(
            self,
            Op::ProtectedGroupList
                | Op::ProtectedGroupSyncClass
                | Op::NodeKeyList
                | Op::NodeKeyRevoke
        )
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
    /// port **must** bind every cursor it issues to the caller and this digest: a cursor
    /// replayed by another user, on another operation or with other filters answers like a
    /// malformed one.
    pub query_digest: String,
}

/// What the layer asks of a port: one validated operation.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyCall {
    /// The operation.
    pub op: Op,
    /// The path ids in path order (`id`, `group` or `node`); each 1 to 200 visible ASCII.
    pub path: Vec<String>,
    /// The validated filters (`run`).
    pub query: BTreeMap<String, String>,
    /// The page request of a listing.
    pub page: Option<PageRequest>,
    /// The bounded body, `additionalProperties: false` already enforced. For
    /// `security_retention_run_create` it is `{"dry_run": true}` or
    /// `{"dry_run": false, "plan_digest": "<64 hex>"}`: a real run always carries its digest.
    pub body: Option<Map<String, Json>>,
    /// The `If-Match` value, quoted, for an operation that requires it.
    pub if_match: Option<String>,
    /// The `Idempotency-Key` for an operation that requires it. The revoke and sync-class
    /// operations take none: they are never replayed and never cached.
    pub idempotency_key: Option<String>,
}

/// A successful answer.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyReply {
    /// The `data` member (`None` for a `204`).
    pub data: Option<Json>,
    /// `Some(None)` for the last page, `Some(Some(cursor))` for another, `None` when the
    /// operation is not a listing.
    pub next_cursor: Option<Option<String>>,
    /// The version for the `ETag` header (the policy), quoted, at most 40 characters.
    pub etag: Option<String>,
}

/// The data-policy operations.
pub trait DataPolicyPort: Send + Sync + 'static {
    /// Whether the caller's session carries a recent step-up marker (ADR-070 section 3). Asked
    /// before every write; `false` is `403 auth.step_up_required` and the operation is not
    /// called. An error is a refusal.
    fn step_up_current<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>>;

    /// One operation of [`Op::DataPolicyGet`] to [`Op::ResidencyCheck`]. The port answers for the
    /// caller `ctx.credential` names: it checks the platform permission (fail closed), scopes
    /// to the caller's tenant (another tenant's id is `404 security.not_found`), checks a real
    /// run's `plan_digest` against the current plan, and audits every outcome with
    /// `ctx.request_id` as the correlation id.
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a PolicyCall,
    ) -> BoxFuture<'a, Result<PolicyReply, ApiError>>;
}

/// The protected-group operations: no key material, wrapped key or signature seed is ever
/// accepted from or answered to the layer.
pub trait ProtectedGroupPort: Send + Sync + 'static {
    /// As [`DataPolicyPort::step_up_current`].
    fn step_up_current<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>>;

    /// One operation of [`Op::ProtectedGroupList`] to [`Op::NodeKeyRevoke`].
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a PolicyCall,
    ) -> BoxFuture<'a, Result<PolicyReply, ApiError>>;
}
