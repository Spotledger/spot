//! The table: `spot_promotion` in the control schema, created idempotently.
//!
//! | Column | Holds |
//! | --- | --- |
//! | `tenant` | the target environment's tenant id (text, leads the primary key) |
//! | `id` | the promotion id (a run id: at most 256 characters) |
//! | `revision` | the optimistic revision: the promotion's own, +1 on every save (`bigint`, at least 0) |
//! | `state` | the promotion's state name, kept beside the document for operators reading `psql` |
//! | `doc` | the promotion as a versioned `jsonb` snapshot |
//!
//! No extension is needed. The statements name the table with its schema, so nothing depends on,
//! or changes, the session's `search_path`.

use spot_pg_bridge::Bridge;
use spot_promotion::PromotionError;

use crate::error::{cause, store_note};

/// The control schema the table lives in (shared with the tenant registry).
pub const CONTROL_SCHEMA: &str = "spot_control";

/// The table.
pub const TABLE: &str = "spot_promotion";

/// The advisory lock that serialises concurrent schema preparation (transaction-scoped).
const SCHEMA_LOCK: i64 = 0x0050_524f_4d4f_5401;

/// The idempotent DDL. Every name starts with `spot_`; the constraints are named.
pub const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.spot_promotion (\
    tenant text NOT NULL, \
    id text NOT NULL, \
    revision bigint NOT NULL, \
    state text NOT NULL, \
    doc jsonb NOT NULL, \
    CONSTRAINT spot_promotion_pkey PRIMARY KEY (tenant, id), \
    CONSTRAINT spot_promotion_revision_check CHECK (revision >= 0), \
    CONSTRAINT spot_promotion_id_check CHECK (char_length(id) BETWEEN 1 AND 256))";

/// Creates the schema and table if they are missing, in one transaction under an advisory lock.
/// Safe to call from several processes at once and on every start.
///
/// # Errors
/// `promotion.store_failed` carrying the bridge code or the SQLSTATE.
pub(crate) fn ensure(bridge: &Bridge, deadline: std::time::Duration) -> Result<(), PromotionError> {
    bridge
        .run(deadline, |mut conn| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
                    .await?;
                tx.batch_execute(DDL).await?;
                tx.commit().await
            })
        })
        .map_err(|e| {
            store_note(&format!(
                "the promotion table could not be prepared ({})",
                cause(&e)
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_carries_the_spot_prefix() {
        assert!(TABLE.starts_with("spot_"));
        assert!(DDL.contains("spot_control.spot_promotion"));
        assert!(DDL.contains("CONSTRAINT spot_promotion_pkey"));
    }
}
