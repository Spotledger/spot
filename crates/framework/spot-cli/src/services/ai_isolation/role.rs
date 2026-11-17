//! The `ai` database role and its pool: created idempotently at start, never at request time.
//!
//! The role is made with `spot-ai-isolation`'s own statements (no DDL, row-limited, read-only by
//! default, `statement_timeout`, `idle_in_transaction_session_timeout`, parallel workers off), given
//! only the grants of its list for the site's schema, and enabled for login with the password the
//! operator keeps in `services.ai_isolation.password_file`. The pool that connects as it is a second
//! [`PgBackend`] with its own small `max_connections`; the main pool is never used for an AI call.

use std::path::Path;

use spot_ai_isolation::{AI_ROLE, AiPoolSettings, grants_for_schema, role_creation_sql};
use spot_postgres::{PgBackend, PgConfig};

/// The longest and shortest password the file may hold, and the characters it may use. The
/// password is placed in an `ALTER ROLE` statement, which cannot take a bound parameter, so only a
/// set that needs no escaping is accepted.
const PASSWORD_MIN: usize = 16;
const PASSWORD_MAX: usize = 128;

/// Reads and validates the password file.
///
/// # Errors
///
/// A fixed sentence (never the content) when the file is missing, unreadable, too short or long, or
/// holds a character outside `[A-Za-z0-9._~-]`.
pub fn read_password(path: &Path) -> Result<String, String> {
    let text =
        std::fs::read_to_string(path).map_err(|_| "the password file cannot be read".to_owned())?;
    let password = text.trim().to_owned();
    check_password(&password)?;
    Ok(password)
}

/// The password rule, separate from the file read so it is a unit test.
pub fn check_password(password: &str) -> Result<(), String> {
    if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&password.len()) {
        return Err(format!(
            "the password must be {PASSWORD_MIN} to {PASSWORD_MAX} characters"
        ));
    }
    if !password
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'))
    {
        return Err("the password may use only letters, digits, `.`, `_`, `~` and `-`".to_owned());
    }
    Ok(())
}

/// The role's connection limit. A role is cluster-wide while a pool is per site, so the limit leaves
/// room for several sites and a rolling restart: eight times the pool plus eight.
#[must_use]
pub fn connection_limit(pool: &AiPoolSettings) -> u32 {
    pool.pool_size.saturating_mul(8).saturating_add(8)
}

/// Every statement that provisions the role for `database`.`schema`, in order, with the login
/// statement last. The password is already validated.
///
/// # Errors
///
/// The statement builder's refusal, as a fixed sentence.
pub fn provisioning_sql(
    pool: &AiPoolSettings,
    database: &str,
    schema: &str,
    password: &str,
) -> Result<Vec<String>, String> {
    check_password(password)?;
    let mut statements = role_creation_sql(AI_ROLE, pool)
        .map_err(|e| format!("the role statements were refused: {}", e.code))?;
    statements.extend(
        grants_for_schema(AI_ROLE, database, schema)
            .map_err(|e| format!("the grant statements were refused: {}", e.code))?,
    );
    statements.push(format!(
        "ALTER ROLE {AI_ROLE} WITH LOGIN PASSWORD '{password}' CONNECTION LIMIT {}",
        connection_limit(pool)
    ));
    Ok(statements)
}

/// Runs the provisioning statements on the main pool.
///
/// # Errors
///
/// A fixed sentence when the database refuses (typically a missing privilege to create roles).
pub async fn provision(backend: &PgBackend, statements: &[String]) -> Result<(), String> {
    let client = backend
        .pool()
        .get()
        .await
        .map_err(|_| "the database is not available".to_owned())?;
    for statement in statements {
        client
            .batch_execute(statement)
            .await
            .map_err(|_| "the database refused the ai role statements".to_owned())?;
    }
    Ok(())
}

/// The config of the `ai` pool: the site's own, logging in as the role, with the small pool.
#[must_use]
pub fn ai_config(site: &PgConfig, password: &str, pool: &AiPoolSettings) -> PgConfig {
    let mut config = site.clone();
    config.user = AI_ROLE.to_owned();
    config.password = password.to_owned();
    config.application_name = "spot-ai".to_owned();
    config.pool_max_size = usize::try_from(pool.pool_size).unwrap_or(4);
    config
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const GOOD: &str = "correct-horse.battery_staple~42";

    #[test]
    fn a_password_must_be_long_enough_and_plain() {
        assert!(check_password(GOOD).is_ok());
        assert!(check_password("short").is_err());
        assert!(check_password(&"a".repeat(129)).is_err());
        assert!(check_password("has'quote-and-more-chars").is_err());
        assert!(check_password("has space and more chars").is_err());
        assert!(check_password("semi;colon-and-more-chars").is_err());
    }

    #[test]
    fn provisioning_creates_grants_and_enables_login_last() {
        let pool = AiPoolSettings::default();
        let sql = provisioning_sql(&pool, "site_db", "public", GOOD).unwrap();
        assert!(sql.first().unwrap().contains("CREATE ROLE"));
        assert!(sql.iter().any(|s| s.contains("GRANT SELECT ON ALL TABLES")));
        assert!(
            sql.iter()
                .any(|s| s.contains("default_transaction_read_only"))
        );
        let last = sql.last().unwrap();
        assert!(last.starts_with("ALTER ROLE spot_ai WITH LOGIN PASSWORD"));
        assert!(last.ends_with("CONNECTION LIMIT 40"));
        // Never a write grant.
        for s in &sql {
            let upper = s.to_ascii_uppercase();
            assert!(!upper.contains("GRANT INSERT") && !upper.contains("GRANT ALL"));
        }
    }

    #[test]
    fn a_bad_password_provisions_nothing() {
        assert!(provisioning_sql(&AiPoolSettings::default(), "d", "public", "bad").is_err());
    }

    #[test]
    fn the_ai_config_logs_in_as_the_role_with_the_small_pool() {
        let site = PgConfig::new("localhost", 5432, "site_db", "owner", "secret");
        let config = ai_config(&site, GOOD, &AiPoolSettings::default());
        assert_eq!(config.user, "spot_ai");
        assert_eq!(config.dbname, "site_db");
        assert_eq!(config.pool_max_size, 4);
        assert_eq!(config.application_name, "spot-ai");
    }
}
