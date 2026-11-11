//! Tamper-evident audit of every document change (WP-SEC-06.T1).
//!
//! `spot-audit-chain` proves a chain of audit records was not altered; `spot-audit-store` is
//! where one chain lives. This crate connects the document engine to it **without making the
//! whole site queue behind one chain head**.
//!
//! # The design
//!
//! A single hash chain has one head, so appending to it inside every business transaction would
//! serialise *all* writers on that head. Instead:
//!
//! 1. The business transaction writes an ordinary **audit event to the outbox**, in the same
//!    transaction as the change, so the event cannot be lost and cannot survive a rollback
//!    (ADR-011, ADR-056, ADR-061). [`AuditStep`] builds the event and an [`AuditWriter`] writes
//!    it; [`enqueue_audit`] is the exact outbox enqueue.
//! 2. A separate appender chains those events afterwards, **in commit order per lane**, where a
//!    lane is `hash(subject doctype + id) mod N` ([`lane_of`]). [`LaneAppender`] reads one lane's
//!    events from the outbox intake and appends each to that lane's [`AuditChain`] with
//!    [`spot_audit_chain::AuditChain::append`]. `N` lanes means `N` independent chains, so no
//!    lock is shared on the write path and no lane's head serialises another lane.
//! 3. A small **head digest** hashes every lane head into one value ([`HeadDigest`]), and a signed
//!    **site checkpoint** binds it ([`SiteCheckpoint`]), so removing a whole lane or rolling
//!    several lanes back together is detected too.
//!
//! # The exact guarantee
//!
//! *An event cannot be lost or altered after it is chained; between commit and chaining it is
//! protected only by the outbox's own durability, and the lag is bounded and visible.*
//! [`DocAudit::audit_lag`] and [`DocAudit::status`] report the oldest unchained event's age and
//! the pending count per lane; an alert threshold can fail readiness and, by policy, stop new
//! posting operations (the default is **warn only**).
//!
//! # Limits
//!
//! - Between commit and chaining the event is only as durable as the outbox; the window is
//!   bounded and visible, never silent.
//! - A chain alone cannot show that its newest entries were cut off; only the signed site
//!   checkpoint (kept here, or better held by a third party) can name the missing tail. That is
//!   what [`DocAudit::verify_site`] with a trusted checkpoint checks.
//! - The stored diff holds the **masked** form of a secured field (ADR-070 section 12 rule 9);
//!   the real value lives only where the field lives. A reader's history is redacted again for
//!   that reader through the field guard's `Channel::Version`.
//! - The chain's payload limit (`spot_audit_chain::MAX_PAYLOAD_BYTES`, 8 KiB) is smaller than the
//!   default event cap ([`MAX_EVENT_BYTES`], 32 KiB); an event over the chain limit is chained in
//!   a compact form with the digest of the full event. See the task report's open questions.
//!
//! # Not here
//!
//! No chain, no store, no engine: this crate builds events, shards them into lanes, appends them
//! and reads them back. It reads no clock directly (callers pass time and keys in) and holds no
//! float and no `unsafe`.

pub mod appender;
pub mod denial;
pub mod error;
pub mod event;
pub mod install;
pub mod job;
pub mod lane;
pub mod masker;
pub mod participant;
pub mod secret;
pub mod service;
pub mod step;

pub use appender::{
    AppendResult, AppenderStore, DrainReport, InMemoryAppenderStore, LaneAppender, OutboxFeed,
    SharedStore, decode_outbox, event_of, record_of,
};
pub use denial::{AuditDenialSink, DEFAULT_MAX_IN_FLIGHT, DenialStats, UNKNOWN_ID};
pub use error::DocAuditError;
pub use event::{
    ActorRef, AuditSource, DiffChange, DiffValue, DocAuditEvent, DocOperation, DocumentImage,
    EventExtra, EventSizePolicy, LONG_VALUE_BYTES, MAX_CHANGES, MAX_EVENT_BYTES, MAX_METHOD_BYTES,
    OUTCOME_DENIED, build_event, build_event_with, cap_event, decode_event, diff_images,
    encode_event, is_recordable_method, sha256_hex, value_to_json,
};
pub use install::{
    ParticipantRole, Placed, Role, checked_participants, register, with_audit_participant,
};
pub use job::{AUDIT_APPEND_JOB_PREFIX, append_job_name, job_lane, register_appender};
pub use lane::{HeadDigest, LaneConfig, LaneHead, SiteCheckpoint, lane_of, lane_site};
pub use masker::{AuditMask, FieldMasker, GuardMasker};
pub use participant::{AuditParticipant, operation_for};
pub use secret::{
    SECRET_AUDIT_TOPIC, SECRET_EVENT_KEYS, SECRET_KIND_PREFIX, SecretAuditEvent,
    decode_secret_outbox, encode_secret_event, is_secret_audit, is_secret_entry, secret_event_of,
    secret_record_of,
};
pub use service::{
    AuditLag, AuditPolicy, AuditStatus, DocAudit, HistoryEntry, LaneFailure, SiteVerifyReport,
    TRUNCATED_KIND,
};
pub use step::{
    AUDIT_OFF_DOCTYPES, AUDIT_TOPIC, AuditStep, AuditWriter, BeforeImages, FailingWriter,
    FixedBeforeImages, NoBeforeImages, SCHEMA_VERSION, enqueue_audit, from_micros, is_audit_off,
    is_audit_step_name, micros, operation_of_point, operation_of_step, outbox_event_of,
    record_id_of,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-doc-audit";
