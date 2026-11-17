//! `spot migrate --apply` seeds the platform's missing system rows (WP-IAM-02.T3).
//!
//! `bootstrap::seed` runs only when a site is created, so a site that predates a role added to
//! `apps/platform/bootstrap.toml` never gets it. After the schema step, `migrate --apply` runs
//! [`seed_missing`]: the platform's `seed_system_rows` in ONE transaction on the same path as
//! `users::seed_platform` (same catalogue, same options, rolled back on any error).
//!
//! What it writes: the system roles (and any other non-identity row `bootstrap.toml` declares) whose
//! fixed id is not there yet. What it never does: create the `Administrator`, a user, a role
//! assignment or any credential; overwrite or update an existing row (an edited description
//! survives); run on a bare site (no platform app). A role an operator deleted on purpose is
//! created again by the next `--apply`: the grants name the roles.
//!
//! Seeded system rows are reference data written as the system principal through the document
//! store directly, as `seed` does: they carry no audit entry, and `Role.origin` is `reference`.

use spot_meta::EffectiveMeta;
use spot_platform_app::bootstrap::{SeedError, SeedParams, seed_system_rows};
use spot_postgres::PgBackend;
use spot_storage_contract::{Isolation, Name, StorageBackend, Transaction, TxOptions};
use std::sync::Arc;

use crate::config::Site;
use crate::{Error, Result, error, schema};

fn options() -> Result<TxOptions> {
    Ok(TxOptions::new(
        Name::new("spot_cli").map_err(error)?,
        Isolation::ReadCommitted,
    ))
}

/// Whether `site` has the platform app (a `--bare` site has none and is skipped silently).
#[must_use]
pub fn has_platform(site: &Site) -> bool {
    site.apps.first().map(String::as_str) == Some(crate::appdir::PLATFORM)
}

/// Writes the missing system rows in one transaction and returns the role names it wrote.
///
/// # Errors
/// [`Error`] naming the cause; the transaction is rolled back, nothing is written.
pub async fn seed_missing(backend: &PgBackend, meta: &EffectiveMeta) -> Result<Vec<String>> {
    run(backend, meta, true).await
}

/// The names `seed_missing` would write, read-only: the transaction is always rolled back.
///
/// # Errors
/// [`Error`] when the rows cannot be read (for instance the platform tables are not there yet).
pub async fn would_add(backend: &PgBackend, meta: &EffectiveMeta) -> Result<Vec<String>> {
    run(backend, meta, false).await
}

async fn run(backend: &PgBackend, meta: &EffectiveMeta, commit: bool) -> Result<Vec<String>> {
    let catalog = Arc::new(schema::catalog(meta)?);
    let mut tx = backend.begin(catalog, options()?).await.map_err(error)?;
    let outcome = match seed_system_rows(&mut tx, &SeedParams::default()).await {
        Ok(outcome) => outcome,
        Err(e) => {
            let _ = tx.rollback().await;
            return Err(Error(describe(&e)));
        }
    };
    if commit {
        tx.commit().await.map_err(error)?;
    } else {
        tx.rollback().await.map_err(error)?;
    }
    Ok(outcome.roles_added)
}

fn describe(error: &SeedError) -> String {
    // The text names a DocType or a storage cause, never a row value.
    error.to_string()
}

/// The lines `migrate` prints for the seed step of an applied run.
#[must_use]
pub fn applied_lines(added: &[String]) -> String {
    if added.is_empty() {
        return "seed: nothing to add\n".to_owned();
    }
    added
        .iter()
        .map(|role| format!("seed: added role \"{role}\"\n"))
        .collect()
}

/// The lines `migrate` prints for a plan-only run.
#[must_use]
pub fn would_lines(added: &[String]) -> String {
    if added.is_empty() {
        return "seed: nothing to add\n".to_owned();
    }
    added
        .iter()
        .map(|role| format!("seed: would add role \"{role}\"\n"))
        .collect()
}

/// The text of a failed seed: the schema step is already applied and a re-run is safe.
#[must_use]
pub fn failure(cause: &str) -> String {
    format!(
        "seed failed: {cause}\nthe schema step is already applied; the seed was rolled back and \
         nothing partial was written; re-running `spot migrate --apply` is safe (the plan is then \
         empty and the seed is retried)"
    )
}
