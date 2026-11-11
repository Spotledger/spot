//! The PostgreSQL link store for the tenant link (WP-CTL-09.T2, ADR-058, ADR-080).
//!
//! `spot-tenant-link` (WP-CTL-09.T1) is the pure link core: the model, the signed handshake,
//! grants, consent, revocation and the [`LinkStore`](spot_tenant_link::LinkStore) port. This
//! crate makes the port durable on PostgreSQL and adds the administration operations the
//! management API calls.
//!
//! # Where link data lives (ADR-058 rule 3)
//!
//! - **In each tenant's own schema:** the link, its grants, consents, **public** peer keys, each
//!   side's offer, references to the audit records and the handshake nonces seen
//!   ([`PgLinkStore`], [`schema`]). A tenant never reads another tenant's rows: every statement
//!   runs with that tenant's schema as the only `search_path` entry.
//! - **In the control schema:** `link_registry(tenant_a, tenant_b, link_id, state)` and nothing
//!   else ([`PgLinkRegistry`]). It records that a link exists and who its parties are, for
//!   routing; it has no grant, topic, field, key or consent column.
//!
//! No private key is ever stored; the identity signer stays outside this crate and the only key
//! column holds a 32-byte public key.
//!
//! # Revocation and readers
//!
//! The store reads the database on every call, so a revocation is visible to the next read. A
//! host that caches wraps the store in a [`CachedLinkStore`]; the store pushes an invalidation
//! through [`LinkInvalidation`] after every commit. See [`cache`] for how other nodes are told
//! and why there is no time-to-live.
//!
//! # Operations
//!
//! [`LinkAdmin`] holds the service functions: propose, accept, reject, grant, revoke, list, show,
//! and show audit. Each needs the `link.manage` right ([`permissions`]), re-confirms consents
//! through the step-up gate, and audits before it changes (see [`admin`]).
//!
//! # Handshake replay
//!
//! [`PgLinkStore::claim_inbound`] persists the handshake nonces, so a message replayed after a
//! restart is refused. The host calls it for each inbound message before handing the message to
//! `Handshake::receive`; `Handshake` keeps its own in-memory set for the session and cannot be
//! re-seeded from the store, so the persisted set is the one that survives a restart.
//!
//! The nonce is claimed **last**: `claim_inbound` first checks the signature, the link and the
//! signer's key, so an outsider who cannot sign as the peer cannot add a row, and no public call
//! claims a nonce for an unchecked message. [`PgLinkStore::prune_nonces`] removes the nonces older
//! than the handshake's replay window (the consent age). A revocation from the peer is a signed
//! message too ([`LinkAdmin::receive_revoke`]); the local revoke takes its party and actor from
//! the store and the authenticated caller.
//!
//! [`PgLinkStore::handshake_ports`] is the one call a host uses to build the `NonceLog` and
//! `PeerKeyDirectory` the handshake needs, durable across restarts ([`ports`]).
//!
//! # Time, secrets and SQL
//!
//! The crate reads no clock (every instant is an argument) and uses no floats or `unsafe`. Every
//! SQL statement is static text with bound parameters; failures carry fixed sentences, never a
//! driver message, a connection string or a value.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, like the other `*-pg` crates; with `SPOT_REQUIRE_PG=1` a missing
//! server is a failure.

#![warn(missing_docs)]

pub mod admin;
pub mod audit_refs;
mod blocking;
pub mod cache;
mod codec;
pub mod error;
pub mod intake;
pub mod permissions;
pub mod ports;
pub mod registry;
pub mod schema;
pub mod store;

pub use admin::{Caller, GRANTED_KIND, LinkAdmin};
pub use audit_refs::{AuditRef, RefRecordingSink};
pub use cache::{CacheHandle, CachedLinkStore, LinkInvalidation};
pub use error::{MIGRATION_FAILED, OpError, PERMISSION_DENIED};
pub use intake::{HandshakeSink, PgLinkIntake};
pub use permissions::{
    DenyAll, EnginePermissions, LINK_DOCTYPE, LinkPermissions, Operation, manage_right,
};
pub use ports::{PgNonceLog, PgPeerKeyDirectory};
pub use registry::{PgLinkRegistry, RegistryEntry, RegistryState};
pub use schema::{CONTROL_SCHEMA, REGISTRY_COLUMNS, REGISTRY_DDL, TENANT_DDL, TENANT_TABLES};
pub use store::PgLinkStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-tenant-link-pg";
