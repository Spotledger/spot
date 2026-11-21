//! [`PgAppRegistry`]: the PostgreSQL [`AppRegistryStore`].
//!
//! Every method takes one pooled connection and, when it writes more than one row, one
//! transaction on it: it never opens a second connection inside an operation that must be atomic.
//! A `commit_*` call writes the registry change and the journal row in that one transaction, so a
//! failure at any point (a refused version check, a journal write that fails, a dropped
//! connection) leaves neither visible.
//!
//! Every statement is parameterised and carries a `site` predicate; no SQL text is built from a
//! caller's value.

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_core::SiteId;
use spot_installer::{
    AppRegistryStore, BoxFuture, ExecError, InstallChange, InstallJournal, InstalledApp,
    LifecycleConflict, LifecycleError, SiteLifecycle, SiteState, UpgradeChange,
};
use tokio_postgres::Transaction;

use crate::codec::{
    AppColumns, depends_to_json, granted_to_json, kind_from, lifecycle_from, lifecycle_text,
    patches_to_json, state_from, to_i32, undo_from_json, undo_to_json,
};
use crate::error::{
    ALREADY_INSTALLED, NOT_INSTALLED, STEP_FAILED, corrupt, pg, pool as pool_error,
};
use crate::schema;

/// The PostgreSQL installer registry over a connection pool the caller builds.
#[derive(Clone)]
pub struct PgAppRegistry {
    pool: Pool,
}

impl std::fmt::Debug for PgAppRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgAppRegistry").finish_non_exhaustive()
    }
}

impl PgAppRegistry {
    /// A registry over `pool`. Call [`prepare`](Self::prepare) once before use.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the registry tables if they are missing (idempotent).
    ///
    /// # Errors
    /// `installer_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    /// Moves `site` from `from` to `to` only if the stored lifecycle is still `from`: a
    /// conditional `UPDATE`, so of two nodes racing for `Idle -> Installing` exactly one wins
    /// and the other gets an error naming both states. A site with no row is `Idle`.
    ///
    /// # Errors
    /// `installer.lease_held` when the target is `Installing` and the site already is;
    /// `installer.step_failed` for any other mismatch; `installer_pg.storage_failed` when the
    /// database cannot be written.
    pub async fn transition_lifecycle(
        &self,
        site: &SiteId,
        from: SiteLifecycle,
        to: SiteLifecycle,
    ) -> Result<(), ExecError> {
        self.move_lifecycle(site, from, to)
            .await
            .map_err(|error| error.into_exec_error(site))
    }

    /// The conditional move, with a conflict kept typed for the executor.
    async fn move_lifecycle(
        &self,
        site: &SiteId,
        from: SiteLifecycle,
        to: SiteLifecycle,
    ) -> Result<(), LifecycleError> {
        let mut conn = self.conn("transition_lifecycle").await?;
        let tx = conn
            .transaction()
            .await
            .map_err(|e| pg("begin transition_lifecycle", &e))?;
        // A site nobody has touched reads as idle, so give it that row before the compare.
        tx.execute(
            "INSERT INTO spot_installer_site (site, lifecycle) VALUES ($1, 'idle') \
             ON CONFLICT (site) DO NOTHING",
            &[&site.as_str()],
        )
        .await
        .map_err(|e| pg("seed the lifecycle flag", &e))?;
        let updated = tx
            .execute(
                "UPDATE spot_installer_site SET lifecycle = $3 WHERE site = $1 AND lifecycle = $2",
                &[&site.as_str(), &lifecycle_text(from), &lifecycle_text(to)],
            )
            .await
            .map_err(|e| pg("set the lifecycle flag", &e))?;
        if updated == 1 {
            return tx
                .commit()
                .await
                .map_err(|e| LifecycleError::Failed(pg("commit transition_lifecycle", &e)));
        }
        let found = tx
            .query_one(
                "SELECT lifecycle FROM spot_installer_site WHERE site = $1",
                &[&site.as_str()],
            )
            .await
            .map_err(|e| pg("read the lifecycle flag", &e))?;
        let stored = lifecycle_from(&found.get::<_, String>(0))?;
        Err(LifecycleError::Conflict(LifecycleConflict {
            stored,
            expected: from,
            target: to,
        }))
    }

