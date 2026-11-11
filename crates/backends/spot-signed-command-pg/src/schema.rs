//! The DDL of the nonce table and the idempotent `prepare`.

use deadpool_postgres::Pool;

use crate::error::SetupError;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x0073_6e63_0000_0001;

/// The idempotent DDL. `key_id` and `nonce` are the pair a command is replay-checked by; the
/// index on `expires_at` serves [`crate::PgNonceStore::prune`].
pub const DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_signed_command_nonce (
    key_id     bytea  NOT NULL,
    nonce      bytea  NOT NULL CHECK (octet_length(nonce) = 16),
    expires_at bigint NOT NULL,
    PRIMARY KEY (key_id, nonce)
);
CREATE INDEX IF NOT EXISTS spot_signed_command_nonce_expiry ON spot_signed_command_nonce (expires_at);
";

/// Creates the table and index; safe to call again, from several nodes at once.
pub(crate) async fn prepare(pool: &Pool) -> Result<(), SetupError> {
    let mut conn = pool.get().await.map_err(|_| SetupError::pool())?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| SetupError::statement(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| SetupError::statement(&e))?;
    tx.batch_execute(DDL)
        .await
        .map_err(|e| SetupError::statement(&e))?;
    tx.commit().await.map_err(|e| SetupError::statement(&e))
}
