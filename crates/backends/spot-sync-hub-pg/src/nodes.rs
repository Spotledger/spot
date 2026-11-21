//! [`PgNodeRegistry`]: the hub's nodes, their lifecycle state and their subscriptions.
//!
//! A node's subscription (the scope keys it holds and the entities it receives) is written by the
//! **administrator path only**: nothing a node sent is ever stored in `subscription_json`. It is
//! stored in a fixed JSON form and read back through the master crate's own constructors
//! (`NodeId::new`, `ScopeKey::new`, `Doctype::new`), so a stored subscription is validated every
//! time. **An unreadable subscription makes the node receive nothing**
//! (`sync_hub_pg.subscription_invalid`): [`PgNodeRegistry::active_router`] leaves the node out
//! and names it, it is never widened to "everything".
//!
//! Lifecycle: `enrolled` (known, not yet serving), `active`, `suspended`, `revoked`. Revoked is
//! final (a revoked node is never reinstated; enrol a new one). Only `active` nodes with a valid
//! subscription are destinations: suspended and revoked nodes are never returned by
//! [`PgNodeRegistry::active_router`] or [`PgNodeRegistry::active_nodes`].

use serde::{Deserialize, Serialize};
use spot_sync_master::{Doctype, Router, ScopeKey, Subscription};
use spot_sync_protocol::NodeId;

use crate::db::{Db, MAX_NAME};
use crate::error::{OpError, PgHubError, map};

/// Deadline of a registry call.
pub const NODE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// The longest stored subscription, in bytes.
pub const MAX_SUBSCRIPTION_BYTES: usize = 1024 * 1024;

pub(crate) const UPSERT: &str = "INSERT INTO spot_sync_node \
    (tenant, node_id, role, state, subscription_json, version_range, created_at, updated_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $7) ON CONFLICT (tenant, node_id) DO NOTHING";
pub(crate) const GET: &str = "SELECT node_id, role, state, subscription_json, last_seen, last_ack_serve_seq, \
    applied_prefix, version_range, created_at, updated_at FROM spot_sync_node \
    WHERE tenant = $1 AND node_id = $2";
pub(crate) const LIST: &str = "SELECT node_id, role, state, subscription_json, last_seen, last_ack_serve_seq, \
    applied_prefix, version_range, created_at, updated_at FROM spot_sync_node \
    WHERE tenant = $1 ORDER BY node_id COLLATE \"C\"";
pub(crate) const SET_STATE: &str = "UPDATE spot_sync_node SET state = $3, updated_at = $4 \
    WHERE tenant = $1 AND node_id = $2 AND state <> 'revoked'";
pub(crate) const ACTIVE: &str = "SELECT node_id, subscription_json FROM spot_sync_node \
    WHERE tenant = $1 AND state = 'active' ORDER BY node_id COLLATE \"C\"";
const SET_SUBSCRIPTION: &str = "UPDATE spot_sync_node SET subscription_json = $3, updated_at = $4 \
    WHERE tenant = $1 AND node_id = $2 AND state <> 'revoked'";
const TOUCH: &str = "UPDATE spot_sync_node SET last_seen = GREATEST(COALESCE(last_seen, 0), $3) \
    WHERE tenant = $1 AND node_id = $2";

/// A node's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeState {
    /// Known to the hub, not yet serving.
    Enrolled,
    /// Serving: a destination.
    Active,
    /// Temporarily not served.
    Suspended,
    /// Permanently not served.
    Revoked,
}

