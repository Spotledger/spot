//! The SCIM 2.0 server core (WP-IAM-06.T1; ADR-070 sections 7.2 and 7.3).
//!
//! SCIM is how a customer's identity provider provisions and deprovisions people. This crate
//! serves RFC 7643 resources and RFC 7644 requests for `/Users` and `/Groups`, maps IdP groups
//! to roles through an explicit table, and makes deactivation (`active=false`) end sessions,
//! tokens, keys and invitations at once. It is a **sans-I/O core**: nothing here reads storage,
//! the clock or any ambient state, and nothing performs I/O. Every effect goes through
//! [`ScimStore`], which the platform fills with real storage and the event outbox.
//!
//! | Area | Where |
//! | --- | --- |
//! | Resources and their mapping to directory users | [`ScimUser`], [`ScimGroup`], [`StoredUser`] |
//! | Filters, pagination, ETags | [`Filter`], [`handle`] |
//! | PATCH | [`PatchRequest`], [`PatchOperation`] |
//! | Group-to-role mapping table | [`GroupMapping`], [`MappingStore`] |
//! | Deactivation as `active=false` | [`LeaverOutcome`], [`apply_leaver_actions`] |
//! | The request entry point | [`handle`], [`ScimRequest`], [`ScimResponse`], [`ScimContext`] |
//! | The HTTP adapter (`feature = "http"`) | [`http`] |
//!
//! # The rules this crate keeps
//!
//! - **Deactivation, never deletion.** `PATCH active=false`, a `PUT` that clears `active`, and
//!   `DELETE /Users/{id}` all run the directory's leaver path; no user record is ever removed.
//! - **A group edit changes no role.** The mapping table decides assignments with source `idp`
//!   through `spot-iam-assign`; the role records are read to validate a mapping and never
//!   written.
//! - **No tenant leakage.** Every store method is scoped by the credential's tenant.
//! - **No secret on the wire.** Events are validated against the identity contract, whose
//!   `identity::validate` refuses a payload carrying any key that looks like a credential.

#![forbid(unsafe_code)]

pub mod error;
pub mod filter;
#[cfg(feature = "http")]
pub mod http;
pub mod mapping;
pub mod patch;
pub mod request;
pub mod resource;
pub mod store;
pub(crate) mod wire;

/// A tenant: the unit of residency and of every SCIM credential (ADR-058, ADR-070).
pub type TenantId = String;

pub use error::{ScimError, ScimErrorBody, ScimResult, codes};
pub use filter::{AttrPath, CompareOp, Filter, ScimValue, Target};
pub use mapping::{
    GroupAdminRow, GroupMapping, MappingStore, build_group_role_map, group_admin_view,
    validate_mapping,
};
pub use patch::{PatchOpKind, PatchOperation, PatchRequest};
pub use request::{
    Credential, LeaverOutcome, Limits, Method, ScimContext, ScimRequest, ScimResponse,
    apply_leaver_actions, handle,
};
pub use resource::{
    ENTERPRISE_USER_SCHEMA, Email, EnterpriseUser, GROUP_SCHEMA, LIST_SCHEMA, Manager, Member,
    Meta, Name, PATCH_SCHEMA, SCIM_BASE, SCIM_CONTENT_TYPE, ScimGroup, ScimProfile, ScimUser,
    StoredGroup, StoredUser, USER_SCHEMA, etag, parse_etag, parse_record_id,
};
pub use store::{AuditEntry, MemoryStore, ScimEvent, ScimStore};
