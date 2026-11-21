//! The [`DefinitionStore`] port.

use std::future::Future;
use std::pin::Pin;

use spot_app_api::prelude::OffsetDateTime;

use crate::error::Result;
use crate::types::StoredVersion;

/// A boxed, `Send` future: the return type of every [`DefinitionStore`] method, so the trait
/// stays object-safe, like `CaseStore`.
pub type DefFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Durable storage of published process versions.
///
/// A store serves one tenant, fixed when it is built; no method takes a tenant, so a caller can
/// never name another tenant's rows. A stored version is immutable: there is no method that
/// changes its text or hash, and publishing the same text again is a new version only because the
/// caller asked for one. A backend failure is an error, never an empty result.
pub trait DefinitionStore: Send + Sync {
    /// Publishes `text` as version `latest + 1` of `name`.
    ///
    /// `expected_previous` is the latest version the caller saw (`None` when it saw none). The
    /// check and the assignment of the number are one atomic step, so two concurrent publishes
    /// with the same `expected_previous` cannot both win: one gets
    /// [`DefinitionError::VersionConflict`](crate::DefinitionError::VersionConflict). A refused
    /// publish leaves nothing behind. The text's own `version` and `status` fields are ignored:
    /// the store assigns the number and the state is `Published`.
    fn publish<'a>(
        &'a self,
        name: &'a str,
        text: &'a str,
        expected_previous: Option<u32>,
        publisher: &'a str,
        note: Option<&'a str>,
        now: OffsetDateTime,
    ) -> DefFuture<'a, StoredVersion>;

    /// The version, when it exists (published or retired).
    fn get<'a>(&'a self, name: &'a str, version: u32) -> DefFuture<'a, Option<StoredVersion>>;

    /// The highest version still in the `Published` state, when there is one.
    fn latest_published<'a>(&'a self, name: &'a str) -> DefFuture<'a, Option<StoredVersion>>;

    /// Versions of `name` in ascending order, those above `after` (`None`: from the start), at
    /// most `limit` (capped at [`MAX_PAGE`](crate::MAX_PAGE)).
    fn list<'a>(
        &'a self,
        name: &'a str,
        after: Option<u32>,
        limit: usize,
    ) -> DefFuture<'a, Vec<StoredVersion>>;

    /// Every process name the tenant has a version of, sorted.
    fn names<'a>(&'a self) -> DefFuture<'a, Vec<String>>;

    /// Retires a version: it is no longer started, and stays readable for Runs pinned to it.
    /// Retiring a retired version returns it unchanged (the first `by` and `now` are kept).
    /// The store does not check the pins: the caller consults [`Self::running_pins`] first.
    fn retire<'a>(
        &'a self,
        name: &'a str,
        version: u32,
        by: &'a str,
        now: OffsetDateTime,
    ) -> DefFuture<'a, StoredVersion>;

    /// The versions of `name` that running Cases are pinned to, ascending and distinct.
    fn running_pins<'a>(&'a self, name: &'a str) -> DefFuture<'a, Vec<u32>>;
}
