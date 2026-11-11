//! Running a plan online.

use std::time::Duration;

use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, StorageBackend};
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

use crate::conn::connect;
use crate::error::{MigrateError, db, is_lock_error, storage};
use crate::extra::STEP_PREFIX;
use crate::live::{LiveSchema, MANAGED_EXTENSIONS, read_extensions};
use crate::names::{PROGRESS_TABLE, quote};
use crate::plan::{
    BackfillSpec, ExtensionNeed, MigrationPlan, PlanOptions, Run, Step, check_extension, plan_with,
    required_extensions,
};

/// The key prefix of an index build step: `create_index:<table>.<index name>`.
const CREATE_INDEX_PREFIX: &str = "create_index:";

/// The bytes of `SPOTMIGR`: one migration per database at a time.
pub(crate) const ADVISORY_KEY: i64 = 0x5350_4f54_4d49_4752;

/// Knobs of [`apply`].
#[derive(Clone, Debug)]
pub struct ApplyOptions {
    /// `lock_timeout` for every step. Default 5 s.
    pub lock_timeout: Duration,
    /// `statement_timeout` for every step except an index build. Default 60 s.
    pub statement_timeout: Duration,
    /// `statement_timeout` for `CREATE INDEX CONCURRENTLY`. Default 15 minutes.
    pub index_statement_timeout: Duration,
    /// How many times a step is retried after a lock timeout or deadlock. Default 5.
    pub max_lock_retries: u32,
    /// Rows per backfill batch. Default 5,000.
    pub batch_size: u32,
    /// Test hook: stop with `migrate.interrupted` after the step with this index completes.
    pub stop_after_step: Option<usize>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            lock_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(60),
            index_statement_timeout: Duration::from_secs(15 * 60),
            max_lock_retries: 5,
            batch_size: 5_000,
            stop_after_step: None,
        }
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    /// The keys of the steps that ran, in order. Empty when the database was already current.
    pub executed: Vec<String>,
    /// Rows filled by backfill steps.
    pub backfilled_rows: u64,
    /// The hash of the plan that was executed (after the re-plan under the lock).
    pub plan_hash: String,
}

/// Plans the change to `wanted` without side effects.
///
/// The plan also carries what the database has of the managed extensions, so a catalogue that
/// needs one the database lacks comes back with a refusal naming `extension.install`.
pub async fn dry_run(
    backend: &PgBackend,
    wanted: &Catalog,
    options: &PlanOptions,
) -> Result<MigrationPlan, MigrateError> {
    let client = connect(backend).await?;
    let live = LiveSchema::read_with(&client).await?;
    let mut options = options.clone();
    options.extensions = Some(read_managed_extensions(&client).await?);
    Ok(plan_with(&live, wanted, &options))
}

async fn read_managed_extensions(
    client: &Client,
) -> Result<std::collections::BTreeMap<String, crate::live::LiveExtension>, MigrateError> {
    let names: Vec<String> = MANAGED_EXTENSIONS.iter().map(|n| (*n).to_owned()).collect();
    read_extensions(client, &names).await
}

/// Checks `needs` against `pg_available_extensions` and `pg_extension`; the refusals, if any,
/// come back as one [`MigrateError::Refused`]. Reads only.
async fn check_needs(client: &Client, needs: &[ExtensionNeed]) -> Result<(), MigrateError> {
    if needs.is_empty() {
        return Ok(());
    }
    let inventory = read_managed_extensions(client).await?;
    let refusals: Vec<_> = needs
        .iter()
        .filter_map(|need| check_extension(need, inventory.get(&need.name)))
        .collect();
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(MigrateError::Refused(refusals))
    }
}

/// Applies `plan` online.
///
/// Refused changes in the plan stop everything before anything is touched. Otherwise the call
/// takes the session advisory lock, reads the live schema again and re-plans (so a second node
/// finds nothing to do, and a rerun after a crash does only the rest), runs the steps of the
/// fresh plan, and checks that the schema now matches the catalogue (`migrate.drift`).
pub async fn apply(
    backend: &PgBackend,
    plan: &MigrationPlan,
    options: ApplyOptions,
) -> Result<ApplyReport, MigrateError> {
    if !plan.refused.is_empty() {
        return Err(MigrateError::Refused(plan.refused.clone()));
    }
    let client = connect(backend).await?;
    client
        .execute("SELECT pg_advisory_lock($1)", &[&ADVISORY_KEY])
        .await
        .map_err(|e| db(&e))?;
    let result = run_locked(backend, &client, plan, &options).await;
    let _ = client
        .execute("SELECT pg_advisory_unlock($1)", &[&ADVISORY_KEY])
        .await;
    result
}

