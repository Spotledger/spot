//! The dedicated `ai` role and its connection-pool budget (WP-AI-05.T1, ADR-26).
//!
//! AI traffic never shares a connection pool with human traffic. The role is created
//! idempotently by a migration helper for `spot-pg-migrate` consumers, and the grant list is
//! generated per tenant schema by [`grants_for_schema`].
//!
//! # The grant list (the plan text)
//!
//! The `ai` role may do exactly this and nothing else:
//!
//! 1. `CONNECT` on the database, so the pool can open a connection.
//! 2. `USAGE` on the tenant schema, so the objects in it are reachable.
//! 3. `SELECT` on the schema's tables (the reporting views and read models the storage layer
//!    exposes).
//! 4. `EXECUTE` on the schema's functions (the query API's functions as defined by the storage
//!    layer).
//! 5. Default `SELECT` on tables created later in the schema.
//! 6. Default `EXECUTE` on functions created later in the schema.
//!
//! The role gets no `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE` or DDL right, and
//! `default_transaction_read_only` is on, so a write fails at the server even if a grant is
//! ever widened by mistake. [`GRANT_LIST`] is the same list as data, so a test can prove the
//! generated statements match it.

use std::time::Duration;

use crate::config::AiPoolSettings;
use crate::error::{AiError, Result};

/// The dedicated AI role's name.
pub const AI_ROLE: &str = "spot_ai";

/// The documented grant list, in the order [`grants_for_schema`] generates it.
pub const GRANT_LIST: [&str; 6] = [
    "CONNECT on the database",
    "USAGE on the schema",
    "SELECT on the schema's tables",
    "EXECUTE on the schema's functions",
    "default SELECT on tables created later",
    "default EXECUTE on functions created later",
];

/// PostgreSQL's identifier limit, in bytes.
const MAX_IDENT_BYTES: usize = 63;

/// The role-creation statements, with the pool budget applied as role-level defaults.
///
/// Idempotent: safe to run on every migration. `role` is a bare identifier, never quoted or
/// interpolated from user text.
///
/// # Errors
///
/// [`crate::error::INVALID_ARGUMENT`] when `role` is not a bare identifier.
pub fn role_creation_sql(role: &str, pool: &AiPoolSettings) -> Result<Vec<String>> {
    validate_ident(role, "role")?;
    let template = include_str!("../sql/ai_role.sql");
    let statements = render(
        template,
        &[
            ("role", role.to_owned()),
            (
                "statement_timeout_ms",
                pool.statement_timeout_ms().to_string(),
            ),
            (
                "idle_in_transaction_timeout_ms",
                pool.idle_in_transaction_timeout_ms().to_string(),
            ),
            ("work_mem_kb", pool.work_mem_kb.to_string()),
        ],
    )?;
    Ok(statements)
}

/// The per-tenant grant statements for the `ai` role, in [`GRANT_LIST`] order.
///
/// # Errors
///
/// [`crate::error::INVALID_ARGUMENT`] when any identifier is not a bare identifier.
pub fn grants_for_schema(role: &str, database: &str, schema: &str) -> Result<Vec<String>> {
    validate_ident(role, "role")?;
    validate_ident(database, "database")?;
    validate_ident(schema, "schema")?;
    let template = include_str!("../sql/ai_grants.sql");
    let statements = render(
        template,
        &[
            ("role", role.to_owned()),
            ("database", database.to_owned()),
            ("schema", schema.to_owned()),
        ],
    )?;
    Ok(statements)
}

/// Every tenant schema's grants, in the order the schemas are given.
///
/// # Errors
///
/// [`crate::error::INVALID_ARGUMENT`] when any identifier is not a bare identifier.
pub fn grants_for_schemas(role: &str, database: &str, schemas: &[&str]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for schema in schemas {
        out.extend(grants_for_schema(role, database, schema)?);
    }
    Ok(out)
}

