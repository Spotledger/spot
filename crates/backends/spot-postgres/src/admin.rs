//! Additive database provisioning for the product host; never modifies an existing database.

use spot_storage_contract::{ErrorKind, StorageError, StorageResult};

use crate::PgConfig;
use crate::conn::{TlsMode, connect_raw};
use crate::error::map_pg_error;

fn identifier(name: &str) -> StorageResult<String> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(StorageError::new(
            ErrorKind::InvalidValue,
            "database name must contain 1 to 63 lowercase ASCII letters, digits or underscores",
        ));
    }
    Ok(format!("\"{name}\""))
}

/// Whether the validated database name already exists on the admin server.
///
/// Invalid names fail before opening a connection. Names are bound as values in the lookup.
pub async fn database_exists(admin: &PgConfig, name: &str) -> StorageResult<bool> {
    identifier(name)?;
    let client = connect_raw(admin, &TlsMode::new(admin.tls)?).await?;
    let row = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .map_err(|e| map_pg_error(&e, None))?;
    row.try_get(0).map_err(|e| map_pg_error(&e, None))
}

/// Creates a UTF-8, ICU database from template0, refusing duplicates without alteration.
///
/// PostgreSQL does not accept bind parameters for this utility statement. Both tokens are
/// validated before quoting; no caller-controlled quote, escape or statement delimiter is legal.
pub async fn create_database(admin: &PgConfig, name: &str, collation: &str) -> StorageResult<()> {
    let name = identifier(name)?;
    if collation.is_empty()
        || !collation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(StorageError::new(
            ErrorKind::InvalidValue,
            "invalid ICU collation",
        ));
    }
    let client = connect_raw(admin, &TlsMode::new(admin.tls)?).await?;
    client.batch_execute(&format!(
        "CREATE DATABASE {name} TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C' LOCALE_PROVIDER icu ICU_LOCALE '{collation}'"
    )).await.map_err(|e| map_pg_error(&e, None))
}
