//! [`SearchHook`]: the implementation of `spot-postgres`'s [`SearchMaintenance`] that connects
//! the document engine to this crate's projection (WP-STO-13.T3).
//!
//! `spot-postgres` cannot depend on this crate (this crate takes its `PgTransaction`), so the
//! backend defines a small trait and the host installs this implementation:
//!
//! ```text
//! let backend = SearchHook::install(PgBackend::connect(config).await?, language, protection);
//! backend.prepare(&catalog).await?;   // creates spot_search_doc and its indexes
//! // every insert, update and delete of a searchable DocType now keeps the projection, in the
//! // write's own transaction; Expr::Search runs through PgTransaction::query and read.
//! ```
//!
//! # What it does, and what it leaves alone
//!
//! - `prepare_entity` compiles the entity's `[search]` declaration into a [`ProjectionPlan`]
//!   (refusing a searchable or faceted field in a protection group), caches the plan and runs
//!   [`prepare`](crate::prepare) for it: extension check, table, indexes.
//! - `after_write` runs [`upsert`] and `after_delete` runs [`remove`], both on the transaction
//!   the backend passes, so the projection row commits or rolls back with its document.
//! - The plan of an entity is built on first use when `prepare_entity` has not run in this
//!   process (a restart that skips `prepare`), from the same declaration, so a write can never
//!   skip its projection; an entity without a `search` declaration is an `Internal` error here,
//!   because the backend never calls the hook for one.
//! - Nothing here reads a tenant or a language from a document. The tenant is the database the
//!   transaction runs on (one tenant is one database, ADR-34) and the language is the one the
//!   host passed at construction (or the declaration's own).
//!
//! Per-field language overrides (`ProjectionPlan::with_field_language`) and the other plan
//! tweaks are not reachable through the hook yet: a host that needs them builds its plans and
//! calls [`upsert`] itself, as before. That is out of this task and noted in its report.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use spot_postgres::{HookFuture, PgBackend, PgTransaction, SearchMaintenance};
use spot_storage_contract::{
    Catalog, EntityDef, ErrorKind, Name, RecordId, StorageError, StorageResult,
};

use crate::error::SearchError;
use crate::maintain::{remove, upsert};
use crate::plan::{Language, ProjectionPlan};
use crate::protect::Protection;
use crate::schema::prepare;

/// Keeps the search projection in step with the documents of a `spot-postgres` backend.
///
/// Cheap to share (the backend holds it in an `Arc`); the plans it caches are immutable.
#[derive(Debug)]
pub struct SearchHook {
    language: Language,
    protection: Protection,
    plans: RwLock<BTreeMap<Name, Arc<ProjectionPlan>>>,
}

impl SearchHook {
    /// A hook whose DocTypes default to `language` (a declaration's own `language` wins) and
    /// whose fields are grouped by `protection`.
    #[must_use]
    pub fn new(language: Language, protection: Protection) -> Self {
        Self {
            language,
            protection,
            plans: RwLock::new(BTreeMap::new()),
        }
    }

    /// Installs a hook on `backend` and returns the backend: from then on `prepare` creates the
    /// projection, writes maintain it, and search is served.
    #[must_use]
    pub fn install(backend: PgBackend, language: Language, protection: Protection) -> PgBackend {
        backend.with_search(Arc::new(Self::new(language, protection)))
    }

    /// The plan of `entity`, built once from its declaration.
    fn plan_for(&self, entity: &EntityDef) -> StorageResult<Arc<ProjectionPlan>> {
        if let Some(plan) = self.read_plans().get(&entity.name) {
            return Ok(Arc::clone(plan));
        }
        let plan = ProjectionPlan::from_entity(entity, &self.language, &self.protection)
            .map_err(storage)?
            .ok_or_else(|| {
                StorageError::new(
                    ErrorKind::Internal,
                    format!(
                        "`{}` has no search declaration, so it has no projection to maintain",
                        entity.name
                    ),
                )
                .with_entity(&entity.name)
            })?;
        let plan = Arc::new(plan);
        self.write_plans()
            .insert(entity.name.clone(), Arc::clone(&plan));
        Ok(plan)
    }

    fn read_plans(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<Name, Arc<ProjectionPlan>>> {
        // The map stays valid even if a holder panicked.
        self.plans.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_plans(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<Name, Arc<ProjectionPlan>>> {
        self.plans.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// A [`SearchError`] as the storage error the backend returns.
fn storage(error: SearchError) -> StorageError {
    match error {
        SearchError::Storage(error) => error,
        other => StorageError::new(ErrorKind::Internal, other.explanation()),
    }
}

impl SearchMaintenance for SearchHook {
    fn serves_search(&self) -> bool {
        true
    }

    fn prepare_entity<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        _catalog: &'a Catalog,
        entity: &'a EntityDef,
    ) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let plan = self.plan_for(entity)?;
            prepare(tx, std::slice::from_ref(&*plan))
                .await
                .map_err(storage)
        })
    }

    fn after_write<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        entity: &'a EntityDef,
        id: RecordId,
    ) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let plan = self.plan_for(entity)?;
            let written = upsert(tx, &plan, id).await.map_err(storage)?;
            if written == 0 {
                // The row was written in this transaction a moment ago; not finding it means the
                // projection would silently miss a document, which must fail the write.
                return Err(StorageError::new(
                    ErrorKind::Internal,
                    format!(
                        "the search projection of `{}` found no row to project for {id:?}",
                        entity.name
                    ),
                )
                .with_entity(&entity.name));
            }
            Ok(())
        })
    }

    fn after_delete<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        entity: &'a EntityDef,
        id: RecordId,
    ) -> HookFuture<'a, ()> {
        Box::pin(async move {
            remove(tx, &entity.name, id).await.map_err(storage)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{catalog, language, name};

    fn hook() -> SearchHook {
        SearchHook::new(language("en"), Protection::new())
    }

    #[test]
    fn a_plan_is_built_once_and_shared() {
        let catalog = catalog();
        let entity = catalog
            .entity(&name("listing"))
            .unwrap_or_else(|e| panic!("{e}"));
        let hook = hook();
        let first = hook.plan_for(entity).unwrap_or_else(|e| panic!("{e}"));
        let second = hook.plan_for(entity).unwrap_or_else(|e| panic!("{e}"));
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.doctype(), &name("listing"));
    }

    #[test]
    fn an_entity_without_search_has_no_plan_and_the_refusal_is_internal() {
        let mut catalog_entity = catalog()
            .entity(&name("listing"))
            .unwrap_or_else(|e| panic!("{e}"))
            .clone();
        catalog_entity.search = None;
        let error = match hook().plan_for(&catalog_entity) {
            Ok(_) => panic!("an entity without search has no plan"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::Internal);
    }

    #[test]
    fn a_protected_searchable_field_is_refused_when_the_plan_is_built() {
        let catalog = catalog();
        let entity = catalog
            .entity(&name("listing"))
            .unwrap_or_else(|e| panic!("{e}"));
        let protection = Protection::new()
            .group_field(&name("listing"), &name("title"), "owner")
            .unwrap_or_else(|e| panic!("{e}"));
        let hook = SearchHook::new(language("en"), protection);
        let error = match hook.plan_for(entity) {
            Ok(_) => panic!("a protected searchable field must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::InvalidSchema);
    }

    #[test]
    fn the_hook_serves_search() {
        assert!(hook().serves_search());
    }
}
