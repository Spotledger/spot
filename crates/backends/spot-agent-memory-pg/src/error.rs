//! Turning database, pool and provisioning failures into this crate's two vocabularies.
//!
//! There are two seams because there are two audiences. A read or a write through
//! [`MemoryStore`](spot_agent_memory::MemoryStore) is answered with
//! `MemoryError::Store { detail }`, whose detail names what was refused and **never** a value, an
//! id, a database name, a password or driver text: the driver's own message can quote a JSON
//! document, a connection string or a constraint's detail. A schema migration or a provisioning
//! step is a control-plane operation and is answered with an `AppError` carrying one of this
//! crate's three `agent_memory_pg.*` codes.
//!
//! The three codes are deliberately few: a migration that cannot run, a server without the
//! `vector` extension (the one condition the crate reports as a *skippable* state rather than a
//! failure, so a CI image without pgvector is visible instead of silently green), and a
//! provisioning step that failed, naming the step and nothing else.

use deadpool_postgres::{PoolError, TimeoutType};
use spot_agent_memory::MemoryError;
use spot_app_api::{AppError, ErrorClass};

/// The memory tables could not be created in the memory database.
pub const MIGRATION_FAILED: &str = "agent_memory_pg.migration_failed";

/// The server does not have the `vector` extension available, so a memory database cannot be
/// prepared on it. The skills tests report this as `SKIPPED`, never as a pass.
pub const EXTENSION_MISSING: &str = "agent_memory_pg.extension_missing";

/// One provisioning step failed: creating the database, the role, its grants, the extension or
/// the control-store record.
pub const PROVISION_FAILED: &str = "agent_memory_pg.provision_failed";

/// The SQLSTATE of a driver error, or `none` for a connection-level failure.
fn sqlstate(error: &tokio_postgres::Error) -> &str {
    error.code().map_or("none", |state| state.code())
}

/// A database failure while running a statement, as a store refusal with a short cause code.
pub(crate) fn store(error: &tokio_postgres::Error) -> MemoryError {
    MemoryError::Store {
        detail: format!(
            "the memory database refused a statement ({})",
            cause_for(sqlstate(error))
        ),
    }
}

/// A pool failure (no connection could be checked out).
pub(crate) fn store_pool(error: &PoolError) -> MemoryError {
    MemoryError::Store {
        detail: format!(
            "the memory database is unavailable ({})",
            match error {
                PoolError::Timeout(TimeoutType::Wait) => "pool_exhausted",
                PoolError::Timeout(_) => "timeout",
                PoolError::Backend(_) | PoolError::PostCreateHook(_) => "connection_lost",
                PoolError::Closed | PoolError::NoRuntimeSpecified => "pool_closed",
            }
        ),
    }
}

/// A stable short cause for a SQLSTATE, chosen from the class of the failure only.
fn cause_for(state: &str) -> &'static str {
    match state {
        "none" => "connection_lost",
        "40P01" | "40001" => "conflict",
        "57014" => "timeout",
        "42501" => "permission_denied",
        "23505" => "duplicate",
        "23514" => "constraint",
        "22P02" | "22023" => "bad_value",
        other if other.starts_with("08") || other.starts_with("57P") => "connection_lost",
        other if other.starts_with("42") => "statement_refused",
        other if other.starts_with("23") => "constraint",
        _ => "statement_failed",
    }
}

/// A failed schema migration, as the `agent_memory_pg.migration_failed` code.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the agent memory tables could not be created (SQLSTATE {})",
            sqlstate(&error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the agent memory tables could not be created: no connection".to_owned(),
    )
}

/// The server has no `vector` extension: the one condition a caller may skip on.
pub(crate) fn extension_missing() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        EXTENSION_MISSING,
        "the PostgreSQL server has no `vector` extension available, so a memory database \
         cannot be prepared on it"
            .to_owned(),
    )
}

/// The extension exists on the server but could not be created in the memory database.
pub(crate) fn extension_failed(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the `vector` extension could not be created in the memory database (SQLSTATE {})",
            sqlstate(&error)
        ),
    )
}

/// A provisioning step failed. `stage` names the step (`database`, `role`, `grants`,
/// `extension`, `registry`) and nothing else: a database name and a password are the two values
/// a provisioning failure is most likely to be about, and neither may reach a log.
pub(crate) fn provision(stage: &'static str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        PROVISION_FAILED,
        format!("the tenant's memory database could not be provisioned at the {stage} step"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_store_refusal_names_a_cause_and_no_sql() {
        let refused = MemoryError::Store {
            detail: "the memory database refused a statement (permission_denied)".to_owned(),
        };
        assert_eq!(refused.code(), "agent_memory.store_failed");
        let text = refused.to_string();
        assert!(text.contains("permission_denied"), "{text}");
        assert!(!text.contains("SELECT"), "{text}");
    }

    #[test]
    fn pool_failures_map_to_a_cause_code() {
        for (error, cause) in [
            (PoolError::Timeout(TimeoutType::Wait), "pool_exhausted"),
            (PoolError::Timeout(TimeoutType::Create), "timeout"),
            (PoolError::Closed, "pool_closed"),
        ] {
            let mapped = store_pool(&error).to_string();
            assert!(mapped.contains(cause), "{mapped}");
        }
    }

    #[test]
    fn the_three_codes_carry_this_crate_prefix_and_no_value() {
        for error in [
            migration_pool(PoolError::Closed),
            extension_missing(),
            provision("role"),
        ] {
            let code = &error.code;
            assert!(code.starts_with("agent_memory_pg."), "{code}");
            let text = &error.message;
            assert!(!text.contains('$'), "{text}");
            assert!(!text.contains("postgres://"), "{text}");
        }
    }
}
