//! `spot-bpm-entry` — process-first entry for the Business Process engine (WS8, WP-BPM-13.T1;
//! ADR-080 sections 2 and 3, with the owner's decision of 17 Oct 2026: no bypass in
//! `process_only`).
//!
//! Everything in a business can be a process, adding an employee or an item as much as a
//! purchase. This crate is the server side of starting one:
//!
//! * [`FormStepHandler`] and [`FormService`]: the form step. A named person or role enters a set
//!   of fields; the output is an ordinary **draft record**, created or updated through the
//!   record type's lifecycle as that person and flagged as owned by the request. Completing the
//!   step and saving the draft commit together ([`Unit::commit_with`]); a validation refusal
//!   keeps the step open and returns the field errors. See [`form`] and [`complete`].
//! * [`OwnershipController`]: while a draft is owned by a running request it can be changed only
//!   through that request's steps; other channels get `bpm_entry.owned_by_request` naming the
//!   request. A request that ends discards or releases its drafts per the step's `on_abort`
//!   ([`abort_drafts`]).
//! * [`ProcessOnlyGuard`] and [`ProcessOnlyController`]: in a `process_only` scope a create from
//!   any channel is refused unless it carries a valid request context for the type's entry
//!   process, and the refusal names the process to start. **No outside-process permission
//!   applies in this mode**; the guard takes no principal at all.
//! * [`EntryProcesses`]: the entry process of a type from the setting `bpm.entry.<type>`, with a
//!   generated minimal default ([`generate_entry_process`]) so `process_only` works with no
//!   authoring; [`ScopedPolicyStore`] holds the mode per type with scope narrowing,
//!   effective-dated and audited ([`ModeAdmin`]).
//! * [`BatchRunner`]: one request that owns many records, for imports: checkpoints, per-item
//!   outcomes, resume, per-item failures reported rather than aborting (ADR-071 semantics).
//! * [`RunLabels`]: the user-facing name of a run, "Request" by default, from the localisation
//!   catalogue keys `bpm.run.singular` and `bpm.run.plural`.
//!
//! # Purity
//!
//! No wall clock (time is always an argument), no floats (ADR-12), no `unsafe`, no
//! `unwrap`/`expect` outside tests, and no secret in any message (the project coding rules).

#![forbid(unsafe_code)]

pub mod approvals;
pub mod batch;
pub mod codes;
pub mod complete;
pub mod entry;
pub mod error;
pub mod form;
pub mod guard;
pub mod inbox;
pub mod label;
pub mod ownership;
pub mod scope;

pub use approvals::{AUTO_APPROVED_TEXT, ApprovalDisplay, ApprovalRequirement, ApprovalStanding};
pub use batch::{
    BatchControl, BatchCounters, BatchIdentity, BatchInput, BatchReport, BatchRun, BatchRunner,
    BatchSpec, BatchStatus, BatchStore, FailurePolicy, ItemApplier, ItemContext, ItemFailure,
    ItemOutcome, MemoryBatchStore, ProblemItem, RequestStarter,
};
pub use complete::{
    CompletionPlanner, DagSource, DraftSave, DraftWriter, ExecPlanner, FormCompleted, FormConfigs,
    FormService, FormSubmission, PlannedCompletion, StaticDag,
};
pub use entry::{
    ENTRY_KEY_PREFIX, EntryProcesses, EntrySettings, MemoryEntrySettings, ProcessRef,
    entry_setting_key, generate_entry_process, generated_process_name,
};
pub use error::{EntryError, EntryResult, FieldIssue};
pub use form::{
    FORM_KIND_TOKEN, FormNodeConfig, FormStepHandler, Inbox, InboxItem, Ticket, TicketState,
};
pub use guard::{
    CreateRoute, EntryAttempt, MemoryRequests, MemoryScopes, NoRequests, ProcessOnlyController,
    ProcessOnlyGuard, RequestAuthority, ScopeResolver, VerifiedRequest,
};
pub use inbox::{NoSignalEngine, TaskInbox};
pub use label::{
    EmptyCatalogue, LabelCatalogue, MemoryCatalogue, RunLabel, RunLabels, user_visible_strings,
};
pub use ownership::{
    AbortPolicy, AbortReport, DocumentServiceDrafts, DraftLifecycle, DraftRef, OWNER_FIELD,
    OwnershipController, RequestContext, abort_drafts,
};
pub use scope::{ModeAdmin, ScopedPolicyStore, scope_chain};
pub use spot_bpm_tasks::is_person;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-entry";
