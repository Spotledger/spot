//! The acknowledgement book and the ledger's store.
//!
//! # [`PgAckBook`]
//!
//! [`PgAckBook::ack_and_trim`] is the durable form of the rule of WP-SYN-11.T2: a node's
//! acknowledgement is a **serve sequence**; in **one transaction** the queue is trimmed through it
//! and the node's row records it (`last_ack_serve_seq`, `applied_prefix`, `last_seen`, each only
//! ever moving forward). A crash leaves both or neither, so the queue is never trimmed past an
//! acknowledgement that was not recorded, nor an acknowledgement recorded for items still
//! queued. The acknowledgement is refused (and nothing changes) for an unknown or revoked node
//! and for a serve sequence the queue never issued.
//!
//! [`PgAckBook`] is also the relay's `AckObserver`: the observer trait has no error channel, so a
//! failed write is remembered for [`PgAckBook::take_failure`] and is never silent.
//!
//! # [`PgNodeStateStore`]
//!
//! The obligation ledger (`spot-sync-acks`) keeps one record per `(node, tenant)` and the
//! mandatory events of each tenant behind the infallible `NodeStateStore` trait. This is its
//! PostgreSQL store, for **one tenant**: a call that names another tenant is refused as if the
//! node were unknown and the failure is remembered. A failed read answers "not registered" (the
//! ledger then refuses the acknowledgement, the safe direction); a failed write is remembered for
//! [`PgNodeStateStore::take_failure`], which the host checks after each ledger call.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_sync_acks::{NodeRecord, NodeStateStore};
use spot_sync_hub::AckObserver;
use spot_sync_obligations::MandatoryEvent;

use crate::db::{Db, MAX_NAME};
use crate::error::{OpError, PgHubError, map};
use crate::queue::{ACK_SERVED, LAST_SEQ};

/// Deadline of an acknowledgement (one transaction, three statements).
pub const ACK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// Deadline of a ledger store call.
pub const LEDGER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) const NODE_ACK: &str = "UPDATE spot_sync_node SET \
    last_ack_serve_seq = GREATEST(last_ack_serve_seq, $3), \
    applied_prefix = GREATEST(applied_prefix, $4), \
    last_seen = GREATEST(COALESCE(last_seen, 0), $5) \
    WHERE tenant = $1 AND node_id = $2 AND state <> 'revoked'";
pub(crate) const NODE_LAST: &str = "SELECT last_ack_serve_seq FROM spot_sync_node \
    WHERE tenant = $1 AND node_id = $2";
pub(crate) const NODE_PREFIX: &str = "UPDATE spot_sync_node SET \
    applied_prefix = GREATEST(applied_prefix, $3) \
    WHERE tenant = $1 AND node_id = $2 AND state <> 'revoked'";
pub(crate) const LOAD_RECORD: &str = "SELECT record FROM spot_sync_hub_node_record \
    WHERE tenant = $1 AND node_id = $2";
pub(crate) const SAVE_RECORD: &str = "INSERT INTO spot_sync_hub_node_record (tenant, node_id, record) \
    VALUES ($1, $2, $3) ON CONFLICT (tenant, node_id) DO UPDATE SET record = EXCLUDED.record";
pub(crate) const LIST_RECORDS: &str = "SELECT node_id FROM spot_sync_hub_node_record \
    WHERE tenant = $1 ORDER BY node_id COLLATE \"C\"";
pub(crate) const LIST_EVENTS: &str = "SELECT event FROM spot_sync_hub_mandatory \
    WHERE tenant = $1 ORDER BY position";
pub(crate) const SAVE_EVENT: &str = "INSERT INTO spot_sync_hub_mandatory (tenant, position, event) \
    VALUES ($1, $2, $3) ON CONFLICT (tenant, position) DO NOTHING";

/// What an acknowledgement did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckTrim {
    /// Queue rows the acknowledgement removed.
    pub removed: u64,
    /// The node's last acknowledged serve sequence after this call.
    pub last_ack_serve_seq: u64,
}

fn to_i64(value: u64) -> Result<i64, OpError> {
    i64::try_from(value).map_err(|_| OpError::Decode)
}

/// The acknowledgement book of one tenant.
#[derive(Clone, Debug)]
pub struct PgAckBook {
    db: Db,
    failed: Arc<AtomicBool>,
}

impl PgAckBook {
    pub(crate) fn new(db: Db) -> Self {
        Self {
            db,
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Records the acknowledgement of `through` (a serve sequence) by `node` and trims the node's
    /// queue through it, **in one transaction**. `applied_prefix` is the journal prefix the relay
    /// derived for the node (never the node's own claim) and `at` the hub time in milliseconds.
    ///
    /// # Errors
    /// `sync_hub_pg.node_refused` for an unknown or revoked node, or a serve sequence the queue
    /// never issued (nothing changed); the store's failure (nothing changed).
    pub fn ack_and_trim(
        &self,
        node: &str,
        through: u64,
        applied_prefix: u64,
        at: i64,
    ) -> Result<AckTrim, PgHubError> {
        if node.is_empty() || node.len() > MAX_NAME {
            return Err(PgHubError::NodeRefused {
                reason: "the node name is empty or too long",
            });
        }
        let (tenant, node) = (self.db.tenant.clone(), node.to_owned());
        self.db
            .run(ACK_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let through_i = to_i64(through)?;
                    let prefix = to_i64(applied_prefix)?;
                    let tx = conn.transaction().await?;
                    let issued = match tx.query_opt(LAST_SEQ, &[&tenant, &node]).await? {
                        Some(row) => {
                            u64::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)?
                        }
                        None => 0,
                    };
                    if through > issued {
                        return Err(OpError::Refused(PgHubError::NodeRefused {
                            reason: "the acknowledged serve sequence was never issued",
                        }));
                    }
                    let recorded = tx
                        .execute(NODE_ACK, &[&tenant, &node, &through_i, &prefix, &at])
                        .await?;
                    if recorded != 1 {
                        return Err(OpError::Refused(PgHubError::NodeRefused {
                            reason: "the node is unknown or revoked",
                        }));
                    }
                    let removed = tx
                        .execute(ACK_SERVED, &[&tenant, &node, &through_i])
                        .await?;
                    let row = tx.query_one(NODE_LAST, &[&tenant, &node]).await?;
                    let last = u64::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)?;
                    tx.commit().await?;
                    Ok(AckTrim {
                        removed,
                        last_ack_serve_seq: last,
                    })
                })
            })
            .map_err(map)
    }

    /// Whether a write the observer could not report failed since the last call; clears it.
    pub fn take_failure(&self) -> bool {
        self.failed.swap(false, Ordering::SeqCst)
    }
}