    /// Records the DocTypes `app` owns on `site`, for the uninstall export hook. Idempotent: a
    /// DocType already recorded is left alone; none is removed.
    ///
    /// # Errors
    /// `installer_pg.storage_failed` when the database cannot be written.
    pub async fn register_owned_doctypes(
        &self,
        site: &SiteId,
        app: &str,
        doctypes: &[String],
    ) -> Result<(), ExecError> {
        let mut conn = self.conn("register_owned_doctypes").await?;
        let tx = conn
            .transaction()
            .await
            .map_err(|e| pg("register_owned_doctypes", &e))?;
        for doctype in doctypes {
            tx.execute(
                "INSERT INTO spot_installer_owned_doctype (site, app, doctype) VALUES ($1, $2, $3) \
                 ON CONFLICT DO NOTHING",
                &[&site.as_str(), &app, doctype],
            )
            .await
            .map_err(|e| pg("register_owned_doctypes", &e))?;
        }
        tx.commit()
            .await
            .map_err(|e| pg("register_owned_doctypes", &e))
    }

    async fn conn(&self, operation: &str) -> Result<Object, ExecError> {
        self.pool.get().await.map_err(|e| pool_error(operation, &e))
    }
}

/// Writes (or replaces) the journal row for a step on the caller's transaction.
async fn write_journal(
    tx: &Transaction<'_>,
    site: &SiteId,
    journal: &InstallJournal,
) -> Result<(), ExecError> {
    let index = to_i32(journal.step.index, "a step index")?;
    let undo = journal.undo.as_ref().map(undo_to_json);
    tx.execute(
        "INSERT INTO spot_installer_journal \
            (site, step_index, plan_hash, kind, name, state, started_at, undo) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (site, step_index) DO UPDATE SET \
            plan_hash = EXCLUDED.plan_hash, kind = EXCLUDED.kind, name = EXCLUDED.name, \
            state = EXCLUDED.state, started_at = EXCLUDED.started_at, undo = EXCLUDED.undo",
        &[
            &site.as_str(),
            &index,
            &journal.plan_hash,
            &journal.step.kind.as_str(),
            &journal.step.name,
            &journal.state.as_str(),
            &journal.started_at,
            &undo,
        ],
    )
    .await
    .map_err(|e| pg("write the journal row", &e))?;
    Ok(())
}

/// The stored version of `app` on `site`, or `None`.
async fn live_version(
    tx: &Transaction<'_>,
    site: &SiteId,
    app: &str,
) -> Result<Option<String>, ExecError> {
    let row = tx
        .query_opt(
            "SELECT version FROM spot_installer_app WHERE site = $1 AND app = $2",
            &[&site.as_str(), &app],
        )
        .await
        .map_err(|e| pg("read the live version", &e))?;
    Ok(row.map(|row| row.get::<_, String>(0)))
}

fn refuse_empty(app: &str) -> Result<(), ExecError> {
    if app.is_empty() {
        Err(ExecError::new(STEP_FAILED, "an app name must not be empty"))
    } else {
        Ok(())
    }
}