async fn run_locked(
    backend: &PgBackend,
    client: &Client,
    plan: &MigrationPlan,
    options: &ApplyOptions,
) -> Result<ApplyReport, MigrateError> {
    // Before anything is written, not even the bookkeeping table: every extension the catalogue
    // needs, and every `Run::Extension` requirement in the plan, must be satisfied. A tenant is
    // never left half-migrated because an extension was missing (ADR-087 section 4.3). The
    // database is the authority here, not the inventory the plan was built with, which may be
    // stale or absent.
    let mut needs = required_extensions(&plan.wanted, &plan.options);
    for step in &plan.steps {
        if let Run::Extension { name, min_version } = &step.run {
            needs.push(ExtensionNeed {
                name: name.clone(),
                min_version: min_version.clone(),
                entity: String::new(),
                because: "required by the plan".to_owned(),
            });
        }
    }
    check_needs(client, &needs).await?;
    // The re-plans below take no verdict on extensions: the check above just made it.
    let options_for_plan = PlanOptions {
        extensions: None,
        ..plan.options.clone()
    };
    client
        .batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {} (key text COLLATE \"C\" PRIMARY KEY, \
             state text NOT NULL, rows_done bigint NOT NULL DEFAULT 0, \
             updated_at timestamptz NOT NULL DEFAULT now())",
            quote(PROGRESS_TABLE)
        ))
        .await
        .map_err(|e| db(&e))?;
    set_timeouts(client, options.lock_timeout, options.statement_timeout).await?;

    let live = LiveSchema::read_with(client).await?;
    let fresh = plan_with(&live, &plan.wanted, &options_for_plan);
    if !fresh.refused.is_empty() {
        return Err(MigrateError::Refused(fresh.refused));
    }
    let mut report = ApplyReport {
        executed: Vec::new(),
        backfilled_rows: 0,
        plan_hash: fresh.plan_hash.clone(),
    };
    for (index, step) in fresh.steps.iter().enumerate() {
        report.backfilled_rows += run_step(backend, client, step, &fresh, options).await?;
        report.executed.push(step.key.clone());
        if options.stop_after_step == Some(index) {
            return Err(MigrateError::Interrupted { step: index });
        }
    }

    let after = plan_with(
        &LiveSchema::read_with(client).await?,
        &plan.wanted,
        &options_for_plan,
    );
    if !after.is_empty() {
        let mut what: Vec<String> = after.steps.iter().take(3).map(|s| s.key.clone()).collect();
        what.extend(after.refused.iter().take(3).map(|r| r.reason.clone()));
        return Err(MigrateError::Drift {
            what: what.join("; "),
        });
    }
    Ok(report)
}

async fn set_timeouts(
    client: &Client,
    lock: Duration,
    statement: Duration,
) -> Result<(), MigrateError> {
    client
        .execute(
            "SELECT set_config('lock_timeout', $1, false), set_config('statement_timeout', $2, false)",
            &[&format!("{}ms", lock.as_millis()), &format!("{}ms", statement.as_millis())],
        )
        .await
        .map_err(|e| db(&e))?;
    Ok(())
}

/// Runs one step; returns the rows it backfilled.
async fn run_step(
    backend: &PgBackend,
    client: &Client,
    step: &Step,
    plan: &MigrationPlan,
    options: &ApplyOptions,
) -> Result<u64, MigrateError> {
    match &step.run {
        Run::Tx(statements) => {
            with_retries(step, options, || run_tx(client, statements))
                .await
                .map_err(|e| redact_extra(step, e))?;
            Ok(0)
        }
        Run::Outside(sql) => {
            set_timeouts(
                client,
                options.lock_timeout,
                options.index_statement_timeout,
            )
            .await?;
            let result = with_retries(step, options, || async {
                client
                    .batch_execute(sql)
                    .await
                    .map_err(|e| (step_error(step, plan, &e), is_lock_error(&e)))
            })
            .await;
            set_timeouts(client, options.lock_timeout, options.statement_timeout).await?;
            result?;
            Ok(0)
        }
        Run::Backfill(spec) => backfill(client, step, spec, options).await,
        Run::SetNotNull(spec) => {
            with_retries(step, options, || set_not_null(client, spec)).await?;
            Ok(0)
        }
        Run::Extension { name, min_version } => {
            // A requirement, never an install: the check already ran before the first step,
            // and runs again here so a step list built by hand is held to it as well.
            let need = ExtensionNeed {
                name: name.clone(),
                min_version: min_version.clone(),
                entity: String::new(),
                because: "required by the plan".to_owned(),
            };
            check_needs(client, &[need]).await?;
            Ok(0)
        }
        Run::Finalise => {
            backend
                .prepare(&plan.wanted)
                .await
                .map_err(|e| storage(&e))?;
            Ok(0)
        }
    }
}

