//! `spot-bpm-triggers` — what starts a Case (WS8, WP-BPM-03.T1, ADR-49, ADR-080).
//!
//! A process version has one or more **trigger nodes**; a trigger node is the entry point that
//! starts a run. This crate turns those nodes into [`TriggerSpec`]s, matches the events and
//! requests that reach a site against the published versions, and starts **exactly one Case per
//! source event**, however many times the event is redelivered or however many nodes race.
//!
//! # The pieces
//!
//! - [`TriggerSpec`]: one trigger node's config, read from the model ([`spec`]).
//! - [`TriggerRegistry`]: the open registry of trigger **kinds** (ADR-080). The five built-in
//!   kinds — document event, schedule, webhook, manual and API — are registered through the same
//!   [`TriggerRegistry::register_kind`] a later work package uses to add a kind such as
//!   `message_arrived`; no behaviour change is needed to extend it.
//! - [`TriggerIndex`]: built from published process versions, it returns the `(process, version)`
//!   triggers a source event matches. Matching is a pure function; document-event conditions are
//!   evaluated with `spot-expr`, and manual/API starts are checked with `spot-permissions`.
//! - [`TriggerIntake`]: processes batches of source events, derives the **start key**
//!   `(process, trigger node, source event id)` and calls [`CaseStarter::start`]. A repeated key
//!   returns the existing Case ([`StartOutcome::Existing`]); a rejected event is reported, never
//!   dropped silently.
//! - [`webhook`]: incoming webhooks are verified with `spot_webhooks::signing` **before** the
//!   body is parsed, and keyed by a caller-supplied idempotency header or the body hash.
//!
//! # Exactly-once
//!
//! The start key is the whole of the guarantee: the same source event id under the same process
//! and trigger node produces the same key on every node and after every restart, so the
//! [`CaseStarter`] returns the Case it already started instead of a second one. Schedule slots
//! carry their own deduplication key from `spot-scheduler`, so two scheduler instances ticking
//! the same slot produce one key too.
//!
//! # Purity
//!
//! Time is an argument, never read from a wall clock. No `unsafe`, no floats, no `unwrap`/`expect`
//! outside tests, and no secret is ever logged or returned.

#![forbid(unsafe_code)]

pub mod error;
pub mod index;
pub mod input;
pub mod intake;
pub mod permission;
pub mod registry;
pub mod source;
pub mod spec;
pub mod starter;
pub mod webhook;

pub use error::{Error, Result, TriggerErrorCode};
pub use index::{MatchRejection, MatchReport, TriggerIndex, TriggerMatch};
pub use input::validate_input;
pub use intake::{IntakeReport, RejectedEvent, TriggerIntake};
pub use permission::{AllowAll, PermissionGate, StartGate, StartPermission};
pub use registry::{TriggerMatcher, TriggerRegistry, TriggerStarter};
pub use source::{
    ApiStart, CustomEvent, DocFactsSource, EventImageFacts, ManualStart, Source, SourceEvent,
    WebhookEvent, check_complete,
};
pub use spec::{
    ApiSpec, CustomSpec, DocumentEventSpec, ManualSpec, ScheduleSpec, TriggerSpec, WebhookSpec,
    parse_operation,
};
pub use starter::{CaseStarter, InMemoryCaseStarter, StartInput, StartKey, StartOutcome};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-triggers";

/// The kind token of a document-event trigger.
pub const KIND_DOCUMENT_EVENT: &str = "document_event";
/// The kind token of a schedule trigger.
pub const KIND_SCHEDULE: &str = "schedule";
/// The kind token of an incoming-webhook trigger.
pub const KIND_WEBHOOK: &str = "webhook";
/// The kind token of a manual trigger.
pub const KIND_MANUAL: &str = "manual";
/// The kind token of an API trigger.
pub const KIND_API: &str = "api";

/// The signature header an incoming webhook carries (re-exported from `spot-webhooks`).
pub const SIGNATURE_HEADER: &str = spot_webhooks::SIGNATURE_HEADER;
/// The default idempotency header an incoming webhook may carry.
pub const IDEMPOTENCY_HEADER: &str = "X-Spot-Idempotency-Key";
