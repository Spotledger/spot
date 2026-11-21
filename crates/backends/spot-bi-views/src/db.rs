//! `apply`, `retire` and the role setup. Everything runs in one transaction per call, with a lock
//! timeout, and nothing is dropped unless `retire` is called.

use deadpool_postgres::Pool;
use spot_storage_contract::{Catalog, Name};
use tokio_postgres::Transaction;

use crate::error::{BREAKING_CHANGE, BiError, DATABASE, NOT_A_VIEW, UNKNOWN_ENTITY};
use crate::plan::{BiPlan, ViewPlan};
use crate::sql::{
    BI_SCHEMA, CONNECTION_LIMIT, IDLE_TRANSACTION_TIMEOUT, LOCK_TIMEOUT, READER_ROLE,
    STATEMENT_TIMEOUT, quote_ident, quote_literal,
};

/// What `apply` did, per view. A second apply of an unchanged plan reports nothing created or
/// replaced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Views that did not exist and were created.
    pub created: Vec<String>,
    /// Views whose definition changed, or that gained columns, and were replaced in place.
    pub replaced: Vec<String>,
    /// Views already matching the plan.
    pub unchanged: Vec<String>,
}

impl ApplyReport {
    /// Whether any view was created or replaced.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.created.is_empty() || !self.replaced.is_empty()
    }
}

/// Makes the database's `bi` schema match the plan.
///
/// A view that does not exist is created. A view that exists with the same columns and the same
/// fingerprint is left alone. A view whose columns are the plan's columns with more added at the
/// end, or whose definition changed, is replaced with `CREATE OR REPLACE VIEW`. A view whose
/// columns were removed, retyped or reordered is never touched: the call fails with
/// `bi.breaking_change` and nothing is applied, so a new `version` is needed.
///
/// # Errors
///
/// `bi.breaking_change`, `bi.not_a_view` for a non-view object with a planned name, and
/// `bi.database` for anything the server refuses (the statement itself is not in the message).
pub async fn apply(pool: &Pool, plan: &BiPlan) -> Result<ApplyReport, BiError> {
    let mut conn = pool
        .get()
        .await
        .map_err(|_| database("no connection from the pool"))?;
    let tx = conn.transaction().await.map_err(db)?;
    tx.execute(
        "SELECT set_config('lock_timeout', $1, true)",
        &[&LOCK_TIMEOUT],
    )
    .await
    .map_err(db)?;
    for statement in role_setup() {
        tx.batch_execute(&statement).await.map_err(db)?;
    }
    tx.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(BI_SCHEMA)
    ))
    .await
    .map_err(db)?;
    tx.batch_execute(&format!(
        "REVOKE ALL ON SCHEMA {} FROM PUBLIC",
        quote_ident(BI_SCHEMA)
    ))
    .await
    .map_err(db)?;
    tx.batch_execute(&format!(
        "GRANT USAGE ON SCHEMA {} TO {}",
        quote_ident(BI_SCHEMA),
        quote_ident(READER_ROLE)
    ))
    .await
    .map_err(db)?;
    let mut report = ApplyReport::default();
    for view in plan.views.iter().chain(std::iter::once(&plan.catalogue)) {
        let existing = existing_view(&tx, &view.name).await?;
        match decide(existing.as_ref(), view)? {
            Action::Create => {
                create(&tx, view, false).await?;
                report.created.push(view.name.clone());
            }
            Action::Replace => {
                create(&tx, view, true).await?;
                report.replaced.push(view.name.clone());
            }
            Action::Unchanged => report.unchanged.push(view.name.clone()),
        }
        tx.batch_execute(&format!(
            "GRANT SELECT ON {}.{} TO {}",
            quote_ident(BI_SCHEMA),
            quote_ident(&view.name),
            quote_ident(READER_ROLE)
        ))
        .await
        .map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(report)
}