/// The pool budget's timeouts as a `Duration` pair, for a host that configures its pool
/// library rather than the server.
#[must_use]
pub fn pool_timeouts(pool: &AiPoolSettings) -> (Duration, Duration) {
    (pool.statement_timeout, pool.idle_in_transaction_timeout)
}

/// Splits a compiled-in template into statements and substitutes every `{{name}}` placeholder.
fn render(template: &str, values: &[(&str, String)]) -> Result<Vec<String>> {
    for (name, _) in values {
        let placeholder = format!("{{{{{name}}}}}");
        if !template.contains(&placeholder) {
            return Err(AiError::invalid_argument(format!(
                "the SQL template has no placeholder for {name}"
            )));
        }
    }
    let mut out = Vec::new();
    for raw in template.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let mut statement = line.to_owned();
        for (name, value) in values {
            statement = statement.replace(&format!("{{{{{name}}}}}"), value);
        }
        out.push(statement);
    }
    Ok(out)
}

/// Whether `name` is a bare identifier: `[a-z_][a-z0-9_]*`, at most 63 bytes.
fn validate_ident(name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_IDENT_BYTES {
        return Err(AiError::invalid_argument(format!(
            "the {what} must be 1 to {MAX_IDENT_BYTES} bytes"
        )));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('_');
    if !(first.is_ascii_lowercase() || first == '_') {
        return Err(AiError::invalid_argument(format!(
            "the {what} must start with a lower-case letter or underscore"
        )));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err(AiError::invalid_argument(format!(
            "the {what} must be lower-case letters, digits and underscores only"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_role_sql_is_idempotent_and_applies_the_budget() {
        let pool = AiPoolSettings::default();
        let statements = role_creation_sql(AI_ROLE, &pool).expect("the role is a bare identifier");
        assert_eq!(statements.len(), 6);
        assert!(statements[0].contains("NOT EXISTS"));
        assert!(statements[0].contains(AI_ROLE));
        assert!(statements[1].contains("30000ms"));
        assert!(statements[2].contains("60000ms"));
        assert!(statements[3].contains("65536kB"));
        assert!(statements[4].contains("default_transaction_read_only"));
        assert!(statements[5].contains("max_parallel_workers_per_gather = 0"));
    }

    #[test]
    fn the_grant_set_matches_the_documented_plan() {
        let statements =
            grants_for_schema(AI_ROLE, "spot", "acme").expect("the identifiers are bare");
        assert_eq!(statements.len(), GRANT_LIST.len());
        for statement in &statements {
            assert!(statement.contains(AI_ROLE));
            assert!(
                statement.starts_with("GRANT ") || statement.starts_with("ALTER "),
                "unexpected statement shape: {statement}"
            );
        }
        assert!(statements[0].contains("CONNECT"));
        assert!(statements[0].contains("spot"));
        // Every statement after the database CONNECT is scoped to the tenant schema.
        assert!(statements[1..].iter().all(|s| s.contains("acme")));
        assert!(statements[2].contains("TABLES"));
        assert!(statements[3].contains("FUNCTIONS"));
    }

    #[test]
    fn every_schema_is_granted_in_order() {
        let statements = grants_for_schemas(AI_ROLE, "spot", &["acme", "globex"])
            .expect("the identifiers are bare");
        assert_eq!(statements.len(), GRANT_LIST.len() * 2);
        assert!(statements[1].contains("acme"));
        assert!(statements[GRANT_LIST.len()].contains("spot"));
        assert!(statements[GRANT_LIST.len() + 1].contains("globex"));
    }

    #[test]
    fn a_quoted_or_upper_case_identifier_is_refused() {
        for bad in ["Acme", "acme\"", "acme; DROP", "", "a-b"] {
            assert_eq!(
                grants_for_schema(AI_ROLE, "spot", bad)
                    .err()
                    .map(|e| e.code),
                Some(crate::error::INVALID_ARGUMENT),
                "{bad} should be refused"
            );
        }
    }
}
