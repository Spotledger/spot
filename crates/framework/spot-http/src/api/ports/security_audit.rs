//! The audit and support-access seams of `/api/v1/security/*` (contract 0.20.0, WP-CON-03.T43;
//! WP-SEC-07.T3): the audit chain verification, masked entries and export, the compliance
//! evidence pack and controls, support-access requests, consent, revocation, the support sessions
//! and their recordings.
//!
//! Two ports, both object safe with boxed futures:
//!
//! - [`AuditPort`]: audit chain, entries, export, compliance evidence and controls.
//! - [`SupportAccessPort`]: support requests, consent, revoke, sessions and recordings.
//!
//! `spot-http` mounts the 10 operations as thin, bounded handlers; **every decision** (the
//! platform permission, tenant scope, masking, who may read an audit or a recording, the
//! verification of a customer-signed consent, the audit record of each outcome, refusals
//! included) belongs to the ports. The layer passes the validated operation, path ids, filters,
//! page request and bounded body, and the caller through [`RequestCtx`] (credential and request
//! id, the correlation id of the audit record). The approving user of a consent is the caller
//! the credential names: no body field carries one.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`AuditPort`].
pub const KEY: PortKey = PortKey::of::<dyn AuditPort>();

/// The port key of [`SupportAccessPort`] (one `KEY` per port file; the group names both in its
/// `required_ports`).
pub const SUPPORT_ACCESS_KEY: PortKey = PortKey::of::<dyn SupportAccessPort>();

/// One operation of the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    /// `security_audit_chain_verify`.
    AuditChainVerify,
    /// `security_audit_entries`.
    AuditEntries,
    /// `security_audit_export`.
    AuditExport,
    /// `security_compliance_evidence_pack`.
    ComplianceEvidencePack,
    /// `security_compliance_controls`.
    ComplianceControls,
    /// `security_support_request_list`.
    SupportRequestList,
    /// `security_support_request_consent`.
    SupportRequestConsent,
    /// `security_support_request_revoke`.
    SupportRequestRevoke,
    /// `security_support_session_list`.
    SupportSessionList,
    /// `security_support_session_recording`.
    SupportSessionRecording,
}

impl Op {
    /// Every operation, in contract order.
    pub const ALL: [Op; 10] = [
        Op::AuditChainVerify,
        Op::AuditEntries,
        Op::AuditExport,
        Op::ComplianceEvidencePack,
        Op::ComplianceControls,
        Op::SupportRequestList,
        Op::SupportRequestConsent,
        Op::SupportRequestRevoke,
        Op::SupportSessionList,
        Op::SupportSessionRecording,
    ];

    /// The contract `operationId`.
    pub fn id(self) -> &'static str {
        match self {
            Op::AuditChainVerify => "security_audit_chain_verify",
            Op::AuditEntries => "security_audit_entries",
            Op::AuditExport => "security_audit_export",
            Op::ComplianceEvidencePack => "security_compliance_evidence_pack",
            Op::ComplianceControls => "security_compliance_controls",
            Op::SupportRequestList => "security_support_request_list",
            Op::SupportRequestConsent => "security_support_request_consent",
            Op::SupportRequestRevoke => "security_support_request_revoke",
            Op::SupportSessionList => "security_support_session_list",
            Op::SupportSessionRecording => "security_support_session_recording",
        }
    }

    /// Whether the support-access port (not the audit port) answers the operation.
    pub fn is_support_access(self) -> bool {
        matches!(
            self,
            Op::SupportRequestList
                | Op::SupportRequestConsent
                | Op::SupportRequestRevoke
                | Op::SupportSessionList
                | Op::SupportSessionRecording
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
pub struct AuditCall {
    /// The operation.
    pub op: Op,
    /// The path ids in path order (`id`); 1 to 200 visible ASCII characters.
    pub path: Vec<String>,
    /// The validated filters (`from`, `to`, `actor`).
    pub query: BTreeMap<String, String>,
    /// The page request of a listing (audit entries, recordings and the other listings).
    pub page: Option<PageRequest>,
    /// The bounded body, `additionalProperties: false` already enforced. A consent carries
    /// `consent` (at most 8192 characters) and `signature` (base64, at most 1024 characters, the
    /// layer has checked the alphabet and padding; the **port verifies** it).
    pub body: Option<Map<String, Json>>,
    /// The `If-Match` value for an operation that requires it (none of today's: reserved).
    pub if_match: Option<String>,
    /// The `Idempotency-Key` for an operation that requires it (the evidence pack).
    pub idempotency_key: Option<String>,
}

/// A successful answer.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditReply {
    /// The `data` member (`None` for an export, which has none).
    pub data: Option<Json>,
    /// `Some(None)` for the last page, `Some(Some(cursor))` for another, `None` when the
    /// operation is not a listing.
    pub next_cursor: Option<Option<String>>,
    /// The export's lines (`security_audit_export` only): one masked JSON entry per line, at
    /// most 1 MiB. The layer fixes the content type and the file name.
    pub export: Option<String>,
}

/// The audit operations.
pub trait AuditPort: Send + Sync + 'static {
    /// Whether the caller's session carries a recent step-up marker (ADR-070 section 3). Asked
    /// before an operation that needs one (the export); `false` is
    /// `403 auth.step_up_required` and the operation is not called. An error is a refusal.
    fn step_up_current<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>>;

    /// One audit or compliance operation. The port checks the platform permission (fail
    /// closed), scopes to the caller's tenant, masks every entry (no protected value, no secret,
    /// no document content), and audits every outcome (the export and the verification are
    /// themselves audited) with `ctx.request_id` as the correlation id.
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a AuditCall,
    ) -> BoxFuture<'a, Result<AuditReply, ApiError>>;
}

/// The support-access operations.
pub trait SupportAccessPort: Send + Sync + 'static {
    /// As [`AuditPort::step_up_current`] (consent, revoke and recordings need one).
    fn step_up_current<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>>;

    /// One support-access operation. A cross-tenant id answers `404 security.not_found` exactly
    /// like a missing one; a consent is verified here (signature and match with the request).
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a AuditCall,
    ) -> BoxFuture<'a, Result<AuditReply, ApiError>>;
}
