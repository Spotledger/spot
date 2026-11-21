//! `verify`: asks the database, as the BI role, whether the role can do only what it should.
//!
//! The checks run inside one transaction that is always rolled back. Writes are attempted with
//! the read-only default switched off, so a missing privilege is what stops them, and a write that
//! succeeds is rolled back before anything else runs.

use deadpool_postgres::Pool;
use tokio_postgres::error::SqlState;

use crate::db::database;
use crate::error::BiError;
use crate::sql::{BI_SCHEMA, READER_ROLE, quote_ident};

/// One check and its outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyCheck {
    /// What was checked.
    pub name: String,
    /// Whether it held.
    pub passed: bool,
    /// What was seen. Names of objects and SQLSTATE codes, never row values.
    pub detail: String,
}

/// Every check `verify` ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// The checks, in the order they ran.
    pub checks: Vec<VerifyCheck>,
}

impl VerifyReport {
    /// Whether every check held.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// The checks that did not hold.
    pub fn failures(&self) -> impl Iterator<Item = &VerifyCheck> {
        self.checks.iter().filter(|c| !c.passed)
    }

    fn push(&mut self, name: impl Into<String>, passed: bool, detail: impl Into<String>) {
        self.checks.push(VerifyCheck {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
    }
}

/// Runs every check. The connection must belong to a role that may `SET ROLE` to the BI role
/// (a superuser or a member), which is how the admin console and CI call it.
///
/// # Errors
///
/// `bi.database` when the checks cannot run at all. A check that fails is in the report.
pub async fn verify(pool: &Pool) -> Result<VerifyReport, BiError> {
    let mut conn = pool
        .get()
        .await
        .map_err(|_| database("no connection from the pool"))?;
    let tx = conn.transaction().await.map_err(db)?;
    let mut report = VerifyReport::default();

    // As the administrator: the role itself, its settings, and what it holds.
    let role = tx
        .query_opt(
            "SELECT rolcanlogin, rolsuper, rolbypassrls, rolcreatedb, rolcreaterole \
             FROM pg_roles WHERE rolname = $1",
            &[&READER_ROLE],
        )
        .await
        .map_err(db)?;
    match role {
        None => report.push("role exists", false, "the role is missing"),
        Some(row) => {
            report.push("role exists", true, "present");
            let login: bool = row.get(0);
            let flags = [
                ("cannot log in", !login),
                ("is not a superuser", !row.get::<_, bool>(1)),
                ("does not bypass row security", !row.get::<_, bool>(2)),
                ("cannot create databases", !row.get::<_, bool>(3)),
                ("cannot create roles", !row.get::<_, bool>(4)),
            ];
            for (label, ok) in flags {
                report.push(format!("role {label}"), ok, if ok { "yes" } else { "no" });
            }
        }
    }
    let settings: String = tx
        .query_one(
            "SELECT COALESCE(string_agg(array_to_string(s.setconfig, ','), ','), '') \
             FROM pg_db_role_setting s JOIN pg_roles r ON r.oid = s.setrole \
             WHERE r.rolname = $1 \
             AND s.setdatabase IN (0, (SELECT oid FROM pg_database WHERE datname = current_database()))",
            &[&READER_ROLE],
        )
        .await
        .map_err(db)?
        .get(0);
    for wanted in [
        "default_transaction_read_only=on",
        "statement_timeout=",
        "idle_in_transaction_session_timeout=",
    ] {
        let ok = settings.contains(wanted);
        report.push(
            format!("role default has {wanted}"),
            ok,
            if ok { "set" } else { "missing" },
        );
    }

    let writable_views = tx
        .query(
            "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind = 'v' \
             AND has_table_privilege($2::name, c.oid, 'INSERT,UPDATE,DELETE,TRUNCATE') ORDER BY 1",
            &[&BI_SCHEMA, &READER_ROLE],
        )
        .await
        .map_err(db)?;
    let writable_views: Vec<String> = writable_views.iter().map(|r| r.get(0)).collect();
    report.push(
        "role holds no write privilege on bi views",
        writable_views.is_empty(),
        if writable_views.is_empty() {
            "none".to_owned()
        } else {
            writable_views.join(", ")
        },
    );

    let view_rows = tx
        .query(
            "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind = 'v' ORDER BY 1",
            &[&BI_SCHEMA],
        )
        .await
        .map_err(db)?;
    let views: Vec<String> = view_rows.iter().map(|r| r.get(0)).collect();

    let base_rows = tx
        .query(
            "SELECT n.nspname::text, c.relname::text FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
             AND n.nspname NOT IN ('pg_catalog', 'information_schema', $1) \
             AND n.nspname NOT LIKE 'pg_toast%' ORDER BY 1, 2",
            &[&BI_SCHEMA],
        )
        .await
        .map_err(db)?;
    let base: Vec<(String, String)> = base_rows.iter().map(|r| (r.get(0), r.get(1))).collect();

    // Privileges, read from the catalogue: no table outside the views is readable or writable.
    let granted = tx
        .query(
            "SELECT n.nspname::text || '.' || c.relname::text FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
             AND n.nspname NOT IN ('pg_catalog', 'information_schema', $1) \
             AND n.nspname NOT LIKE 'pg_toast%' \
             AND (has_table_privilege($2::name, c.oid, 'SELECT') \
                  OR has_any_column_privilege($2::name, c.oid, 'SELECT') \
                  OR has_table_privilege($2::name, c.oid, 'INSERT,UPDATE,DELETE,TRUNCATE')) \
             ORDER BY 1",
            &[&BI_SCHEMA, &READER_ROLE],
        )
        .await
        .map_err(db)?;
    let granted: Vec<String> = granted.iter().map(|r| r.get(0)).collect();
    report.push(
        "no table outside bi is readable or writable",
        granted.is_empty(),
        if granted.is_empty() {
            "none".to_owned()
        } else {
            granted.join(", ")
        },
    );
    let schemas = tx
        .query(
            "SELECT n.nspname::text FROM pg_namespace n \
             WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
             AND has_schema_privilege($1::name, n.nspname, 'CREATE') ORDER BY 1",
            &[&READER_ROLE],
        )
        .await
        .map_err(db)?;
    let schemas: Vec<String> = schemas.iter().map(|r| r.get(0)).collect();
    report.push(
        "role cannot create objects in any schema",
        schemas.is_empty(),
        if schemas.is_empty() {
            "none".to_owned()
        } else {
            schemas.join(", ")
        },
    );

    // As the BI role: real statements.
    tx.batch_execute("SET LOCAL default_transaction_read_only = off")
        .await
        .map_err(db)?;
    tx.batch_execute(&format!("SET LOCAL ROLE {}", quote_ident(READER_ROLE)))
        .await
        .map_err(db)?;
    for view in &views {
        let qualified = format!("{}.{}", quote_ident(BI_SCHEMA), quote_ident(view));
        let read = attempt(&tx, &format!("SELECT count(*) FROM {qualified}")).await?;
        report.push(
            format!("role reads {BI_SCHEMA}.{view}"),
            read.is_none(),
            outcome(read),
        );
        // Some views cannot be written at all (SQLSTATE 55000), so the refusal is checked
        // here and the privilege above is what makes it hold.
        let write = attempt(&tx, &format!("DELETE FROM {qualified}")).await?;
        report.push(
            format!("statement refused: DELETE on {BI_SCHEMA}.{view}"),
            write.is_some(),
            outcome(write),
        );
    }
    for (schema, table) in &base {
        let qualified = format!("{}.{}", quote_ident(schema), quote_ident(table));
        let read = attempt(&tx, &format!("SELECT count(*) FROM {qualified}")).await?;
        report.push(
            format!("role cannot read {schema}.{table}"),
            is_denied(&read),
            outcome(read),
        );
    }
    let create = attempt(
        &tx,
        &format!(
            "CREATE TABLE {}.spot_bi_probe (x integer)",
            quote_ident(BI_SCHEMA)
        ),
    )
    .await?;
    report.push(
        format!("role cannot create objects in {BI_SCHEMA}"),
        is_denied(&create),
        outcome(create),
    );
    tx.rollback().await.map_err(db)?;
    Ok(report)
}

/// Runs one statement inside a savepoint. `None` when it succeeded, else its SQLSTATE (or
/// `"?"` when the error carries none).
async fn attempt(
    tx: &tokio_postgres::Transaction<'_>,
    sql: &str,
) -> Result<Option<String>, BiError> {
    tx.batch_execute("SAVEPOINT bi_verify").await.map_err(db)?;
    let result = tx.batch_execute(sql).await;
    tx.batch_execute("ROLLBACK TO SAVEPOINT bi_verify")
        .await
        .map_err(db)?;
    Ok(match result {
        Ok(()) => None,
        Err(error) => Some(
            error
                .code()
                .map_or_else(|| "?".to_owned(), |c| c.code().to_owned()),
        ),
    })
}

fn is_denied(outcome: &Option<String>) -> bool {
    outcome.as_deref() == Some(SqlState::INSUFFICIENT_PRIVILEGE.code())
}

fn outcome(result: Option<String>) -> String {
    match result {
        None => "succeeded".to_owned(),
        Some(code) => format!("failed with SQLSTATE {code}"),
    }
}

fn db(error: tokio_postgres::Error) -> BiError {
    match error.code() {
        Some(code) => BiError::new(
            crate::error::DATABASE,
            format!("the database refused the check (SQLSTATE {})", code.code()),
        ),
        None => BiError::new(crate::error::DATABASE, "the database connection failed"),
    }
}