/// A failed extra table step reports the step key and the SQLSTATE only: the statements are the
/// caller's, and the server's message may quote them or a value.
fn redact_extra(step: &Step, e: MigrateError) -> MigrateError {
    match e {
        MigrateError::Database { message } if step.key.starts_with(STEP_PREFIX) => {
            let state = message.split_whitespace().next().unwrap_or("-----");
            MigrateError::Database {
                message: format!("{state} step {} failed", step.key),
            }
        }
        other => other,
    }
}

/// The error to report for the statement of `step` that failed with `e`.
///
/// Building a unique index over rows that already hold duplicates is the one failure an operator
/// can fix without reading the database log, so it is named: the table, the index and the
/// indexed columns come from the plan. Every other failure keeps the plain database error. The
/// server's detail text holds row values and is never read or copied.
fn step_error(step: &Step, plan: &MigrationPlan, e: &tokio_postgres::Error) -> MigrateError {
    if step.key.starts_with(CREATE_INDEX_PREFIX) && e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        duplicate_error(&step.key, &plan.wanted)
    } else {
        db(e)
    }
}

/// [`MigrateError::DuplicateValues`] for the step `step_key`, with the columns of the index looked
/// up in `wanted`. An index the catalogue does not name (the parent index of a child table, for
/// example, which the planner adds itself) gets an empty column list, never a different error.
fn duplicate_error(step_key: &str, wanted: &Catalog) -> MigrateError {
    let key = step_key
        .strip_prefix(CREATE_INDEX_PREFIX)
        .unwrap_or(step_key);
    let (table, index) = key.split_once('.').unwrap_or((key, ""));
    let columns: Vec<String> = wanted
        .entities()
        .find(|e| e.name.as_str() == table)
        .and_then(|e| e.indexes.iter().find(|i| i.name.as_str() == index))
        .map(|i| i.fields.iter().map(|f| f.as_str().to_owned()).collect())
        .unwrap_or_default();
    MigrateError::DuplicateValues {
        table: table.to_owned(),
        index: index.to_owned(),
        columns,
    }
}

async fn run_tx(client: &Client, statements: &[String]) -> Result<(), (MigrateError, bool)> {
    let fail = |e: tokio_postgres::Error| (db(&e), is_lock_error(&e));
    client.batch_execute("BEGIN").await.map_err(fail)?;
    for sql in statements {
        if let Err(e) = client.batch_execute(sql).await {
            let _ = client.batch_execute("ROLLBACK").await;
            return Err(fail(e));
        }
    }
    client.batch_execute("COMMIT").await.map_err(fail)
}

async fn set_not_null(client: &Client, spec: &BackfillSpec) -> Result<(), (MigrateError, bool)> {
    let table = quote(&spec.table);
    let column = quote(&spec.column);
    let key = format!("{}.{}", spec.table, spec.column).replace('\'', "''");
    let statements = [
        // The final sweep: rows inserted since the last batch.
        format!(
            "UPDATE {table} SET {column} = ($1::text)::{ty} WHERE {column} IS NULL",
            ty = spec.ty
        ),
        format!("ALTER TABLE {table} ALTER COLUMN {column} SET NOT NULL"),
        format!("DELETE FROM {} WHERE key = '{key}'", quote(PROGRESS_TABLE)),
    ];
    let fail = |e: tokio_postgres::Error| {
        // A writer slipped in a NULL between the sweep and the lock: retry the sweep.
        let retry = is_lock_error(&e) || e.code().map(|c| c.code()) == Some("23502");
        (db(&e), retry)
    };
    client.batch_execute("BEGIN").await.map_err(fail)?;
    let run = async {
        client.execute(&statements[0], &[&spec.value]).await?;
        client.batch_execute(&statements[1]).await?;
        client.batch_execute(&statements[2]).await
    };
    match run.await {
        Ok(()) => client.batch_execute("COMMIT").await.map_err(fail),
        Err(e) => {
            let _ = client.batch_execute("ROLLBACK").await;
            Err(fail(e))
        }
    }
}

