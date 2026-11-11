//! The PostgreSQL [`SchemaBackend`] (feature `postgres`): the online migration engine of
//! `spot-pg-migrate` behind the pipeline's plan and apply steps.
//!
//! Planning diffs the live schema against the catalogue and classifies every step (additive,
//! needs backfill) or refuses it; applying takes the engine's session advisory lock, re-plans
//! under it so a rerun after a crash does only what is left, runs each step in a short
//! transaction with lock and statement timeouts, and checks the schema matches afterwards.
//! This crate holds no lock of its own across any of that.

use std::sync::Arc;

use spot_pg_migrate::{ApplyOptions, LiveSchema, MigrationPlan, PlanOptions, Risk, apply, dry_run};
use spot_postgres::PgBackend;
use spot_storage_contract::Catalog;

use crate::schema::{
    Fill, PlanStep, RefusedStep, SchemaBackend, SchemaFuture, SchemaPlan, StepClass,
    backfill_documents,
};

/// A [`SchemaBackend`] over a PostgreSQL backend.
pub struct PgSchema {
    backend: Arc<PgBackend>,
    options: PlanOptions,
    apply: ApplyOptions,
}

impl PgSchema {
    /// A schema over `backend` with default plan and apply options.
    #[must_use]
    pub fn new(backend: Arc<PgBackend>) -> Self {
        Self {
            backend,
            options: PlanOptions::default(),
            apply: ApplyOptions::default(),
        }
    }

    /// Fills new required columns on populated tables with the values in `options`.
    #[must_use]
    pub fn with_plan_options(mut self, options: PlanOptions) -> Self {
        self.options = options;
        self
    }

    /// Overrides the apply knobs (timeouts, batch size).
    #[must_use]
    pub fn with_apply_options(mut self, apply: ApplyOptions) -> Self {
        self.apply = apply;
        self
    }
}

fn to_plan(plan: &MigrationPlan) -> SchemaPlan {
    SchemaPlan {
        steps: plan
            .steps
            .iter()
            .map(|s| PlanStep {
                key: s.key.clone(),
                class: match s.risk {
                    Risk::Additive => StepClass::Additive,
                    Risk::NeedsBackfill => StepClass::NeedsBackfill,
                    Risk::NeedsValidation => StepClass::NeedsValidation,
                },
                sentence: s.sentence.clone(),
            })
            .collect(),
        refused: plan
            .refused
            .iter()
            .map(|r| RefusedStep {
                entity: r.entity.clone(),
                field: r.field.clone(),
                reason: r.reason.clone(),
            })
            .collect(),
    }
}

impl SchemaBackend for PgSchema {
    fn plan<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, SchemaPlan> {
        Box::pin(async move {
            let plan = dry_run(&self.backend, wanted, &self.options)
                .await
                .map_err(|e| e.to_string())?;
            Ok(to_plan(&plan))
        })
    }

    fn apply<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, ()> {
        Box::pin(async move {
            let plan = dry_run(&self.backend, wanted, &self.options)
                .await
                .map_err(|e| e.to_string())?;
            apply(&self.backend, &plan, self.apply.clone())
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn has_rows<'a>(&'a self, doctype: &'a str) -> SchemaFuture<'a, bool> {
        Box::pin(async move {
            let live = LiveSchema::read(&self.backend)
                .await
                .map_err(|e| e.to_string())?;
            Ok(live.tables.get(doctype).is_some_and(|t| t.has_rows))
        })
    }

    fn backfill<'a>(&'a self, wanted: &'a Catalog, fills: &'a [Fill]) -> SchemaFuture<'a, u64> {
        Box::pin(async move {
            let catalog = Arc::new(wanted.clone());
            let mut total = 0;
            for fill in fills {
                total += backfill_documents(&*self.backend, Arc::clone(&catalog), fill).await?;
            }
            Ok(total)
        })
    }
}

impl std::fmt::Debug for PgSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSchema").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_pg_migrate::{Run, Step};

    #[test]
    fn a_validating_step_is_never_classed_as_additive() {
        let wanted = match spot_storage_contract::Collation::new("en")
            .and_then(|c| Catalog::new(1, c, Vec::new()))
        {
            Ok(catalog) => catalog,
            Err(error) => panic!("{error}"),
        };
        let mut migration = spot_pg_migrate::plan(&LiveSchema::default(), &wanted);
        migration.steps = vec![Step {
            key: "add_exclusion:party.no_overlap".to_owned(),
            risk: Risk::NeedsValidation,
            sentence: "A step.".to_owned(),
            run: Run::Outside("exclusion statement".to_owned()),
        }];
        let plan = to_plan(&migration);
        assert_eq!(plan.steps[0].class, StepClass::NeedsValidation);
    }
}
