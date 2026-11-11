//! `spot-bpm-model` — the Process model of the Spotledger Platform
//! (blueprint WS8, WP-BPM-01, ADR-49).
//!
//! A **Process** is the platform's one workflow concept: a graph of typed
//! steps across documents and systems. This crate is the model and the
//! compiler, never an executor. It provides:
//!
//! - the types: [`Process`], [`ProcessVersion`], [`Node`] with its typed
//!   [`NodeKind`], [`Edge`], [`Rule`] and the run state [`CaseState`];
//! - the reviewable TOML [text form](text) — deterministic, round-tripping and
//!   ready to be carried by `spot-custom-bundle` as a section;
//! - [`validate`], which reports **every** problem at once against the DocType
//!   metadata the caller supplies through [`MetaView`];
//! - [`compile`], which turns a version into a `spot_dag::DagDefinition` plus a
//!   side table of per-node config and compiles it onto the DAG runtime;
//! - a [template library](templates) of process definitions that all compile.
//!
//! The crate is pure and synchronous: no I/O, no async, no storage, no clock,
//! no randomness, no float arithmetic (ADR-12), no `unsafe` and no
//! `unwrap`/`expect` outside tests, so it runs on any host, including a WASM
//! guest.
//!
//! # Extensibility (ADR-080)
//!
//! [`NodeKind`] is `#[non_exhaustive]` and carries an
//! [`UnknownNode`](model::UnknownNode) variant: a kind a later work package
//! adds parses, round-trips byte for byte and is reported by [`validate`] as
//! `bpm_model.unknown_node_kind`, never a parse failure. WP-BPM-12 adds the
//! `form`, `send_message` and `wait_message` kinds and the optional
//! [`Participants`] section a process across organisations declares — a new
//! enum variant and a new optional section, no format break.
//!
//! # Names
//!
//! The run is `run` or `case` in code; a person never reads the word "Case".
//! Every user-facing name for a run comes from the label keys in
//! [`model::labels`] (default "Request").

#![forbid(unsafe_code)]

pub mod compile;
pub mod error;
pub mod hash;
pub mod id;
pub mod json;
pub mod model;
pub mod text;
pub mod tomlutil;
pub mod validate;

pub use compile::{CompiledProcess, compile, compile_checked};
pub use error::{ModelError, TextError};
pub use hash::ContentHash;
pub use id::NodeId;
pub use model::{
    AgentConfig, AllowAllMeta, CaseState, DecisionConfig, DocumentAction, DocumentConfig,
    DraftMode, Edge, EmptyMeta, FormConfig, IntegrationConfig, JoinRule, Limits, LogicConfig,
    LogicType, MAX_LOOP_ITERATIONS, MetaView, Node, NodeKind, Participant, ParticipantKind,
    Participants, Process, ProcessVersion, RejectAction, RetryPolicy, Rule, RunStatus,
    SendMessageConfig, SubProcessConfig, TimeoutAction, TriggerConfig, TriggerType, UnknownNode,
    VersionStatus, WaitConfig, WaitMessageConfig, WaitType,
};
pub use text::{from_text, from_text_with_participants, to_text, to_text_with_participants};
pub use validate::{Position, Problem, codes, position_of, validate, validate_version};

/// The process definitions shipped with this crate, as data, in file order.
pub mod templates {
    /// An order-to-cash skeleton: a posted Sales Order starts the flow, a
    /// delivery is created and posted, a decision approves an exception and a
    /// notification closes it.
    pub const ORDER_TO_CASH: &str = include_str!("../templates/order_to_cash.toml");
    /// An approval with escalation: a decision, a wait, and an escalated
    /// decision on timeout.
    pub const APPROVAL_ESCALATION: &str = include_str!("../templates/approval_escalation.toml");
    /// Document then notify: create and post a document, then call an
    /// integration.
    pub const DOCUMENT_THEN_NOTIFY: &str = include_str!("../templates/document_then_notify.toml");
    /// An integration with retry and an error branch.
    pub const INTEGRATION_RETRY: &str = include_str!("../templates/integration_retry.toml");

    /// Every template, as `(file name, text)`.
    pub const ALL: [(&str, &str); 4] = [
        ("approval_escalation.toml", APPROVAL_ESCALATION),
        ("document_then_notify.toml", DOCUMENT_THEN_NOTIFY),
        ("integration_retry.toml", INTEGRATION_RETRY),
        ("order_to_cash.toml", ORDER_TO_CASH),
    ];
}

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-model";

/// The stable name a `spot-custom-bundle` section carrying process definitions
/// will use (WP-BPM-07 registers the section; this crate only fixes the name
/// and the text format).
pub const SECTION_KIND: &str = "processes";

/// The schema version of the process text form a bundle section carries.
pub const SECTION_SCHEMA_VERSION: u32 = 1;
