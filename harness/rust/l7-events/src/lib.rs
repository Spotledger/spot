//! L7 emitted-event conformance (WP-TST-07.T4).
//!
//! `contracts/events` defines the registry and the payload schemas the consumers (the realtime
//! bridge, webhooks, the journal, audit) validate with. The engine builds its events itself, and
//! nothing compared the whole population of events a loaded system really produced with those
//! schemas. This crate does: six invariants registered on T1's registry read **every** event of
//! the outbox.
//!
//! | Id | Proves |
//! | --- | --- |
//! | `events.registered` | every platform topic is in the registry; app topics outside it are counted |
//! | `events.envelope` | the payload is a well formed `{meta, data}` document |
//! | `events.payload_schema` | `data` validates against its family's schema; no emitter without a schema |
//! | `events.class` | the registry's class and retention agree, audit topics have their document |
//! | `events.no_secret_values` | no planted secret (exact, base64 or hex form) and no forbidden key |
//! | `events.coverage` | every topic the engine is expected to emit was emitted |
//! | `events.rows_match` | (WP-TST-07.T5, opt-in: `rows::RowsMatch` over a `RowSource`) every audit and job-enqueued event has its row and every row its event, once each |
//!
//! Nothing here prints an event payload value: a violation names the topic, the path and the kind
//! of mismatch, never a value.

pub mod classify;
pub mod coverage;
pub mod invariants;
pub mod rows;
pub mod secrets;
pub mod table;

pub use coverage::{ExpectedTopic, expected_for_engine_world};
pub use invariants::{
    CLASS, COVERAGE, ENVELOPE, EventClass, EventCoverage, EventEnvelope, NoSecretValues,
    PayloadSchema, REGISTERED, Registered, SCHEMA, SECRETS, conformance_registry,
    conformance_registry_with_rows,
};
pub use rows::{ROWS, RowSource, RowsMatch};
