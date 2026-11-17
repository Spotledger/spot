//! PostgreSQL stores for deep support access (WP-SEC-07.T2).
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_deep_access_requests` | the support request as the operator sent it, its hash and status |
//! | `spot_deep_access_consents` | each customer approval: who, with which key, the signed record, its hash |
//! | `spot_deep_access_nonces` | the receipt nonces already used, one row each; the primary key is the replay guard |
//! | `spot_deep_access_revocations` | the first revocation of a session; never updated, never deleted |
//! | `spot_deep_access_credentials` | what was issued: session id, scope text, window and the **digest** of the secret |
//! | `spot_deep_access_keys` | the customer's enrolled signing keys (public key, identity, who enrolled it) |
//!
//! Rules: the tenant is the first column of every primary key and every statement filters on it,
//! so a row of another tenant answers exactly like a missing one; a credential's secret is never
//! stored (only its SHA-256 digest); a nonce is consumed by one `INSERT .. ON CONFLICT DO NOTHING`
//! so two nodes cannot both win; a revocation is kept by `DO NOTHING` so the first one stays; no
//! error carries a row value, a key or a nonce (only a fixed sentence and a SQLSTATE).
//!
//! [`PgDeepAccessStore`] is the async store (the hosting service uses it). [`BridgeNonces`] and
//! [`BridgeRevocations`] implement the synchronous ports of `spot-deep-access` over the shared
//! [`spot_pg_bridge::Bridge`] and must be called from a thread outside any tokio runtime.

#![warn(missing_docs)]

mod bridge;
mod error;
pub mod schema;
mod store;

pub use bridge::{BridgeNonces, BridgeRevocations};
pub use error::{MIGRATION_FAILED, STORE_FAILED, StoreError};
pub use store::{ConsentRow, CredentialRow, KeyRow, PgDeepAccessStore, RequestRow, RequestStatus};
