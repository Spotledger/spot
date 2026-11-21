//! `spot-edge-sync` — the business graph's edges, maintained atomically from Link fields
//! (WS8, WP-DAG-04.T1; ADR-50).
//!
//! The business graph is never written by hand. Every `Link`, `DynamicLink`, `Table` and
//! `TableMultiselect` field is an edge, and a relation DocType is one typed edge with
//! properties; the edges of a document are a **consequence** of its link values, derived from
//! the same effective metadata the graph model ([`spot_graph_model`]) is derived from. This
//! crate is the small layer that keeps them equal:
//!
//! - [`EdgeSchema::derive`] and [`edges_of`] compute a document's edge set, purely, from the
//!   effective metadata (a runtime-created DocType gets its edges with no code change);
//! - [`diff`] computes what a document change does to that set — create, update, delete and
//!   cancel all reduce to the same add/remove/update-properties;
//! - [`EdgeWriter`] is the only way an edge is written, and [`apply_in`] writes a whole change
//!   in the caller's transaction, so the document and its edges commit or roll back together;
//! - [`LifecycleHook::after_write`] is the post-write seam the document engine calls, in the
//!   document's own transaction, with [`EdgeSync`] as the reference implementation;
//! - [`EdgeParticipant`] is the engine's `TxParticipant`: the hook above, run in the document's
//!   own transaction for every write the engine makes;
//! - [`check_invariant`] proves the acceptance — **the edge set always equals the link set** —
//!   and [`repair`] returns the change that would fix any drift, without applying it.
//!
//! The crate is pure: it reads no clock, holds no random source, does no I/O and never writes an
//! edge outside the hook. Time, ids and storage are its callers' arguments.
//!
//! # Example
//!
//! ```
//! use spot_app_api::prelude::{Name, RecordId, Value};
//! use spot_app_api::Document;
//! use spot_edge_sync::{EdgeSchema, check_invariant, diff, edges_of};
//! use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Registry};
//!
//! // `party` and `order`, the order linking to the party.
//! let party = Definition::new("party");
//! let mut order = Definition::new("order");
//! let mut customer = FieldSpec::new("customer", FieldType::Link);
//! customer.options = Some("party".into());
//! order.fields = vec![customer];
//! let meta = Registry::build(vec![party, order], &Customisations::new()).unwrap();
//! let schema = EdgeSchema::derive(&meta, &[]);
//!
//! let mut doc = Document::new(Name::new("order").unwrap(), RecordId::new_v7());
//! let party_id = RecordId::new_v7();
//! doc.set(Name::new("customer").unwrap(), Value::Id(party_id));
//!
//! // One link, one edge.
//! assert_eq!(edges_of(&schema, &doc).len(), 1);
//! // Creating it adds the edge; deleting it removes it.
//! assert_eq!(diff(&schema, None, Some(&doc)).added.len(), 1);
//! assert_eq!(diff(&schema, Some(&doc), None).removed.len(), 1);
//! // The invariant holds for the document and its own edge set.
//! let edges = edges_of(&schema, &doc);
//! assert!(check_invariant(&schema, [&doc], edges).is_empty());
//! ```

#![forbid(unsafe_code)]

pub mod diff;
pub mod edge;
pub mod engine;
pub mod error;
pub mod hook;
pub mod invariant;
pub mod schema;
pub mod writer;

pub use diff::{EdgeChanges, EdgeUpdate, diff};
pub use edge::{Edge, EdgeKey, PropValue};
pub use engine::{EdgeParticipant, EdgeWriterFor, MemoryEdgeStore, MetaSource};
pub use error::{EdgeResult, EdgeSyncError};
pub use hook::{EdgeSync, HookResult, LifecycleHook, sync_document};
pub use invariant::{Drift, check_invariant, repair};
pub use schema::{EdgeSchema, edges_of};
pub use writer::{EdgeFuture, EdgeWriter, MemoryEdgeWriter, apply_in};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-edge-sync";
