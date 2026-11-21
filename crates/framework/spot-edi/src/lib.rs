#![forbid(unsafe_code)]
//! The EDI adapter (WP-INT-06.T1; blueprint WS17, WP-INT-06).
//!
//! Reads and writes **ASC X12** and **UN/EDIFACT** interchanges with our own implementation of
//! the public structure rules — delimiters, envelopes and segment/element/component syntax —
//! and never with a licensed implementation guide's text. The acceptance is a round trip:
//! parsing a canonical document and writing it again gives identical bytes.
//!
//! # The shape
//!
//! 1. [`delimiters::discover`] finds the syntax and its separators: X12's `ISA` is a fixed-width
//!    106-character record, EDIFACT either opens with a `UNA` service string advice or uses the
//!    syntax defaults revealed by `UNB`.
//! 2. [`tokenizer::SegmentScanner`] is a byte-at-a-time state machine that honours the release
//!    character and yields whole [`model::Segment`]s, bounded by [`limits::Limits`];
//!    [`parser::parse`] and [`parser::parse_reader`] turn a slice or a reader into an
//!    [`model::Interchange`].
//! 3. The model is a typed [`model::Interchange`] of [`model::Segment`]s of
//!    [`model::Element`]s (simple, composite or repeated), with the envelope's identity and the
//!    nested [`model::Group`]/[`model::Message`] view.
//! 4. [`envelope::check`] compares every declared count and control number with the content.
//! 5. [`schema`] is a lightweight, data-driven message definition (`schemas/x12/*.toml`,
//!    `schemas/edifact/*.toml`) and [`schema::validate`] reports every violation with its
//!    position.
//! 6. [`mapping::EdiMapping`] maps a message to and from a JSON document through the shared
//!    [`spot_connectors::MappingSpec`].
//! 7. [`ack::acknowledge`] builds a `997` or `CONTRL` acknowledgement with accept/reject per
//!    transaction and the error codes that go with it.
//! 8. [`connector::EdiConnector`] exposes `parse`, `build`, `validate` and `acknowledge` as a
//!    connector operation set for a BPM integration node or a connector bundle. Transport
//!    (AS2/SFTP) is out of scope: bytes and text in, bytes and text out.
//!
//! # What this crate does not do
//!
//! Copy schema text or code lists from a licensed standard (every definition here is our own
//! minimal one), use floating point for money or quantities, read the wall clock (time is an
//! argument), or use `unsafe`.

pub mod ack;
pub mod connector;
pub mod delimiters;
pub mod envelope;
pub mod error;
pub mod limits;
pub mod mapping;
pub mod model;
pub mod parser;
pub mod report;
pub mod schema;
pub mod tokenizer;
pub mod writer;

pub use ack::{
    AckRequest, Action, TransactionResult, acknowledge, message_997, message_contrl, results_from,
};
pub use connector::{CONNECTOR_ID, EdiConnector, connector_id, to_connector_error};
pub use delimiters::{Delimiters, Syntax, discover};
pub use envelope::{check as check_envelope, is_consistent};
pub use error::{EdiError, Result, codes};
pub use limits::Limits;
pub use mapping::EdiMapping;
pub use model::{Element, Group, Interchange, Message, Party, Segment};
pub use parser::{parse, parse_reader, parse_str};
pub use report::{Position, Violation, has_code};
pub use schema::{ElementKind, ElementRule, Schema, SegmentRule, validate as validate_schema};
pub use tokenizer::{SegmentScanner, segments};
pub use writer::{WriteOptions, write, write_segments};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-edi";
