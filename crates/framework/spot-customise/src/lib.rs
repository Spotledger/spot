//! `spot-customise` — DocTypes, custom fields and property overrides created while the site
//! runs (WP-PLT-16.T1; ADR-05, ADR-068).
//!
//! The pieces exist elsewhere: `spot-meta` validates and merges, `spot-pg-migrate` plans and
//! applies additive schema changes, `spot-meta-live` swaps snapshots atomically and tells the
//! other nodes. This crate is the one orchestrated, crash-safe path that connects them, and the
//! records that persist what a site changed:
//!
//! - [`CustomiseService`] runs every change through the same pipeline (validate in a scratch
//!   copy, plan the schema and refuse the destructive, persist as `pending`, apply, mark and
//!   swap, announce). A crash between any two steps is recovered by [`CustomiseService::recover`].
//! - [`RecordStore`] persists the three record kinds (`Custom DocType`, `Custom Field`,
//!   `Property Setter`); [`MemoryRecordStore`] is the in-process one, `StorageRecordStore` keeps
//!   them in the platform DocTypes over any storage backend.
//! - [`SchemaBackend`] is the database seam; [`StorageSchema`] drives any backend's additive
//!   `prepare`, and the `postgres` feature adds the online migration engine.
//! - [`Node`] and [`MetaGate`] make a node refuse to serve metadata it cannot reproduce
//!   (`metalive.divergence`) rather than guess.
//!
//! # Safety rules enforced here
//!
//! A custom DocType name may not collide with an app DocType or a reserved name; custom
//! DocTypes live in the module `custom` unless a platform administrator picks another; a custom
//! field may not collide with an existing field; a mandatory custom field on a table with rows
//! needs a default; a security property may only be tightened (`spot-meta` validates a set, this
//! crate refuses a clear); removing a field that something refers to is refused with the
//! referrers listed; a new DocType starts with a single default grant (System Manager read,
//! write, create, delete) and is closed to everyone else until granted.

mod assemble;
pub mod error;
pub mod host;
pub mod node;
pub mod records;
pub mod schema;
mod service;
pub mod storage_records;

#[cfg(feature = "postgres")]
pub mod pg;

pub use assemble::CUSTOM_MODULE;
pub use error::{CustomiseError, CustomiseResult, Issue, codes};
pub use host::{
    AuditEntry, AuditSink, Caller, CustomiseAccess, CustomiseEvent, DefaultGrant, EventSink,
    GrantSink, MemoryHost, NoReferrers, Referrer, ReferrerLookup, Right, RoleAccess,
    SYSTEM_MANAGER,
};
pub use node::{DefinitionSink, ListenerFuture, MetaGate, Node, SwapListener};
pub use records::{
    DocTypeRecord, FieldRecord, MemoryRecordStore, PropertyRecord, RecordKey, RecordOp,
    RecordState, RecordStore, Records,
};
pub use schema::{
    Fill, PlanStep, RefusedStep, SchemaBackend, SchemaFuture, SchemaPlan, StepClass, StorageSchema,
    backfill_documents, diff_catalogs,
};
pub use service::{Applied, CustomiseService, Entry, FaultHook, Parts, Recovery};
pub use storage_records::StorageRecordStore;