impl NodeState {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enrolled => "enrolled",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Revoked => "revoked",
        }
    }

    /// The state named exactly `text`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "enrolled" => Some(Self::Enrolled),
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// One node as the registry keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    /// The node id.
    pub node_id: String,
    /// The node's role (a short lower-case name chosen by the administrator).
    pub role: String,
    /// The lifecycle state.
    pub state: NodeState,
    /// The subscription, or `None` when the stored one cannot be read (the node receives
    /// nothing).
    pub subscription: Option<Subscription>,
    /// When the hub last heard from the node, in hub milliseconds.
    pub last_seen: Option<i64>,
    /// The last serve sequence the node acknowledged.
    pub last_ack_serve_seq: u64,
    /// The journal prefix the hub knows the node applied.
    pub applied_prefix: u64,
    /// The software version range the node reported.
    pub version_range: String,
    /// When the node was registered, in hub milliseconds.
    pub created_at: i64,
    /// When the row last changed, in hub milliseconds.
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeJson {
    key_type: String,
    value: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionJson {
    node: String,
    scope_keys: Vec<ScopeJson>,
    doctypes: Vec<String>,
}

/// The stored form of a subscription.
fn encode(subscription: &Subscription) -> Result<String, PgHubError> {
    let json = SubscriptionJson {
        node: subscription.node().as_str().to_owned(),
        scope_keys: subscription
            .scope_keys()
            .iter()
            .map(|key| ScopeJson {
                key_type: key.key_type().to_owned(),
                value: key.value().to_owned(),
            })
            .collect(),
        doctypes: subscription
            .doctypes()
            .iter()
            .map(|d| d.as_str().to_owned())
            .collect(),
    };
    let text = serde_json::to_string(&json).map_err(|_| PgHubError::SubscriptionInvalid)?;
    if text.len() > MAX_SUBSCRIPTION_BYTES {
        return Err(PgHubError::SubscriptionInvalid);
    }
    Ok(text)
}

/// Reads and validates a stored subscription for `node`; `None` for anything unreadable.
fn decode(node: &str, text: &str) -> Option<Subscription> {
    if text.len() > MAX_SUBSCRIPTION_BYTES {
        return None;
    }
    let json: SubscriptionJson = serde_json::from_str(text).ok()?;
    if json.node != node {
        return None;
    }
    let id = NodeId::new(&json.node).ok()?;
    let mut keys = Vec::with_capacity(json.scope_keys.len());
    for key in &json.scope_keys {
        keys.push(ScopeKey::new(&key.key_type, &key.value).ok()?);
    }
    let mut doctypes = Vec::with_capacity(json.doctypes.len());
    for name in &json.doctypes {
        doctypes.push(Doctype::new(name).ok()?);
    }
    Some(Subscription::new(id, keys, doctypes))
}

fn check_role(role: &str) -> Result<(), PgHubError> {
    let ok = !role.is_empty()
        && role.len() <= 64
        && role
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(PgHubError::NodeRefused {
            reason: "a role is 1 to 64 lower-case letters, digits or underscores",
        })
    }
}

fn check_version_range(text: &str) -> Result<(), PgHubError> {
    if text.len() <= 128 && !text.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(PgHubError::NodeRefused {
            reason: "the version range is too long or not printable",
        })
    }
}

fn row_of(row: &tokio_postgres::Row) -> Result<NodeRow, OpError> {
    let node_id: String = row.get(0);
    let state_text: String = row.get(2);
    let state = NodeState::parse(&state_text).ok_or(OpError::Decode)?;
    let text: String = row.get(3);
    Ok(NodeRow {
        subscription: decode(&node_id, &text),
        node_id,
        role: row.get(1),
        state,
        last_seen: row.get(4),
        last_ack_serve_seq: u64::try_from(row.get::<_, i64>(5)).map_err(|_| OpError::Decode)?,
        applied_prefix: u64::try_from(row.get::<_, i64>(6)).map_err(|_| OpError::Decode)?,
        version_range: row.get(7),
        created_at: row.get(8),
        updated_at: row.get(9),
    })
}

/// The destinations a hub serves right now.
#[derive(Debug)]
pub struct ActiveRouter {
    /// The router over the active nodes whose subscription is valid.
    pub router: Router,
    /// Active nodes left out because their stored subscription cannot be read: they receive
    /// nothing until an administrator fixes it.
    pub invalid: Vec<String>,
}

/// The node registry of one tenant.
#[derive(Clone, Debug)]
pub struct PgNodeRegistry {
    db: Db,
}

