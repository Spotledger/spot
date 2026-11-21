//! Connection-string handling shared by the providers: splitting a `postgres://` URL
//! into a credential-free keyword string for command lines and child-env pairs for the
//! password, and raw driver connections for the fact reads and database renames.

use std::str::FromStr;

use crate::error::{SnapshotError, SnapshotResult};
use crate::provider::ChildEnv;

/// One connection split into what the command line may hold and what only the child's
/// environment may hold.
#[derive(Debug, Clone)]
pub(crate) struct SplitConnection {
    /// The credential-free keyword connection string (for the command line).
    pub keyword: String,
    /// The child-env pairs (the password under `PGPASSWORD`).
    pub env: ChildEnv,
    /// The connection's database name.
    pub database: String,
}

/// Splits and validates a `postgres://` connection string.
///
/// # Errors
/// `snapshot.io` when the string does not parse, names no host or no database.
pub(crate) fn split_connection(connection: &str) -> SnapshotResult<SplitConnection> {
    let (keyword, env) = ChildEnv::from_connection(connection)?;
    let parsed = tokio_postgres::Config::from_str(connection)
        .map_err(|e| SnapshotError::io(format!("the connection string does not parse: {e}")))?;
    let database = parsed
        .get_dbname()
        .map(str::to_owned)
        .ok_or_else(|| SnapshotError::io("the connection string names no database"))?;
    // The TCP-host check keeps the credential-free split honest: Unix sockets are
    // refused, exactly as the storage config refuses them.
    if !parsed
        .get_hosts()
        .iter()
        .any(|h| matches!(h, tokio_postgres::config::Host::Tcp(_)))
    {
        return Err(SnapshotError::io("the connection string names no TCP host"));
    }
    Ok(SplitConnection {
        keyword,
        env,
        database,
    })
}

/// A raw one-off driver connection (no pool: takes hold a transaction open).
pub(crate) struct RawConn {
    /// The driver client.
    pub client: tokio_postgres::Client,
}

/// Connects a raw driver client from a connection string. The password rides the
/// parsed config in-process only; it never reaches a command line.
///
/// # Errors
/// `snapshot.io` on a parse or connect failure.
pub(crate) async fn connect_url(url: &str) -> SnapshotResult<RawConn> {
    let config = spot_postgres::PgConfig::from_url(url)
        .map_err(|e| SnapshotError::io(format!("the connection does not parse: {e}")))?;
    raw_connect(&config).await
}

/// Connects a raw driver client from a config.
///
/// # Errors
/// `snapshot.io` on a connect failure.
pub(crate) async fn raw_connect(config: &spot_postgres::PgConfig) -> SnapshotResult<RawConn> {
    let mut parsed = tokio_postgres::Config::new();
    parsed
        .user(&config.user)
        .dbname(&config.dbname)
        .host(&config.host)
        .port(config.port);
    if !config.password.is_empty() {
        parsed.password(config.password.as_str());
    }
    let (client, connection) = parsed
        .connect(tokio_postgres::NoTls)
        .await
        .map_err(|e| SnapshotError::io(format!("cannot connect: {e}")))?;
    // The connection task outlives the client; it ends on its own when the session
    // closes (drop of the client). No handle is needed.
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(RawConn { client })
}

/// Quotes an identifier after validating it (lowercase, digits, underscores, <=63
/// bytes). Database names come from callers; this keeps the SQL safe (ADR-45).
///
/// # Errors
/// `snapshot.io` naming the refused name.
pub(crate) fn quote_ident(name: &str) -> SnapshotResult<String> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(SnapshotError::io(format!(
            "the database name {name:?} is not a safe identifier"
        )));
    }
    Ok(format!("\"{name}\""))
}

/// The clock's unix milliseconds, integer (no floats anywhere).
pub(crate) fn unix_ms(clock: &std::sync::Arc<dyn spot_core::Clock>) -> u64 {
    let seconds = clock.now_utc().unix_timestamp();
    u64::try_from(seconds).unwrap_or(0).saturating_mul(1000)
}
