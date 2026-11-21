//! The verify gate over `spot-pg-migrate`'s own re-plan.
//!
//! ADR-22 admits users only after verification passes. For the schema part of a release, the
//! honest check is the planner's: re-read the live schema, re-plan against the approved
//! catalogue, and require that **no step is left and nothing is refused**. That is exactly
//! what [`spot_pg_migrate::apply`](spot_pg_migrate::apply) checks at the end of a run
//! (`migrate.drift`), and what [`PgDriftVerifier`] runs as the engine's [`VerifyPort`].
//!
//! The metadata, patch and behaviour parts of the plan are re-diffed by
//! [`spot_migration_plan::verify_applied`] against an [`AppliedState`] the host supplies; this
//! crate's verifier takes those mismatches from the same seam so a host can compose the two.

use std::sync::Arc;

use spot_migration_apply::verify::{AppliedState, VerifyOutcome, VerifyPort};
use spot_migration_apply::{BoxFuture, Result as ApplyResult};
use spot_migration_plan::ReleasePlan;
use spot_pg_migrate::{LiveSchema, PlanOptions, plan_with};
use spot_postgres::PgBackend;
use spot_storage_contract::Catalog;

use crate::error::Error;

/// The schema half of verification: the live schema must need no further step.
pub struct PgDriftVerifier {
    backend: PgBackend,
    catalog: Catalog,
    plan_options: PlanOptions,
    applied: Option<Arc<dyn AppliedState>>,
}

impl PgDriftVerifier {
    /// A verifier over `backend` against `catalog`.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Catalog) -> Self {
        Self {
            backend,
            catalog,
            plan_options: PlanOptions::default(),
            applied: None,
        }
    }

    /// The same verifier with the planner options the apply used.
    #[must_use]
    pub fn with_plan_options(mut self, options: PlanOptions) -> Self {
        self.plan_options = options;
        self
    }

    /// The same verifier also re-diffing the plan's metadata, patches and behaviour through
    /// `applied`.
    #[must_use]
    pub fn with_applied_state(mut self, applied: Arc<dyn AppliedState>) -> Self {
        self.applied = Some(applied);
        self
    }

    /// The planner's remaining work, as verification sees it.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the live schema cannot be read.
    pub async fn remaining(&self) -> crate::Result<spot_pg_migrate::MigrationPlan> {
        let live =
            LiveSchema::read(&self.backend)
                .await
                .map_err(|error| Error::ControlUnavailable {
                    operation: "verifying the live schema".to_owned(),
                    message: error.to_string(),
                })?;
        Ok(plan_with(&live, &self.catalog, &self.plan_options))
    }
}

impl VerifyPort for PgDriftVerifier {
    fn verify<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, ApplyResult<VerifyOutcome>> {
        Box::pin(async move {
            let remaining = self.remaining().await.map_err(|error| {
                spot_migration_apply::Error::StepFailed {
                    index: 0,
                    id: "verify".to_owned(),
                    reason: error.to_string(),
                }
            })?;
            let mut mismatches: Vec<String> = remaining
                .steps
                .iter()
                .map(|step| step.key.clone())
                .collect();
            mismatches.extend(
                remaining
                    .refused
                    .iter()
                    .map(|refusal| format!("refused:{}", refusal.code)),
            );
            if let Some(applied) = &self.applied {
                let after = applied.applied().await?;
                mismatches.extend(
                    spot_migration_plan::verify_applied(plan, &after)
                        .mismatches
                        .iter()
                        .map(|change| change.id.clone()),
                );
            }
            Ok(VerifyOutcome {
                passed: mismatches.is_empty(),
                mismatches,
                invariants: Vec::new(),
            })
        })
    }
}

impl std::fmt::Debug for PgDriftVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgDriftVerifier")
            .field("catalog_version", &self.catalog.version)
            .finish_non_exhaustive()
    }
}
