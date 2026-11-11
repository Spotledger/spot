//! The graph model in metadata (WS8, WP-GRA-01.T1; ADR-50).
//!
//! A graph model is *derived* from the effective metadata:
//! [`GraphModel::derive`] reads an
//! [`EffectiveMeta`](spot_meta::EffectiveMeta) and produces the node and edge
//! types the whole graph layer agrees on. Edges exist **for every link without
//! code** — a `Link` field, a `Table` field (an owned parent-to-child
//! containment) and a `DynamicLink` field all get one — so a DocType created at
//! runtime has its edges exactly like a compiled one, in the next derive with
//! no rebuild.
//!
//! The model is pure data, deterministic and cheap to rebuild from scratch: no
//! storage, no async, no clock, no randomness, no floats, no `unsafe`. The
//! same metadata (listed in any order) always yields an equal model with equal
//! [`EdgeTypeId`]s, which is what the incremental index (WP-GRA-02) and the
//! golden dumps rely on.
//!
//! # Pieces
//!
//! - [`derive`](GraphModel::derive): builds a [`NodeType`] per non-child
//!   DocType and an [`EdgeType`] per link-shaped field; relation DocTypes are
//!   named to [`derive`](GraphModel::derive) with
//!   [`RelationSpec`]s (spot-meta has no relation flag; a later task reads
//!   them from metadata) and become one typed edge between their two ends,
//!   with their remaining fields as edge properties.
//! - [`EdgeType`]: stable [`EdgeTypeId`] (SHA-256 of the canonical name),
//!   canonical `name`, `from`/`to`, [`Cardinality`], `required`, edge
//!   properties and [`EdgeSource`].
//! - [`EdgeTypeRegistry`]: forward and reverse lookups by id, by canonical
//!   name, by endpoint node.
//! - [`GraphModel::diff`]: a [`ModelDiff`] of added, removed and changed edge
//!   types between two derives, for the incremental index later.
//! - [`GraphModel::problems`]: dangling link declarations reported, never
//!   panicked or silently dropped.
//! - [`GraphModel::to_text`]: the stable text dump the golden tests pin.
//!
//! # Relation specs
//!
//! A relation DocType (for example a project task linking a deliverable to a
//! resource) is itself a node carrying two link fields; the spec says it
//! *means* one edge from the `from_field`'s DocType to the `to_field`'s
//! DocType. The edge then carries every **other** field of the DocType as edge
//! properties ([`EdgeProperty`]). A spec is applied only when both fields are
//! links and the DocType exists; anything else is listed in
//! [`problems`](GraphModel::problems) without ever panicking.
//!
//! # What this crate is not
//!
//! Not a runtime: it builds no petgraph, holds no records, answers no
//! traversal. WP-GRA-02 builds the index over this model, WP-GRA-03 the query
//! API. [`AnyOf`]/[`Dynamic`](TargetShape::Dynamic) targets are the model's
//! honest answer for a dynamic link, not a placeholder: the index may build
//! the concrete edges per record.

#![forbid(unsafe_code)]

pub mod derive;
pub mod diff;
pub mod error;
pub mod model;
pub mod registry;
pub mod text;

pub use diff::{EdgeChange, ModelDiff};
pub use error::{GraphModelError, Problem};
pub use model::{
    AggregateHow, AggregateSpec, AnyOf, Cardinality, EdgeProperty, EdgeSource, EdgeType,
    EdgeTypeId, GraphModel, NodeType, NodeTypeId, Ownership, PropertyKind, RelationRef,
    RelationSpec, TargetShape, contains_name,
};
pub use registry::EdgeTypeRegistry;

pub use crate::text::GRAPH_TEXT_VERSION;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-graph-model";
