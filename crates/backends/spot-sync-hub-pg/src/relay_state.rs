//! [`PgRelayStore`]: what the relay must remember across a restart (`RelayState`), kept as one row
//! per tenant for the cursor and the stream facts, and one row per destination for its holds,
//! extras, backlog and acknowledged prefix.
//!
//! The destination snapshot (`DestSnapshot`) is stored in its serde JSON form. Its hold set
//! serialises as sorted `(entity, id, value)` sequences (WP-SYN-08.T4), so the text is stable,
//! diffable and restorable; loading it back runs the hold set's own checks, and a damaged,
//! disordered or over-limit snapshot is refused as a whole with `sync_hub_pg.state_invalid`
//! rather than served in part. The save of a whole `RelayState` is one transaction: a crash
//! leaves the previous state or the new one, never a mix. A destination row's `last_serve_seq`
//! (the queue's number) is not touched by a state save.

use std::collections::BTreeMap;

use spot_sync_hub::{DestSnapshot, RelayState};
use spot_sync_obligations::Lane;

use crate::db::{Db, MAX_NAME};
use crate::error::{OpError, PgHubError, map};

/// Deadline of a state save or load.
pub const STATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
/// The largest destination snapshot stored, in bytes (the schema bounds it too).
pub const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

pub(crate) const LOAD_RELAY: &str = "SELECT cursor, last_txn, lane_of_entity FROM spot_sync_hub_relay \
    WHERE tenant = $1";
pub(crate) const SAVE_RELAY: &str = "INSERT INTO spot_sync_hub_relay (tenant, cursor, last_txn, lane_of_entity) \
    VALUES ($1, $2, $3, $4) ON CONFLICT (tenant) DO UPDATE SET cursor = EXCLUDED.cursor, \
    last_txn = EXCLUDED.last_txn, lane_of_entity = EXCLUDED.lane_of_entity, updated_at = now()";
pub(crate) const LOAD_DESTS: &str = "SELECT destination, snapshot FROM spot_sync_hub_dest_state \
    WHERE tenant = $1 AND snapshot IS NOT NULL ORDER BY destination";
pub(crate) const SAVE_DEST: &str = "INSERT INTO spot_sync_hub_dest_state (tenant, destination, snapshot) \
    VALUES ($1, $2, $3) ON CONFLICT (tenant, destination) DO UPDATE SET snapshot = EXCLUDED.snapshot, \
    updated_at = now()";
pub(crate) const LIST_SNAPSHOTS: &str = "SELECT destination FROM spot_sync_hub_dest_state \
    WHERE tenant = $1 AND snapshot IS NOT NULL";
pub(crate) const DROP_DEST: &str = "UPDATE spot_sync_hub_dest_state SET snapshot = NULL, updated_at = now() \
    WHERE tenant = $1 AND destination = $2 AND NOT ($2 = ANY($3::text[]))";

/// The relay state store of one tenant.
#[derive(Clone, Debug)]
pub struct PgRelayStore {
    db: Db,
}

impl PgRelayStore {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// Saves the whole state in one transaction: the cursor and stream facts, every destination
    /// snapshot, and clears the snapshot of a destination no longer in the state.
    ///
    /// # Errors
    /// `sync_hub_pg.state_invalid` for a name or snapshot beyond its bound; the store's failure.
    pub fn save(&self, state: &RelayState) -> Result<(), PgHubError> {
        let cursor = i64::try_from(state.cursor).map_err(|_| PgHubError::StateInvalid)?;
        let last_txn = state.last_txn.clone();
        let lanes =
            serde_json::to_string(&state.lane_of_entity).map_err(|_| PgHubError::StateInvalid)?;
        let mut dests: Vec<(String, String)> = Vec::new();
        for (dest, snapshot) in &state.destinations {
            if dest.is_empty() || dest.len() > MAX_NAME {
                return Err(PgHubError::StateInvalid);
            }
            let text = serde_json::to_string(snapshot).map_err(|_| PgHubError::StateInvalid)?;
            if text.len() > MAX_SNAPSHOT_BYTES {
                return Err(PgHubError::StateInvalid);
            }
            dests.push((dest.clone(), text));
        }
        let tenant = self.db.tenant.clone();
        self.db
            .run(STATE_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    tx.execute(SAVE_RELAY, &[&tenant, &cursor, &last_txn, &lanes])
                        .await?;
                    let mut kept: Vec<String> = Vec::with_capacity(dests.len());
                    for (dest, text) in &dests {
                        tx.execute(SAVE_DEST, &[&tenant, dest, text]).await?;
                        kept.push(dest.clone());
                    }
                    // A destination the state no longer has loses its snapshot (its queue and
                    // serve sequence stay: they are the queue's).
                    let gone = tx.query(LIST_SNAPSHOTS, &[&tenant]).await?;
                    for row in gone {
                        let dest: String = row.get(0);
                        if !kept.contains(&dest) {
                            tx.execute(DROP_DEST, &[&tenant, &dest, &kept]).await?;
                        }
                    }
                    tx.commit().await?;
                    Ok(())
                })
            })
            .map_err(map)
    }

    /// Loads the saved state, or `None` when none was saved.
    ///
    /// # Errors
    /// `sync_hub_pg.state_invalid` when a stored value is damaged or breaks the hold set's rules
    /// (nothing is returned in part); the store's failure.
    pub fn load(&self) -> Result<Option<RelayState>, PgHubError> {
        let tenant = self.db.tenant.clone();
        let loaded = self
            .db
            .run(STATE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let Some(core) = conn.query_opt(LOAD_RELAY, &[&tenant]).await? else {
                        return Ok(None);
                    };
                    let cursor =
                        u64::try_from(core.get::<_, i64>(0)).map_err(|_| OpError::Decode)?;
                    let last_txn: Option<String> = core.get(1);
                    let lanes: String = core.get(2);
                    let lane_of_entity: BTreeMap<String, Lane> =
                        serde_json::from_str(&lanes).map_err(|_| OpError::Decode)?;
                    let mut destinations = BTreeMap::new();
                    for row in conn.query(LOAD_DESTS, &[&tenant]).await? {
                        let dest: String = row.get(0);
                        let text: String = row.get(1);
                        let snapshot: DestSnapshot =
                            serde_json::from_str(&text).map_err(|_| OpError::Decode)?;
                        destinations.insert(dest, snapshot);
                    }
                    Ok(Some(RelayState {
                        cursor,
                        last_txn,
                        lane_of_entity,
                        destinations,
                    }))
                })
            })
            .map_err(map)?;
        Ok(loaded)
    }
}
