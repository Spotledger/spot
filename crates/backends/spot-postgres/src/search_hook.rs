//! The seam between the document engine and the search projection (ADR-087 section 7;
//! WP-STO-13.T3).
//!
//! `spot-pg-search` depends on this crate (it takes a [`PgTransaction`]), so this crate cannot
//! depend on it. The connection is therefore a **hook**: a small trait defined here, implemented
//! by `spot_pg_search::SearchHook`, and installed on a backend with
//! [`PgBackend::with_search`](crate::PgBackend::with_search).
//!
//! # What the backend does with it
//!
//! - **`prepare`** calls [`SearchMaintenance::prepare_entity`] for every entity that has a
//!   `search` declaration, in one transaction, before any other change, so a missing extension
//!   refuses the whole `prepare`. Without a hook such a catalog is still refused, as
//!   `Unsupported`.
//! - **Every document write** (`insert`, `update`) of an entity with a `search` declaration calls
//!   [`SearchMaintenance::after_write`] and every `delete` calls
//!   [`SearchMaintenance::after_delete`], on the same transaction, after the row write and
//!   before commit. A failing hook fails the write; it is never swallowed. The transaction is
//!   left aborted (a recoverable kind) or poisoned (any other), exactly as a failed statement of
//!   the write would leave it, so a caller that rolls back to its savepoint undoes the row write
//!   too.
//! - **Reads** (`query` and `read`) are not refused for `Expr::Search` and search rank when a
//!   hook that [`serves_search`](SearchMaintenance::serves_search) is installed, and
//!   `Capabilities::search` reports the same answer.
//!
//! A write of an entity without a `search` declaration does nothing extra: the decision is the
//! catalog's `search.is_some()`, a field check, never a parse.
//!
//! # Writes are counted
//!
//! The hook runs its statements through the transaction's raw-SQL seam, so each projection write
//! counts one against `TxOptions::max_writes` (ADR-45): a searchable document costs one more
//! write than a plain one.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_storage_contract::{Catalog, EntityDef, RecordId, StorageResult};

use crate::PgTransaction;

/// What a hook method returns: a boxed future, so the trait can be used as `dyn`.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = StorageResult<T>> + Send + 'a>>;

/// Keeps a search projection in step with the documents, inside the write's own transaction.
///
/// Every method receives the open [`PgTransaction`] and must do all its work on it: it must not
/// take a second connection, because the projection has to commit or roll back with the document.
pub trait SearchMaintenance: Send + Sync {
    /// Whether this hook answers searches. `Capabilities::search` and the refusal of
    /// `Expr::Search` follow it.
    fn serves_search(&self) -> bool;

    /// Creates what `entity`'s projection needs (table, indexes) and checks its extensions.
    /// Additive and idempotent. Called by `prepare` for each entity with a `search` declaration.
    fn prepare_entity<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        catalog: &'a Catalog,
        entity: &'a EntityDef,
    ) -> HookFuture<'a, ()>;

    /// Writes (or rewrites) the projection row of `id`, after the document was inserted or
    /// updated in `tx`.
    fn after_write<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        entity: &'a EntityDef,
        id: RecordId,
    ) -> HookFuture<'a, ()>;

    /// Removes the projection row of `id`, after the document was deleted in `tx`.
    fn after_delete<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        entity: &'a EntityDef,
        id: RecordId,
    ) -> HookFuture<'a, ()>;
}

/// A shared hook, as the backend and its transactions hold it.
pub(crate) type SharedHook = Arc<dyn SearchMaintenance>;

impl PgTransaction {
    /// The hook to call for a write of `entity`: `None` when no hook is installed or the entity
    /// has no `search` declaration (the common case costs one field check).
    pub(crate) fn search_hook_for(&self, entity: &EntityDef) -> Option<SharedHook> {
        if entity.search.is_some() {
            self.search.clone()
        } else {
            None
        }
    }

    /// Whether this transaction may run `Expr::Search`.
    pub(crate) fn serves_search(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|hook| hook.serves_search())
    }
}

/// Calls the hook after a row write of `entity`, if there is one to call.
///
/// A failure is recorded on the transaction as a failed write (aborted or poisoned), so nothing
/// further is sent until the caller rolls back, and the error is returned unchanged.
pub(crate) async fn after_write(
    tx: &mut PgTransaction,
    entity: &EntityDef,
    id: RecordId,
) -> StorageResult<()> {
    let Some(hook) = tx.search_hook_for(entity) else {
        return Ok(());
    };
    match hook.after_write(tx, entity, id).await {
        Ok(()) => Ok(()),
        Err(error) => Err(tx.fail_write(error)),
    }
}

/// Calls the hook after a delete of `entity`; see [`after_write`].
pub(crate) async fn after_delete(
    tx: &mut PgTransaction,
    entity: &EntityDef,
    id: RecordId,
) -> StorageResult<()> {
    let Some(hook) = tx.search_hook_for(entity) else {
        return Ok(());
    };
    match hook.after_delete(tx, entity, id).await {
        Ok(()) => Ok(()),
        Err(error) => Err(tx.fail_write(error)),
    }
}
