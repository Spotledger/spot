//! `spot-bpm-contract` — the process contract of a conversation across
//! organisations (WP-BPM-12, ADR-080 section 5).
//!
//! A process runs inside one tenant (ADR-080 section 5.1). When several
//! organisations take part, each runs **its own half** and the halves agree on
//! a **process contract**: a versioned, signed document every participant
//! holds, naming the roles, the messages that may cross with the fields that
//! may cross in them, the order rules, the wait limits and the outcomes
//! (ADR-080 section 5.2). This crate is the model, the text form and the two
//! checks; it never sends, receives or executes anything — that is
//! `spot-bpm-conversations` (WP-BPM-12.T2) and the engine.
//!
//! - [`ProcessContract`] and its parts: [`Participant`], [`MessageDef`],
//!   [`Transition`], [`Outcome`], [`TimeoutRule`] and [`Signature`];
//! - the deterministic TOML [text form](crate::to_text) — the reviewable
//!   document a bundle section carries and two participants exchange;
//! - [`ProcessContract::content_hash`] and the JSON Schema
//!   [subset](crate::Schema) a payload is checked against;
//! - [`limits`], the size bounds that make a partner-supplied contract safe to
//!   read and check (`bpm_contract.too_large`);
//! - [`check`], which reports **every** problem with a contract at once;
//! - [`conforms`], which checks a process half against the contract for the
//!   role it plays — the designer's check and the publish gate;
//! - [`diff`], [`compatible_range`] and [`can_continue`], which say what a
//!   version change means for a running conversation;
//! - [`verify_signatures`], which verifies any signature present and requires
//!   none, because the signing policy is still open (ADR-080 decision 4).
//!
//! The crate is pure and synchronous: no I/O, no async, no storage, no clock,
//! no randomness, no float arithmetic (ADR-12), no `unsafe` and no
//! `unwrap`/`expect` outside tests, so it runs on any host, including a WASM
//! guest. Time is a number of seconds the caller supplies.
//!
//! # Building on the process model
//!
//! The two new step kinds ([`SendMessage`](spot_bpm_model::SendMessageConfig)
//! and [`WaitMessage`](spot_bpm_model::WaitMessageConfig)) and
//! [`Participants`](spot_bpm_model::Participants) are part of
//! `spot-bpm-model`; this crate names them by their contract id and message
//! name and never invents a second process type.

#![forbid(unsafe_code)]

pub mod check;
pub mod conform;
pub mod diff;
pub mod error;
pub mod hash;
pub mod limits;
pub mod meta;
pub mod model;
pub mod schema;
pub mod signature;
pub mod text;

pub use check::{Problem, check, check_with, codes, is_field_name, is_name, is_topic};
pub use conform::{conforms, conforms_process};
pub use diff::{
    Change, Compatibility, CompatibleRange, ContractChange, RangeError, Subject, can_continue,
    compatible_range, diff, is_compatible,
};
pub use error::TextError;
pub use hash::canonical_bytes;
pub use meta::{ClassifiedFields, FieldClass, FieldMetaView, PublicFields};
pub use model::{
    Field, FieldKind, MessageDef, Next, Outcome, OutcomeKind, Participant, ParticipantKind,
    ProcessContract, Signature, TimeoutRule, Transition,
};
pub use schema::{Schema, SchemaError, SchemaProperty};
pub use signature::{
    AcceptAnySignature, RefuseEverySignature, SignatureKey, SignatureVerifier, verify_signatures,
};
pub use text::{from_text, to_text};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-contract";

/// The shipped contracts, as data, in file order.
pub mod contracts {
    /// Two parties: a requester asks, an approver approves or rejects, and the
    /// requester is told.
    pub const REQUEST_APPROVE: &str = include_str!("../contracts/request_approve.toml");
    /// Three parties: a buyer orders through a broker, who forwards the order
    /// to a supplier as a new message on the next link (ADR-080 section 5.8).
    pub const THREE_PARTY_RELAY: &str = include_str!("../contracts/three_party_relay.toml");

    /// Every shipped contract, as `(file name, text)`.
    pub const ALL: [(&str, &str); 2] = [
        ("request_approve.toml", REQUEST_APPROVE),
        ("three_party_relay.toml", THREE_PARTY_RELAY),
    ];
}
