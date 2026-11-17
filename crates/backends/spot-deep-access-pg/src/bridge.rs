//! The synchronous ports of `spot-deep-access` over the shared bridge. Call them from a thread
//! outside any tokio runtime (the bridge refuses a runtime thread with `pg_bridge.wrong_thread`,
//! and every failure here is `DeepError::StateUnavailable`, which refuses access).

use std::sync::Arc;

use spot_deep_access::{DeepError, ReceiptNonces, Revocation, RevocationStore, RevocationView};
use spot_pg_bridge::{Bridge, PooledConn};
use spot_signed_command::KeyId;

pub(crate) const CONSUME_NONCE: &str = "INSERT INTO spot_deep_access_nonces (tenant, key_id, nonce, used_at) \
     VALUES ($1,$2,$3,(extract(epoch from now()))::bigint) ON CONFLICT DO NOTHING";
pub(crate) const INSERT_REVOCATION: &str = "INSERT INTO spot_deep_access_revocations \
     (tenant, session_id, revoked_by, reason, revoked_at) VALUES ($1,$2,$3,$4,$5) \
     ON CONFLICT DO NOTHING";
pub(crate) const LOOKUP_REVOCATION: &str = "SELECT revoked_by, reason, revoked_at \
     FROM spot_deep_access_revocations WHERE tenant = $1 AND session_id = $2";

/// `ReceiptNonces` over PostgreSQL: one `INSERT .. ON CONFLICT DO NOTHING` decides, so two nodes
/// never both consume a nonce.
#[derive(Clone)]
pub struct BridgeNonces {
    bridge: Arc<Bridge>,
}

impl BridgeNonces {
    /// Over the process's shared bridge.
    #[must_use]
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

impl ReceiptNonces for BridgeNonces {
    fn consume(&mut self, tenant: &str, key: &KeyId, nonce: [u8; 16]) -> Result<bool, DeepError> {
        let (tenant, key, nonce) = (tenant.to_owned(), key.as_str().to_owned(), nonce.to_vec());
        self.bridge
            .run_default(move |conn: PooledConn| {
                Box::pin(async move {
                    conn.execute(CONSUME_NONCE, &[&tenant, &key, &nonce])
                        .await
                        .map(|n| n == 1)
                        .map_err(|_| ())
                })
            })
            .map_err(|_| DeepError::StateUnavailable)
    }
}

/// `RevocationStore` over PostgreSQL; the first revocation of a session is kept.
#[derive(Clone)]
pub struct BridgeRevocations {
    bridge: Arc<Bridge>,
}

impl BridgeRevocations {
    /// Over the process's shared bridge.
    #[must_use]
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

impl RevocationView for BridgeRevocations {
    fn lookup(&self, tenant: &str, session: [u8; 16]) -> Result<Option<Revocation>, DeepError> {
        let (tenant, session) = (tenant.to_owned(), session.to_vec());
        self.bridge
            .run_default(move |conn: PooledConn| {
                Box::pin(async move {
                    let row = conn
                        .query_opt(LOOKUP_REVOCATION, &[&tenant, &session])
                        .await
                        .map_err(|_| ())?;
                    Ok::<_, ()>(row.map(|r| Revocation {
                        by: r.get(0),
                        reason: r.get(1),
                        at: r.get(2),
                    }))
                })
            })
            .map_err(|_| DeepError::StateUnavailable)
    }
}

impl RevocationStore for BridgeRevocations {
    fn insert(
        &mut self,
        tenant: &str,
        session: [u8; 16],
        revocation: Revocation,
    ) -> Result<(), DeepError> {
        let (tenant, session) = (tenant.to_owned(), session.to_vec());
        self.bridge
            .run_default(move |conn: PooledConn| {
                Box::pin(async move {
                    conn.execute(
                        INSERT_REVOCATION,
                        &[
                            &tenant,
                            &session,
                            &revocation.by,
                            &revocation.reason,
                            &revocation.at,
                        ],
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| ())
                })
            })
            .map_err(|_| DeepError::StateUnavailable)
    }
}