impl AppRegistryStore for PgAppRegistry {
    fn load(&self, site: &SiteId) -> BoxFuture<'_, Result<SiteState, ExecError>> {
        let site = site.clone();
        Box::pin(async move {
            let conn = self.conn("load").await?;
            let rows = conn
                .query(
                    "SELECT app, version, digest, approved, applied_patches, ord, depends \
                     FROM spot_installer_app WHERE site = $1 ORDER BY ord, app",
                    &[&site.as_str()],
                )
                .await
                .map_err(|e| pg("load the installed apps", &e))?;
            let mut apps = Vec::with_capacity(rows.len());
            for row in &rows {
                let name: String = row.get(0);
                let version: String = row.get(1);
                let digest: String = row.get(2);
                let approved: serde_json::Value = row.get(3);
                let patches: serde_json::Value = row.get(4);
                let depends: serde_json::Value = row.get(6);
                apps.push(
                    AppColumns {
                        name: &name,
                        version: &version,
                        digest: &digest,
                        approved: &approved,
                        applied_patches: &patches,
                        order: row.get(5),
                        depends: &depends,
                    }
                    .decode()?,
                );
            }
            Ok(SiteState { apps })
        })
    }

    fn lifecycle(&self, site: &SiteId) -> BoxFuture<'_, Result<SiteLifecycle, ExecError>> {
        let site = site.clone();
        Box::pin(async move {
            let conn = self.conn("lifecycle").await?;
            let row = conn
                .query_opt(
                    "SELECT lifecycle FROM spot_installer_site WHERE site = $1",
                    &[&site.as_str()],
                )
                .await
                .map_err(|e| pg("read the lifecycle flag", &e))?;
            match row {
                None => Ok(SiteLifecycle::Idle),
                Some(row) => lifecycle_from(&row.get::<_, String>(0)),
            }
        })
    }

    fn set_lifecycle(
        &self,
        site: &SiteId,
        from: SiteLifecycle,
        to: SiteLifecycle,
    ) -> BoxFuture<'_, Result<(), LifecycleError>> {
        let site = site.clone();
        Box::pin(async move { self.move_lifecycle(&site, from, to).await })
    }

    fn commit_install(
        &self,
        site: &SiteId,
        app: &str,
        change: InstallChange,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        let site = site.clone();
        let app = app.to_owned();
        Box::pin(async move {
            refuse_empty(&app)?;
            let mut conn = self.conn("commit_install").await?;
            let tx = conn
                .transaction()
                .await
                .map_err(|e| pg("begin commit_install", &e))?;
            // One install at a time per site takes the next `ord`: the lock is held to the end
            // of this transaction, so the MAX(ord) read below sees every earlier install.
            tx.execute(
                "SELECT pg_advisory_xact_lock(hashtext('spot_installer_ord'), hashtext($1))",
                &[&site.as_str()],
            )
            .await
            .map_err(|e| pg("lock the install order", &e))?;
            let version = change.version.to_string();
            let digest = change.digest.to_hex();
            let inserted = tx
                .execute(
                    "INSERT INTO spot_installer_app \
                        (site, app, version, digest, approved, applied_patches, ord, depends) \
                     SELECT $1, $2, $3, $4, $5, $6, \
                            COALESCE((SELECT MAX(ord) + 1 FROM spot_installer_app WHERE site = $1), 0), \
                            '[]'::jsonb \
                     ON CONFLICT (site, app) DO NOTHING",
                    &[
                        &site.as_str(),
                        &app,
                        &version,
                        &digest,
                        &granted_to_json(&change.approved),
                        &patches_to_json(&change.applied_patches),
                    ],
                )
                .await
                .map_err(|e| pg("insert the app", &e))?;
            if inserted == 0 {
                // A retried commit of the same bytes is a no-op; anything else is a refusal.
                let existing = tx
                    .query_opt(
                        "SELECT version, digest FROM spot_installer_app WHERE site = $1 AND app = $2",
                        &[&site.as_str(), &app],
                    )
                    .await
                    .map_err(|e| pg("read the installed app", &e))?;
                let same = existing.is_some_and(|row| {
                    row.get::<_, String>(0) == version && row.get::<_, String>(1) == digest
                });
                if !same {
                    return Err(ExecError::new(
                        ALREADY_INSTALLED,
                        format!("`{app}` is already installed with a different version or digest"),
                    ));
                }
            }
            write_journal(&tx, &site, &journal).await?;
            tx.commit()
                .await
                .map_err(|e| pg("commit commit_install", &e))
        })
    }

    fn commit_upgrade(
        &self,
        site: &SiteId,
        app: &str,
        change: UpgradeChange,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        let site = site.clone();
        let app = app.to_owned();
        Box::pin(async move {
            refuse_empty(&app)?;
            let mut conn = self.conn("commit_upgrade").await?;
            let tx = conn
                .transaction()
                .await
                .map_err(|e| pg("begin commit_upgrade", &e))?;
            let from = change.from.to_string();
            let to = change.to.to_string();
            // The version check is part of the UPDATE, so two racing upgrades serialise on the
            // row and exactly one matches `from`.
            let updated = tx
                .execute(
                    "UPDATE spot_installer_app SET version = $4, digest = $5, approved = $6, \
                        applied_patches = $7 \
                     WHERE site = $1 AND app = $2 AND version = $3",
                    &[
                        &site.as_str(),
                        &app,
                        &from,
                        &to,
                        &change.digest.to_hex(),
                        &granted_to_json(&change.approved),
                        &patches_to_json(&change.applied_patches),
                    ],
                )
                .await
                .map_err(|e| pg("update the app", &e))?;
            if updated == 0 {
                return Err(match live_version(&tx, &site, &app).await? {
                    Some(found) => ExecError::new(
                        STEP_FAILED,
                        format!("`{app}` is at {found}, not {from}; the commit lost the race"),
                    ),
                    None => ExecError::new(
                        NOT_INSTALLED,
                        format!("`{app}` is not installed, so it cannot be upgraded"),
                    ),
                });
            }
            write_journal(&tx, &site, &journal).await?;
            tx.commit()
                .await
                .map_err(|e| pg("commit commit_upgrade", &e))
        })
    }

    fn commit_uninstall(
        &self,
        site: &SiteId,
        app: &str,
        purge: bool,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        let site = site.clone();
        let app = app.to_owned();
        Box::pin(async move {
            let mut conn = self.conn("commit_uninstall").await?;
            let tx = conn
                .transaction()
                .await
                .map_err(|e| pg("begin commit_uninstall", &e))?;
            let removed = tx
                .execute(
                    "DELETE FROM spot_installer_app WHERE site = $1 AND app = $2",
                    &[&site.as_str(), &app],
                )
                .await
                .map_err(|e| pg("delete the app", &e))?;
            if removed == 0 {
                return Err(ExecError::new(
                    NOT_INSTALLED,
                    format!("`{app}` is not installed, so it cannot be uninstalled"),
                ));
            }
            if purge {
                // The app's data is deleted with the purge, so nothing is owned any more. Without
                // a purge the ownership stays: the data is still there.
                tx.execute(
                    "DELETE FROM spot_installer_owned_doctype WHERE site = $1 AND app = $2",
                    &[&site.as_str(), &app],
                )
                .await
                .map_err(|e| pg("forget the owned DocTypes", &e))?;
            }
            write_journal(&tx, &site, &journal).await?;
            tx.commit()
                .await
                .map_err(|e| pg("commit commit_uninstall", &e))
        })
    }

    fn restore(
        &self,
        site: &SiteId,
        app: &InstalledApp,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        let site = site.clone();
        let app = app.clone();
        Box::pin(async move {
            refuse_empty(&app.name)?;
            let order = to_i32(app.order, "an install order")?;
            let mut conn = self.conn("restore").await?;
            let tx = conn
                .transaction()
                .await
                .map_err(|e| pg("begin restore", &e))?;
            tx.execute(
                "INSERT INTO spot_installer_app \
                    (site, app, version, digest, approved, applied_patches, ord, depends) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (site, app) DO UPDATE SET \
                    version = EXCLUDED.version, digest = EXCLUDED.digest, \
                    approved = EXCLUDED.approved, applied_patches = EXCLUDED.applied_patches, \
                    ord = EXCLUDED.ord, depends = EXCLUDED.depends",
                &[
                    &site.as_str(),
                    &app.name,
                    &app.version.to_string(),
                    &app.digest.to_hex(),
                    &granted_to_json(&app.approved),
                    &patches_to_json(&app.applied_patches),
                    &order,
                    &depends_to_json(&app.depends),
                ],
            )
            .await
            .map_err(|e| pg("restore the app", &e))?;
            write_journal(&tx, &site, &journal).await?;
            tx.commit().await.map_err(|e| pg("commit restore", &e))
        })
    }

    fn record(
        &self,
        site: &SiteId,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        let site = site.clone();
        Box::pin(async move {
            let mut conn = self.conn("record").await?;
            let tx = conn
                .transaction()
                .await
                .map_err(|e| pg("begin record", &e))?;
            write_journal(&tx, &site, &journal).await?;
            tx.commit().await.map_err(|e| pg("commit record", &e))
        })
    }

    fn journal(&self, site: &SiteId) -> BoxFuture<'_, Result<Vec<InstallJournal>, ExecError>> {
        let site = site.clone();
        Box::pin(async move {
            let conn = self.conn("journal").await?;
            let rows = conn
                .query(
                    "SELECT step_index, plan_hash, kind, name, state, started_at, undo \
                     FROM spot_installer_journal WHERE site = $1 ORDER BY step_index",
                    &[&site.as_str()],
                )
                .await
                .map_err(|e| pg("read the journal", &e))?;
            rows.iter()
                .map(|row| {
                    let index: i32 = row.get(0);
                    let kind: String = row.get(2);
                    let name: String = row.get(3);
                    let state: String = row.get(4);
                    let undo: Option<serde_json::Value> = row.get(6);
                    let index = u32::try_from(index)
                        .map_err(|_| corrupt("a stored step index is negative"))?;
                    Ok(InstallJournal::new(
                        row.get::<_, String>(1),
                        spot_installer::StepId::new(index, kind_from(&kind)?, name),
                        state_from(&state)?,
                        row.get::<_, i64>(5),
                        undo.as_ref().map(undo_from_json).transpose()?,
                    ))
                })
                .collect()
        })
    }

    fn clear_journal(&self, site: &SiteId) -> BoxFuture<'_, Result<(), ExecError>> {
        let site = site.clone();
        Box::pin(async move {
            let conn = self.conn("clear_journal").await?;
            conn.execute(
                "DELETE FROM spot_installer_journal WHERE site = $1",
                &[&site.as_str()],
            )
            .await
            .map_err(|e| pg("clear the journal", &e))?;
            Ok(())
        })
    }

    fn owned_doctypes(
        &self,
        site: &SiteId,
        app: &str,
    ) -> BoxFuture<'_, Result<Vec<String>, ExecError>> {
        let site = site.clone();
        let app = app.to_owned();
        Box::pin(async move {
            let conn = self.conn("owned_doctypes").await?;
            let rows = conn
                .query(
                    "SELECT doctype FROM spot_installer_owned_doctype \
                     WHERE site = $1 AND app = $2 ORDER BY doctype",
                    &[&site.as_str(), &app],
                )
                .await
                .map_err(|e| pg("read the owned DocTypes", &e))?;
            Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
        })
    }
}