impl AckObserver for PgAckBook {
    fn on_ack(&mut self, dest: &str, applied_prefix: u64) {
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        let outcome = self.db.run(ACK_DEADLINE, move |conn| {
            Box::pin(async move {
                let prefix = to_i64(applied_prefix)?;
                conn.execute(NODE_PREFIX, &[&tenant, &dest, &prefix])
                    .await?;
                Ok(())
            })
        });
        if outcome.is_err() {
            self.failed.store(true, Ordering::SeqCst);
        }
    }
}

/// The ledger's store for one tenant.
#[derive(Clone, Debug)]
pub struct PgNodeStateStore {
    db: Db,
    failed: Arc<AtomicBool>,
}

impl PgNodeStateStore {
    pub(crate) fn new(db: Db) -> Self {
        Self {
            db,
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether a call failed (or named another tenant) since the last call; clears it. The host
    /// checks this after every ledger call that writes.
    pub fn take_failure(&self) -> bool {
        self.failed.swap(false, Ordering::SeqCst)
    }

    fn own(&self, tenant: &str) -> bool {
        if tenant == self.db.tenant {
            true
        } else {
            self.failed.store(true, Ordering::SeqCst);
            false
        }
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::SeqCst);
    }
}

impl NodeStateStore for PgNodeStateStore {
    fn load(&self, node: &str, tenant: &str) -> Option<NodeRecord> {
        if !self.own(tenant) {
            return None;
        }
        let (tenant, node) = (self.db.tenant.clone(), node.to_owned());
        let loaded = self.db.run(LEDGER_DEADLINE, move |conn| {
            Box::pin(async move {
                match conn.query_opt(LOAD_RECORD, &[&tenant, &node]).await? {
                    Some(row) => {
                        let text: String = row.get(0);
                        let record: NodeRecord =
                            serde_json::from_str(&text).map_err(|_| OpError::Decode)?;
                        Ok(Some(record))
                    }
                    None => Ok(None),
                }
            })
        });
        match loaded {
            Ok(record) => record,
            Err(_) => {
                self.fail();
                None
            }
        }
    }

    fn save(&mut self, record: NodeRecord) {
        if !self.own(&record.tenant) {
            return;
        }
        let tenant = self.db.tenant.clone();
        let outcome = self.db.run(LEDGER_DEADLINE, move |conn| {
            Box::pin(async move {
                let text = serde_json::to_string(&record).map_err(|_| OpError::Decode)?;
                conn.execute(SAVE_RECORD, &[&tenant, &record.node, &text])
                    .await?;
                Ok(())
            })
        });
        if outcome.is_err() {
            self.fail();
        }
    }

    fn nodes(&self) -> Vec<(String, String)> {
        let tenant = self.db.tenant.clone();
        let listed = self.db.run(LEDGER_DEADLINE, {
            let tenant = tenant.clone();
            move |conn| {
                Box::pin(async move {
                    let rows = conn.query(LIST_RECORDS, &[&tenant]).await?;
                    Ok(rows
                        .iter()
                        .map(|r| r.get::<_, String>(0))
                        .collect::<Vec<_>>())
                })
            }
        });
        match listed {
            Ok(nodes) => nodes.into_iter().map(|n| (n, tenant.clone())).collect(),
            Err(_) => {
                self.fail();
                Vec::new()
            }
        }
    }

    fn events(&self, tenant: &str) -> Vec<MandatoryEvent> {
        if !self.own(tenant) {
            return Vec::new();
        }
        let tenant = self.db.tenant.clone();
        let loaded = self.db.run(LEDGER_DEADLINE, move |conn| {
            Box::pin(async move {
                let rows = conn.query(LIST_EVENTS, &[&tenant]).await?;
                let mut out = Vec::with_capacity(rows.len());
                for row in rows {
                    let text: String = row.get(0);
                    out.push(
                        serde_json::from_str::<MandatoryEvent>(&text)
                            .map_err(|_| OpError::Decode)?,
                    );
                }
                Ok(out)
            })
        });
        match loaded {
            Ok(events) => events,
            Err(_) => {
                self.fail();
                Vec::new()
            }
        }
    }

    fn save_event(&mut self, tenant: &str, event: MandatoryEvent) {
        if !self.own(tenant) {
            return;
        }
        let tenant = self.db.tenant.clone();
        let outcome = self.db.run(LEDGER_DEADLINE, move |conn| {
            Box::pin(async move {
                let position = to_i64(event.position)?;
                let text = serde_json::to_string(&event).map_err(|_| OpError::Decode)?;
                conn.execute(SAVE_EVENT, &[&tenant, &position, &text])
                    .await?;
                Ok(())
            })
        });
        if outcome.is_err() {
            self.fail();
        }
    }
}