/// Fills the NULLs of a column in batches; each batch commits on its own, so a crash loses at
/// most the batch in flight and a rerun continues with whatever is still NULL.
async fn backfill(
    client: &Client,
    step: &Step,
    spec: &BackfillSpec,
    options: &ApplyOptions,
) -> Result<u64, MigrateError> {
    let table = quote(&spec.table);
    let column = quote(&spec.column);
    let sql = format!(
        "UPDATE {table} SET {column} = ($1::text)::{ty} WHERE id IN \
         (SELECT id FROM {table} WHERE {column} IS NULL LIMIT {batch})",
        ty = spec.ty,
        batch = options.batch_size.max(1)
    );
    let key = format!("{}.{}", spec.table, spec.column).replace('\'', "''");
    let mut total: u64 = 0;
    loop {
        let done = with_retries(step, options, || async {
            client
                .execute(&sql, &[&spec.value])
                .await
                .map_err(|e| (db(&e), is_lock_error(&e)))
        })
        .await?;
        if done == 0 {
            return Ok(total);
        }
        total += done;
        client
            .batch_execute(&format!(
                "UPDATE {} SET rows_done = rows_done + {done}, updated_at = now() \
                 WHERE key = '{key}'",
                quote(PROGRESS_TABLE)
            ))
            .await
            .map_err(|e| db(&e))?;
    }
}

/// Runs `attempt`, retrying a lock timeout or deadlock with a jittered backoff up to the bound.
/// The jitter derives from the step key and the attempt number, so it is deterministic.
async fn with_retries<T, F, Fut>(
    step: &Step,
    options: &ApplyOptions,
    mut attempt: F,
) -> Result<T, MigrateError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, (MigrateError, bool)>>,
{
    let mut tries: u32 = 0;
    loop {
        match attempt().await {
            Ok(v) => return Ok(v),
            Err((e, true)) => {
                tries += 1;
                if tries > options.max_lock_retries {
                    return Err(MigrateError::LockTimeout);
                }
                let _ = e;
                let seed = step.key.bytes().fold(u64::from(tries), |h, b| {
                    h.wrapping_mul(31).wrapping_add(u64::from(b))
                });
                let jitter = seed % 100;
                let backoff = 100u64.saturating_mul(1 << tries.min(6)) + jitter;
                tokio::time::sleep(Duration::from_millis(backoff)).await;
            }
            Err((e, false)) => return Err(e),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::error::codes;
    use spot_storage_contract::{
        Collation, EntityDef, EntityKind, FieldDef, FieldType, IndexDef, IndexMethod, Name,
    };

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn field(name: &str) -> FieldDef {
        FieldDef {
            name: n(name),
            ty: FieldType::Text { max_chars: None },
            nullable: false,
        }
    }

    /// `invoice` with a unique index over `(series, number)`.
    fn invoice() -> Catalog {
        let entity = EntityDef {
            name: n("invoice"),
            kind: EntityKind::Document,
            fields: vec![field("series"), field("number")],
            children: vec![],
            indexes: vec![IndexDef {
                name: n("uq_invoice_number"),
                fields: vec![n("series"), n("number")],
                unique: true,
                method: IndexMethod::BTree,
                opclass: None,
                include: vec![],
            }],
            exclusions: vec![],
            search: None,
        };
        Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).unwrap()
    }

    #[test]
    fn a_failed_unique_build_names_the_table_the_index_and_the_columns() {
        let err = duplicate_error("create_index:invoice.uq_invoice_number", &invoice());
        assert_eq!(err.code(), codes::DUPLICATE_VALUES);
        let MigrateError::DuplicateValues {
            table,
            index,
            columns,
        } = &err
        else {
            panic!("expected the duplicate-values error, got {err:?}");
        };
        assert_eq!(table, "invoice");
        assert_eq!(index, "uq_invoice_number");
        assert_eq!(columns.join(", "), "series, number");
        assert_eq!(
            err.to_string(),
            "migrate.duplicate_values: cannot create unique index uq_invoice_number on invoice \
             (series, number): existing rows hold duplicate values; remove or merge the duplicates \
             and run the migration again"
        );
    }

    #[test]
    fn an_index_the_catalogue_does_not_name_gets_no_columns() {
        // The planner adds the parent index of a child table itself, so the catalogue lacks it.
        let err = duplicate_error(
            "create_index:invoice_line.invoice_line_parent_idx",
            &invoice(),
        );
        let MigrateError::DuplicateValues {
            table,
            index,
            columns,
        } = &err
        else {
            panic!("expected the duplicate-values error, got {err:?}");
        };
        assert_eq!(table, "invoice_line");
        assert_eq!(index, "invoice_line_parent_idx");
        assert!(columns.is_empty(), "no columns are invented: {columns:?}");
        assert_eq!(err.code(), codes::DUPLICATE_VALUES);
    }
}
