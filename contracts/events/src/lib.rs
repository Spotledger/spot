//! Event and outbox schema contract v0.1 for the Spotledger Platform (WP-CON-04, ADR-056).
//!
//! The storage contract (ADR-053) fixes how events are **captured and delivered**: an
//! [`OutboxEvent`](spot_storage_contract::OutboxEvent) with an id, a topic, a subject and its
//! version, a schema version and a JSON payload, published with safe bookmarks and causal order.
//! This crate fixes what events **mean and how they evolve**:
//!
//! - [`names`]: validated [`Topic`], [`SourceId`] and [`CommandId`].
//! - [`payload`]: the JSON document stored in `OutboxEvent::payload`, `{"meta": ..., "data": ...}`,
//!   where [`EventMeta`] carries the actor, the command, causation and correlation ids and the
//!   occurrence time. Nothing here needs a storage contract change.
//! - [`envelope`]: the external [`EventEnvelope`] that consumers outside the source database
//!   see (integrations, webhooks, branch sync). The relay builds it from an
//!   [`OutboxRecord`](spot_storage_contract::OutboxRecord) and stamps the [`SourceId`].
//! - [`schema`]: payload schemas per topic and major version, and validation of a payload
//!   against its schema in the native API value encoding (decimals as strings, ADR-055).
//! - [`compat`]: the compatibility checker that enforces the evolution rules.
//! - [`lifecycle`]: the platform's own `doc_*` topics behind the ADR-51 `*_committed` phase.
//! - [`system`]: the `sys_doc_audit`, `sys_graph_edge` and `sys_secret_audit` topics: the document
//!   audit event (including the seven optional fields of audit schema 2), the graph edge event and
//!   the secret field audit event (ADR-056 amendments A3, A4 and A6), with bounded, typed
//!   payloads.
//! - [`system_platform`]: the payload schemas of the nine other platform `sys_*` topics (jobs,
//!   notices, operation receipts, partition ranges, process governance and migration, conditions).
//! - [`journal`]: the payload schemas of the five `sys_*` audit topics of the journal and sync
//!   hosting (access, grants, journal admin, sync node admin, sync decisions; ADR-061, ADR-067,
//!   ADR-062).
//! - [`state`]: the payload schema of the State event `doc_state` (op, entity_schema, image) and the
//!   rule that a state envelope carries `subject_version`.
//! - [`registry`]: the one event class registry (ADR-061 section 2): every platform topic's
//!   [`registry::Class`], retention, coalescing and owner, with the rule that a class never changes.
//! - [`identity`]: the platform's `identity_*` topics for sign-in, users, roles, assignments,
//!   sessions, keys and emergency access (ADR-070), and the rule that no secret travels in one.
//!
//! # Evolution rules (ADR-056)
//!
//! 1. A topic's payload has a **major** version, carried on the wire as `schema_version`, and a
//!    **revision** that only the schema registry records.
//! 2. Within a major version only additive changes are allowed: new optional fields, new enum
//!    values only where the field declares itself open, and deprecation marks. Consumers must
//!    ignore unknown fields.
//! 3. A field name, once removed, is **retired** and never reused within that topic.
//! 4. Anything else is breaking and needs a new major version. During a deprecation window the
//!    producer publishes both majors as separate events with distinct ids.
//!
//! [`compat::check`] rejects every breaking change within one major version.

pub mod compat;
pub mod envelope;
pub mod identity;
pub mod journal;
pub mod lifecycle;
pub mod names;
pub mod payload;
pub mod registry;
pub mod schema;
pub mod state;
pub mod system;
pub mod system_platform;

#[cfg(test)]
#[path = "journal_tests.rs"]
mod journal_tests;
#[cfg(test)]
mod registry_tests;
#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
#[cfg(test)]
#[path = "system_platform/tests.rs"]
mod system_platform_tests;
#[cfg(test)]
mod tests;

pub use compat::{Breaking, check};
pub use envelope::{EnvelopeError, EventEnvelope};
pub use names::{CommandId, InvalidName, SourceId, Topic};
pub use payload::{
    Actor, EventMeta, PayloadDocument, PayloadError, PrincipalKind, PrincipalRef,
    PrincipalRefError, SYSTEM_PRINCIPAL,
};
pub use schema::{EventSchema, FieldSchema, PayloadType, SchemaError};

/// The semantic version of this contract. Implementers pin it (blueprint §4).
pub const CONTRACT_VERSION: &str = "0.7.6";