/// Drops the views of `entity` at `version`: the document view and each child view the catalogue
/// gives it. Nothing else is dropped. Run `apply` again afterwards so that `bi.catalogue` stops
/// listing them.
///
/// # Errors
///
/// `bi.unknown_entity` when the catalogue has no such entity, `bi.database` for a refused drop.
pub async fn retire(
    pool: &Pool,
    catalog: &Catalog,
    entity: &Name,
    version: u32,
) -> Result<Vec<String>, BiError> {
    let def = catalog.entity(entity).map_err(|_| {
        BiError::new(
            UNKNOWN_ENTITY,
            format!("`{entity}` is not in the catalogue"),
        )
    })?;
    let mut names = vec![crate::sql::view_name(version, entity.as_str())];
    for table in &def.children {
        names.push(crate::sql::child_view_name(
            version,
            entity.as_str(),
            table.entity.as_str(),
        ));
    }
    let mut conn = pool
        .get()
        .await
        .map_err(|_| database("no connection from the pool"))?;
    let tx = conn.transaction().await.map_err(db)?;
    let mut dropped = Vec::new();
    for name in names {
        if existing_view(&tx, &name).await?.is_some() {
            tx.batch_execute(&format!(
                "DROP VIEW {}.{}",
                quote_ident(BI_SCHEMA),
                quote_ident(&name)
            ))
            .await
            .map_err(db)?;
            dropped.push(name);
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(dropped)
}

/// The SQL that grants `user` the BI role. Credentials are not part of this crate: the admin
/// console creates the login; this only adds the membership.
#[must_use]
pub fn grant_role_sql(user: &Name) -> String {
    format!(
        "GRANT {} TO {}",
        quote_ident(READER_ROLE),
        quote_ident(user.as_str())
    )
}

/// The role, with its defaults: no login, read-only by default, timeouts and a connection limit.
fn role_setup() -> Vec<String> {
    let role = quote_ident(READER_ROLE);
    vec![
        format!(
            "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {}) THEN CREATE ROLE {role} NOLOGIN; END IF; END $$",
            quote_literal(READER_ROLE)
        ),
        format!("ALTER ROLE {role} NOLOGIN CONNECTION LIMIT {CONNECTION_LIMIT}"),
        format!("ALTER ROLE {role} SET default_transaction_read_only = on"),
        format!(
            "ALTER ROLE {role} SET statement_timeout = {}",
            quote_literal(STATEMENT_TIMEOUT)
        ),
        format!(
            "ALTER ROLE {role} SET idle_in_transaction_session_timeout = {}",
            quote_literal(IDLE_TRANSACTION_TIMEOUT)
        ),
    ]
}

enum Action {
    Create,
    Replace,
    Unchanged,
}

/// What the database holds under a view's name.
struct Existing {
    relkind: String,
    columns: Vec<(String, String)>,
    comment: Option<String>,
}

async fn existing_view(tx: &Transaction<'_>, name: &str) -> Result<Option<Existing>, BiError> {
    let rows = tx
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), c.relkind::text, \
             obj_description(c.oid, 'pg_class') \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&BI_SCHEMA, &name],
        )
        .await
        .map_err(db)?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let relkind: String = first.get(2);
    let comment: Option<String> = first.get(3);
    let columns = rows
        .iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect();
    Ok(Some(Existing {
        relkind,
        columns,
        comment,
    }))
}

fn decide(existing: Option<&Existing>, view: &ViewPlan) -> Result<Action, BiError> {
    let Some(have) = existing else {
        return Ok(Action::Create);
    };
    if have.relkind != "v" {
        return Err(BiError::new(
            NOT_A_VIEW,
            format!("`{BI_SCHEMA}.{}` exists and is not a view", view.name),
        ));
    }
    let want: Vec<(String, String)> = view
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.pg_type.clone()))
        .collect();
    let same_columns = have.columns == want;
    let same_definition = have.comment.as_deref() == Some(view.comment_text().as_str());
    if same_columns && same_definition {
        return Ok(Action::Unchanged);
    }
    if want.starts_with(&have.columns) {
        return Ok(Action::Replace);
    }
    Err(BiError::new(
        BREAKING_CHANGE,
        format!(
            "`{BI_SCHEMA}.{}` has columns the plan no longer produces (removed, retyped or reordered); raise the entity's version to {} to create a new view and keep this one",
            view.name,
            view.version.saturating_add(1)
        ),
    ))
}

async fn create(tx: &Transaction<'_>, view: &ViewPlan, replace: bool) -> Result<(), BiError> {
    tx.batch_execute(&view.create_sql(replace))
        .await
        .map_err(db)?;
    tx.batch_execute(&view.comment_sql()).await.map_err(db)
}

fn db(error: tokio_postgres::Error) -> BiError {
    match error.code() {
        Some(code) => BiError::new(
            DATABASE,
            format!("the database refused the change (SQLSTATE {})", code.code()),
        ),
        None => BiError::new(DATABASE, "the database connection failed"),
    }
}

pub(crate) fn database(text: &str) -> BiError {
    BiError::new(DATABASE, text)
}