impl PgNodeRegistry {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// Registers a node in `state` (normally `enrolled`) at hub time `now` (milliseconds). The
    /// subscription is the administrator's, never a node's.
    ///
    /// # Errors
    /// `sync_hub_pg.node_refused` when the node exists, the role or version range is malformed, or
    /// the state is `revoked`; `sync_hub_pg.subscription_invalid` when it cannot be stored.
    pub fn register(
        &self,
        node: &NodeId,
        role: &str,
        subscription: &Subscription,
        state: NodeState,
        version_range: &str,
        now: i64,
    ) -> Result<(), PgHubError> {
        check_role(role)?;
        check_version_range(version_range)?;
        if state == NodeState::Revoked || node.as_str().len() > MAX_NAME {
            return Err(PgHubError::NodeRefused {
                reason: "a node is registered enrolled, active or suspended",
            });
        }
        if subscription.node() != node {
            return Err(PgHubError::SubscriptionInvalid);
        }
        let text = encode(subscription)?;
        let (tenant, id, role, range) = (
            self.db.tenant.clone(),
            node.as_str().to_owned(),
            role.to_owned(),
            version_range.to_owned(),
        );
        let inserted = self
            .db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let state = state.as_str();
                    let n = conn
                        .execute(UPSERT, &[&tenant, &id, &role, &state, &text, &range, &now])
                        .await?;
                    Ok(n)
                })
            })
            .map_err(map)?;
        if inserted == 1 {
            Ok(())
        } else {
            Err(PgHubError::NodeRefused {
                reason: "the node is already registered",
            })
        }
    }

    /// Replaces a node's subscription.
    ///
    /// # Errors
    /// `sync_hub_pg.node_refused` for an unknown or revoked node; `sync_hub_pg.subscription_invalid`
    /// when it cannot be stored.
    pub fn set_subscription(
        &self,
        node: &NodeId,
        subscription: &Subscription,
        now: i64,
    ) -> Result<(), PgHubError> {
        if subscription.node() != node {
            return Err(PgHubError::SubscriptionInvalid);
        }
        let text = encode(subscription)?;
        let (tenant, id) = (self.db.tenant.clone(), node.as_str().to_owned());
        let changed = self
            .db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    Ok(conn
                        .execute(SET_SUBSCRIPTION, &[&tenant, &id, &text, &now])
                        .await?)
                })
            })
            .map_err(map)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(PgHubError::NodeRefused {
                reason: "the node is unknown or revoked",
            })
        }
    }

    /// Moves a node to `state`. A revoked node never changes again.
    ///
    /// # Errors
    /// `sync_hub_pg.node_refused` for an unknown or revoked node, or a move to `enrolled`.
    pub fn set_state(&self, node: &NodeId, state: NodeState, now: i64) -> Result<(), PgHubError> {
        if state == NodeState::Enrolled {
            return Err(PgHubError::NodeRefused {
                reason: "a node cannot move back to enrolled",
            });
        }
        let (tenant, id) = (self.db.tenant.clone(), node.as_str().to_owned());
        let changed = self
            .db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let state = state.as_str();
                    Ok(conn
                        .execute(SET_STATE, &[&tenant, &id, &state, &now])
                        .await?)
                })
            })
            .map_err(map)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(PgHubError::NodeRefused {
                reason: "the node is unknown or revoked",
            })
        }
    }

    /// Records that the hub heard from `node` at `at` (never moves backwards).
    ///
    /// # Errors
    /// The store's failure.
    pub fn touch(&self, node: &NodeId, at: i64) -> Result<(), PgHubError> {
        let (tenant, id) = (self.db.tenant.clone(), node.as_str().to_owned());
        self.db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    conn.execute(TOUCH, &[&tenant, &id, &at]).await?;
                    Ok(())
                })
            })
            .map_err(map)
    }

    /// One node, or `None` when the tenant has no such node (another tenant's node is exactly as
    /// missing).
    ///
    /// # Errors
    /// The store's failure.
    pub fn get(&self, node: &NodeId) -> Result<Option<NodeRow>, PgHubError> {
        let (tenant, id) = (self.db.tenant.clone(), node.as_str().to_owned());
        self.db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    match conn.query_opt(GET, &[&tenant, &id]).await? {
                        Some(row) => Ok(Some(row_of(&row)?)),
                        None => Ok(None),
                    }
                })
            })
            .map_err(map)
    }

    /// Every node of the tenant, ordered by id.
    ///
    /// # Errors
    /// The store's failure.
    pub fn list(&self) -> Result<Vec<NodeRow>, PgHubError> {
        let tenant = self.db.tenant.clone();
        self.db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn.query(LIST, &[&tenant]).await?;
                    rows.iter().map(row_of).collect()
                })
            })
            .map_err(map)
    }

    /// The subscription of `node`.
    ///
    /// # Errors
    /// `sync_hub_pg.subscription_invalid` when the stored one cannot be read (the node receives
    /// nothing); `None` for an unknown node.
    pub fn subscription(&self, node: &NodeId) -> Result<Option<Subscription>, PgHubError> {
        match self.get(node)? {
            None => Ok(None),
            Some(row) => row
                .subscription
                .map(Some)
                .ok_or(PgHubError::SubscriptionInvalid),
        }
    }

    /// The ids of the active nodes (suspended, revoked and enrolled ones are never returned).
    ///
    /// # Errors
    /// The store's failure.
    pub fn active_nodes(&self) -> Result<Vec<String>, PgHubError> {
        let tenant = self.db.tenant.clone();
        self.db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn.query(ACTIVE, &[&tenant]).await?;
                    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
                })
            })
            .map_err(map)
    }

    /// The router over the active nodes with a valid subscription, and the active nodes left out
    /// because their subscription is invalid.
    ///
    /// # Errors
    /// The store's failure.
    pub fn active_router(&self) -> Result<ActiveRouter, PgHubError> {
        let tenant = self.db.tenant.clone();
        let rows: Vec<(String, String)> = self
            .db
            .run(NODE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn.query(ACTIVE, &[&tenant]).await?;
                    Ok(rows
                        .iter()
                        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
                        .collect())
                })
            })
            .map_err(map)?;
        let mut router = Router::new();
        let mut invalid = Vec::new();
        for (node, text) in rows {
            let inserted = decode(&node, &text).is_some_and(|s| router.insert(s).is_ok());
            if !inserted {
                invalid.push(node);
            }
        }
        Ok(ActiveRouter { router, invalid })
    }
}
