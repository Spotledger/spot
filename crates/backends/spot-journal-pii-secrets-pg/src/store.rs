//! [`PgWrappedKeyStore`]: the `WrappedKeyStore` over the table of [`crate::schema`].

use std::fmt;
use std::sync::Arc;

use spot_journal_pii_secrets::{PortError, StoredKey, WrappedKeyStore};
use spot_pg_bridge::Bridge;
use spot_secrets::{KeyRef, WrappedKey};

use crate::error::{row_invalid, store_failed};

const GET: &str = "SELECT state, key_id, key_version, wrapped \
                   FROM spot_journal_subject_key WHERE entry = $1";

const PUT: &str = "INSERT INTO spot_journal_subject_key \
                   (entry, state, key_id, key_version, wrapped) \
                   VALUES ($1, 'live', $2, $3, $4) ON CONFLICT (entry) DO NOTHING";

/// A live row becomes a tombstone in one statement; the wrapped bytes and the key reference are
/// nulled in the same row version, so nothing of the key stays in the table.
const TOMBSTONE_LIVE: &str = "UPDATE spot_journal_subject_key \
                              SET state = 'destroyed', key_id = NULL, key_version = NULL, \
                                  wrapped = NULL, destroyed_at = now() \
                              WHERE entry = $1 AND state = 'live'";

const TOMBSTONE_NEW: &str = "INSERT INTO spot_journal_subject_key (entry, state, destroyed_at) \
                             VALUES ($1, 'destroyed', now()) ON CONFLICT (entry) DO NOTHING";

/// The wrapped-key store over PostgreSQL, for one tenant database.
///
/// A host builds one per tenant database, shares the bridge, and passes the store to
/// `ProviderSubjectKeys`. Run the migrator with [`crate::extra_table`] and call [`crate::ensure_present`] once at start-up first.
#[derive(Clone)]
pub struct PgWrappedKeyStore {
    bridge: Arc<Bridge>,
}

impl PgWrappedKeyStore {
    /// A store that reaches the database through `bridge` (the process's shared bridge).
    #[must_use]
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

impl fmt::Debug for PgWrappedKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgWrappedKeyStore").finish_non_exhaustive()
    }
}

type Row = (String, Option<String>, Option<i64>, Option<Vec<u8>>);

fn decode(row: Row) -> Result<StoredKey, PortError> {
    match row {
        (state, None, None, None) if state == "destroyed" => Ok(StoredKey::Destroyed),
        (state, Some(id), Some(version), Some(bytes)) if state == "live" => {
            let version = u32::try_from(version).map_err(|_| row_invalid())?;
            Ok(StoredKey::Live(WrappedKey {
                key: KeyRef { id, version },
                bytes,
            }))
        }
        _ => Err(row_invalid()),
    }
}

impl WrappedKeyStore for PgWrappedKeyStore {
    fn get(&self, entry: &str) -> Result<Option<StoredKey>, PortError> {
        let entry = entry.to_owned();
        let row = self
            .bridge
            .run_default(move |conn| {
                Box::pin(async move {
                    let row = conn.query_opt(GET, &[&entry]).await?;
                    row.map(|r| {
                        Ok::<Row, tokio_postgres::Error>((
                            r.try_get(0)?,
                            r.try_get(1)?,
                            r.try_get(2)?,
                            r.try_get(3)?,
                        ))
                    })
                    .transpose()
                })
            })
            .map_err(store_failed)?;
        row.map(decode).transpose()
    }

    fn put_if_absent(&self, entry: &str, wrapped: WrappedKey) -> Result<bool, PortError> {
        let entry = entry.to_owned();
        let version = i64::from(wrapped.key.version);
        let inserted = self
            .bridge
            .run_default(move |conn| {
                Box::pin(async move {
                    conn.execute(PUT, &[&entry, &wrapped.key.id, &version, &wrapped.bytes])
                        .await
                })
            })
            .map_err(store_failed)?;
        Ok(inserted == 1)
    }

    fn destroy(&self, entry: &str) -> Result<bool, PortError> {
        let entry = entry.to_owned();
        self.bridge
            .run_default(move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    let mut removed = tx.execute(TOMBSTONE_LIVE, &[&entry]).await? == 1;
                    if !removed && tx.execute(TOMBSTONE_NEW, &[&entry]).await? == 0 {
                        // The row exists: a tombstone already, or a key a concurrent writer
                        // stored after the first statement looked. Rows are never deleted and
                        // never go back to live, so this second look settles it.
                        removed = tx.execute(TOMBSTONE_LIVE, &[&entry]).await? == 1;
                    }
                    tx.commit().await?;
                    Ok::<bool, tokio_postgres::Error>(removed)
                })
            })
            .map_err(store_failed)
    }
}
