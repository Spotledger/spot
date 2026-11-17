//! The state slots of the offline identity store on PostgreSQL: [`PgIdentityState`] is the
//! [`SealedStateStore`] under [`SealedPersistence`](spot_offline_identity_adapters::SealedPersistence).
//!
//! `spot_offline_identity_state(tenant, node, slot, bytes, updated_at)`: two rows per tenant and
//! node, `state` and `fence`. The bytes are **sealed** by the persistence (authenticated
//! encryption bound to the tenant, the node and the slot) before they reach this store, so the
//! table holds no readable snapshot and no key. The tenant predicate is in every statement
//! ([`STATEMENTS`]); the table comes from `spot migrate` ([`DDL`]), never from the host.
//!
//! A write is one upsert statement, so a slot is replaced whole. The persistence writes the
//! state and then the fence; a crash between them leaves a state newer than the fence, which the
//! persistence accepts (it raises the fence on load), and never the reverse.

use std::sync::Arc;
use std::time::Duration;

use spot_offline_identity_adapters::{AdapterError, SealedStateStore, Slot};
use spot_pg_bridge::Bridge;

use crate::blocking::{Op, off_runtime};

/// The table's idempotent DDL.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_offline_identity_state (\
    tenant text NOT NULL, \
    node text NOT NULL, \
    slot text NOT NULL CHECK (slot IN ('state', 'fence')), \
    bytes bytea NOT NULL, \
    updated_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, node, slot))";

/// The tables this module owns.
pub const TABLES: [&str; 1] = ["spot_offline_identity_state"];

/// Every statement the module runs, for the tenant-predicate scan.
pub const STATEMENTS: [&str; 3] = [READ, WRITE, MISSING];

const READ: &str = "SELECT bytes FROM spot_offline_identity_state \
    WHERE tenant = $1 AND node = $2 AND slot = $3";
const WRITE: &str = "INSERT INTO spot_offline_identity_state (tenant, node, slot, bytes) \
    VALUES ($1, $2, $3, $4) ON CONFLICT (tenant, node, slot) \
    DO UPDATE SET bytes = EXCLUDED.bytes, updated_at = now()";
const MISSING: &str = "SELECT to_regclass('public.spot_offline_identity_state') IS NOT NULL";

/// Deadline of one bridged call.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// The identity state of one node of one tenant.
pub struct PgIdentityState {
    bridge: Arc<Bridge>,
    tenant: String,
    node: String,
}

impl std::fmt::Debug for PgIdentityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgIdentityState")
            .field("tenant", &self.tenant)
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::State => "state",
        Slot::Fence => "fence",
    }
}

impl PgIdentityState {
    /// The store of `node` of `tenant`. No I/O happens here.
    pub fn new(bridge: Arc<Bridge>, tenant: impl Into<String>, node: impl Into<String>) -> Self {
        Self {
            bridge,
            tenant: tenant.into(),
            node: node.into(),
        }
    }

    /// Fails when the table does not exist (the host says to run `spot migrate`).
    ///
    /// # Errors
    ///
    /// A message for the operator.
    pub fn require_table(&self) -> Result<(), String> {
        let bridge = Arc::clone(&self.bridge);
        let present = off_runtime(move || {
            bridge.run(DEADLINE, |conn| {
                Box::pin(async move {
                    let row = conn.query_one(MISSING, &[]).await?;
                    Ok::<bool, Op>(row.get::<_, bool>(0))
                })
            })
        })
        .ok_or_else(|| "the table check did not finish".to_owned())?
        .map_err(|error| crate::blocking::failure_text(&error))?;
        if present {
            Ok(())
        } else {
            Err("spot_offline_identity_state is missing: run `spot migrate`".to_owned())
        }
    }
}

impl SealedStateStore for PgIdentityState {
    fn read(&self, slot: Slot) -> Result<Option<Vec<u8>>, AdapterError> {
        let (bridge, tenant, node) = (
            Arc::clone(&self.bridge),
            self.tenant.clone(),
            self.node.clone(),
        );
        let slot = slot_name(slot);
        off_runtime(move || {
            bridge.run(DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn.query(READ, &[&tenant, &node, &slot]).await?;
                    Ok::<Option<Vec<u8>>, Op>(rows.first().map(|r| r.get::<_, Vec<u8>>(0)))
                })
            })
        })
        .ok_or(AdapterError::StateRead)?
        .map_err(|_| AdapterError::StateRead)
    }

    fn write(&mut self, slot: Slot, bytes: &[u8]) -> Result<(), AdapterError> {
        let (bridge, tenant, node) = (
            Arc::clone(&self.bridge),
            self.tenant.clone(),
            self.node.clone(),
        );
        let slot = slot_name(slot);
        let bytes = bytes.to_vec();
        off_runtime(move || {
            bridge.run(DEADLINE, move |conn| {
                Box::pin(async move {
                    conn.execute(WRITE, &[&tenant, &node, &slot, &bytes])
                        .await?;
                    Ok::<(), Op>(())
                })
            })
        })
        .ok_or(AdapterError::StateWrite)?
        .map_err(|_| AdapterError::StateWrite)
    }
}
