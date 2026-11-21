//! `spot-links`: the engine's link logic (WP-DOC-06.T1).
//!
//! Storage owns referential integrity (ADR-053): a link to a missing record, or deleting a
//! record that a link still points at, fails with `ReferenceViolation`. That is a constraint,
//! not an explanation. This crate is the layer above it:
//!
//! - [`validate_links`] pre-checks every `Link`, `DynamicLink` and table-multiselect value on a
//!   write — the target exists, its DocType is the one the field expects, a dynamic link's type
//!   may be linked at all, a cancelled target is refused where it must be, and a posted source
//!   may not point at a draft — with the exact field path, before storage is asked. Existence
//!   checks are batched: one query per target DocType.
//! - [`where_used`] answers "which documents refer to this one", grouped by DocType and field,
//!   with counts and a bounded sample, through the one-hop incoming traversal of the graph
//!   derived from Link fields.
//! - [`explain_delete`], [`explain_cancel`] and [`explain_reference_violation`] turn a refusal
//!   into the typed [`links.referenced`](codes::REFERENCED) error carrying the grouped
//!   referrers, so the UI renders a sentence and a list instead of a bare constraint name.
//! - [`check_rename_available`] keeps a rename to a name another document holds from landing.
//!
//! # The seam
//!
//! The engine calls this crate through the small [`LinkChecks`] trait: it hands over its
//! effective metadata and a **scoped** query. The caller (the engine's authorizer) applies
//! permission filtering to that query first, so nothing here sees — or counts — a document the
//! caller may not read. [`RepoChecks`] adapts the app API's [`Repository`] to the trait, so the
//! engine can use both without this crate depending on the engine.
//!
//! Rename needs no cascade: links are record ids (ADR-33), so a rename never breaks a reference.

#![forbid(unsafe_code)]

pub mod codes;
mod explain;
mod field;
mod validate;
mod where_used;

pub use explain::{
    check_rename_available, explain_cancel, explain_delete, explain_reference_violation, parent_of,
};
pub use validate::{FieldIssue, validate_links, validate_links_error};
pub use where_used::{DEFAULT_SAMPLE, ReferrerGroup, WhereUsed, where_used};

use std::sync::Arc;

use spot_app_api::context::Repository;
use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::prelude::Name;
use spot_app_api::{AppResult, BoxFuture};
use spot_meta::EffectiveMeta;
use spot_storage_contract::{Query, Rows};

/// The engine-facing seam: the data a link check needs, with permission filtering already
/// applied by the caller.
pub trait LinkChecks: Send {
    /// The effective metadata the checks run against: the DocTypes, their fields and the graph
    /// derived from them.
    fn effective_meta(&self) -> Arc<EffectiveMeta>;

    /// Runs a storage query through the caller's **scoped** repository. The caller filters reads
    /// by the invocation's permissions before the query reaches storage, so a result here is
    /// already the caller's own view and never leaks a hidden document.
    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>>;
}

/// Adapts the app API's [`Repository`] and an engine snapshot's metadata to [`LinkChecks`].
///
/// A host builds one per operation and passes it to [`validate_links`], [`where_used`] or
/// [`check_rename_available`]. The repository it wraps must be the operation's scoped one.
pub struct RepoChecks<'a, R: Repository + ?Sized> {
    meta: Arc<EffectiveMeta>,
    repo: &'a mut R,
}

impl<'a, R: Repository + ?Sized> RepoChecks<'a, R> {
    /// Wraps `repo` with the effective metadata it runs against.
    pub fn new(meta: Arc<EffectiveMeta>, repo: &'a mut R) -> Self {
        Self { meta, repo }
    }
}

impl<R: Repository + ?Sized> LinkChecks for RepoChecks<'_, R> {
    fn effective_meta(&self) -> Arc<EffectiveMeta> {
        self.meta.clone()
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        self.repo.query(query)
    }
}

/// A valid storage [`Name`], or an error. Metadata that already compiled always passes; this
/// maps the impossible case onto the catalogue's `document.invalid_name`.
pub(crate) fn name(value: &str) -> AppResult<Name> {
    Name::new(value).map_err(|e| {
        AppError::new(
            ErrorClass::Internal,
            "document.invalid_name",
            format!("`{value}` is not a valid storage name: {e}"),
        )
    })
}
