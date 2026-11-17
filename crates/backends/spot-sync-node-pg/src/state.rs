//! [`PgNodeState`]: the node state that must survive a restart besides the ledger, the parked
//! bundles and the checkpoint: the hold set and the freshness horizons.
//!
//! * **Holds.** The tenant's `HoldSet` (records the node cannot decode, and the pending-release
//!   queue) is stored in its WP-SYN-08.T4 golden form: sorted `(entity, id, value)` sequences.
//!   Loading runs the hold set's own checks, and a damaged, disordered or over-limit value is
//!   refused whole (`sync_node_pg.state_invalid`), never served in part.
//! * **Horizons.** The class horizons the hub last confirmed. They are stored for display and for
//!   the handshake; they do **not** make a restarted node fresh: the freshness tracker counts
//!   monotonic milliseconds that restart with the process, so after a restart every class is
//!   stale until the hub confirms it again (the safe direction).
//! * **Applied prefix.** Never stored here: [`PgNodeState::applied_prefix`] is recomputed from the
//!   ledger, the parked bundles and the repair list at the moment it is asked, so no cached
//!   column can be ahead of the ledger.

use std::collections::BTreeMap;
use std::time::Duration;

use spot_sync_closure::Position;
use spot_sync_skew::HoldSet;

use crate::db::{Db, valid_tenant};
use crate::error::{OpError, PgNodeError, map};
use crate::ledger::PgLedger;

/// Deadline of a state call.
pub const STATE_DEADLINE: Duration = Duration::from_secs(15);
/// The largest stored value, in bytes (the schema bounds it too).
pub const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) const LOAD: &str =
    "SELECT value FROM spot_sync_node_state WHERE tenant = $1 AND kind = $2";
pub(crate) const SAVE: &str = "INSERT INTO spot_sync_node_state (tenant, kind, value) VALUES ($1, $2, $3) \
    ON CONFLICT (tenant, kind) DO UPDATE SET value = EXCLUDED.value, updated_at = now()";

const HOLDS: &str = "holds";
const HORIZONS: &str = "horizons";

/// The tenant state store.
#[derive(Clone, Debug)]
pub struct PgNodeState {
    db: Db,
    ledger: PgLedger,
}

impl PgNodeState {
    /// A state store on `db`.
    #[must_use]
    pub fn new(db: Db) -> Self {
        let ledger = PgLedger::new(db.clone());
        Self { db, ledger }
    }

    fn save(&self, tenant: &str, kind: &'static str, text: String) -> Result<(), PgNodeError> {
        if !valid_tenant(tenant) || text.len() > MAX_STATE_BYTES {
            return Err(PgNodeError::StateInvalid);
        }
        let tenant = tenant.to_owned();
        self.db
            .run(STATE_DEADLINE, move |conn| {
                Box::pin(async move {
                    conn.execute(SAVE, &[&tenant, &kind, &text]).await?;
                    Ok(())
                })
            })
            .map_err(map)
    }

    fn load(&self, tenant: &str, kind: &'static str) -> Result<Option<String>, PgNodeError> {
        let tenant = tenant.to_owned();
        self.db
            .run(STATE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_opt(LOAD, &[&tenant, &kind]).await?;
                    Ok::<_, OpError>(row.map(|r| r.get::<_, String>(0)))
                })
            })
            .map_err(map)
    }

    /// Saves the tenant's hold set in its golden JSON form.
    ///
    /// # Errors
    /// `sync_node_pg.state_invalid` for an invalid tenant or a set past the size bound; the
    /// store's failure.
    pub fn save_holds(&self, tenant: &str, holds: &HoldSet) -> Result<(), PgNodeError> {
        let text = serde_json::to_string(holds).map_err(|_| PgNodeError::StateInvalid)?;
        self.save(tenant, HOLDS, text)
    }

    /// The tenant's saved hold set, or `None` when none was saved.
    ///
    /// # Errors
    /// `sync_node_pg.state_invalid` when the stored value is damaged or breaks the hold set's
    /// rules (nothing is returned in part); the store's failure.
    pub fn load_holds(&self, tenant: &str) -> Result<Option<HoldSet>, PgNodeError> {
        match self.load(tenant, HOLDS)? {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|_| PgNodeError::StateInvalid),
        }
    }

    /// Saves the class horizons the hub last confirmed for the tenant.
    ///
    /// # Errors
    /// As [`save_holds`](Self::save_holds).
    pub fn save_horizons(
        &self,
        tenant: &str,
        horizons: &BTreeMap<String, u64>,
    ) -> Result<(), PgNodeError> {
        let text = serde_json::to_string(horizons).map_err(|_| PgNodeError::StateInvalid)?;
        self.save(tenant, HORIZONS, text)
    }

    /// The tenant's saved horizons (empty when none were saved).
    ///
    /// # Errors
    /// `sync_node_pg.state_invalid` for a damaged value; the store's failure.
    pub fn load_horizons(&self, tenant: &str) -> Result<BTreeMap<String, u64>, PgNodeError> {
        match self.load(tenant, HORIZONS)? {
            None => Ok(BTreeMap::new()),
            Some(text) => serde_json::from_str(&text).map_err(|_| PgNodeError::StateInvalid),
        }
    }

    /// The applied journal prefix of `source` in `tenant`, recomputed from the ledger now.
    ///
    /// # Errors
    /// The store's failure.
    pub fn applied_prefix(
        &self,
        tenant: &str,
        source: &str,
    ) -> Result<Option<Position>, PgNodeError> {
        self.ledger.applied_prefix(tenant, source)
    }
}
