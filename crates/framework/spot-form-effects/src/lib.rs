#![forbid(unsafe_code)]
//! The UI effects protocol v1 and server-run TypeScript form scripts (WP-UI-12.T1; ADR-28).
//!
//! The client never runs app code. Imperative form logic runs on the server as TypeScript, and
//! what comes back is declarative: an [`EffectBatch`] the client applies in order, plus the
//! document diff the same event produced. The desktop and browser clients apply the same bytes
//! and are held to the same answer by the conformance corpus (see below), which both sides run.
//!
//! # Shape
//!
//! - [`effect`]: the closed protocol. [`Effect`], [`EffectBatch`], [`OptionItem`],
//!   [`MessageLevel`], the version and the additive evolution rules.
//! - [`path`]: field paths, `field` or `table[idx].field` with idx from 1.
//! - [`validate`]: the server's guarantee. Every effect is judged against the form's metadata,
//!   the triggering user's field rights and the registered dialogs; one that fails is **dropped**
//!   with an [`EffectProblem`], never applied. Order is preserved, state-setting duplicates
//!   collapse to their last occurrence, and caps bound the batch.
//! - [`record`]: [`FormScript`], the events (`load`, `change`, `save`, `action`) and the set that
//!   orders them by priority then name.
//! - [`runner`]: [`FormScriptRunner`] runs an event's scripts on the embedded engine
//!   ([`spot_ts`]) after the form-event endpoint's own diff, and returns the validated effects,
//!   the scripts' diff and a deterministic idempotency key. Scripts run as the triggering user,
//!   with the engine's time, memory and instruction limits; the only host functions are *emit an
//!   effect* and *log a line*, so there is no network, no filesystem and no other document.
//! - [`state`]: the reference form state an expected batch produces, which the corpus snapshots
//!   and the client conformance test (WP-UI-12.T2) asserts against.
//! - [`schema`]: `schema/effects.v1.json`, generated from the types and checked in.
//!
//! # A script
//!
//! A script is an ES module exporting the handler for its event. It receives the document, the
//! changed path (or `null`) and a context whose `effects` is the builder:
//!
//! ```ts
//! export function on_change(doc, changed, ctx) {
//!   if (changed === "qty") {
//!     ctx.effects.setValue("total", "100.00");
//!     ctx.effects.setReadOnly("rate", true);
//!   }
//!   return { patch: { changed_at: ctx.now } };
//! }
//! ```
//!
//! `ctx.effects` has one method per effect kind: `setValue`, `setReadOnly`, `setHidden`,
//! `setRequired`, `setOptions`, `setLabel`, `addAction`, `removeAction`, `showMessage`,
//! `showMessageKey`, `openDialog`, `focus`, `setFilter` and `batch`. A handler may also return
//! `{ patch, effects }`: the patch changes the document and its diff is part of the response,
//! the array appends effects after the ones it emitted.
//!
//! # Registration
//!
//! [`FormScript`] is the record the runner needs: a name, a DocType, an event (with its field or
//! action), TypeScript source, a priority and a timeout. A host builds them from its app
//! manifest's form-script part, or from records of the [`FORM_SCRIPT_DOCTYPE`] DocType, exactly
//! as WP-EXT-06's lifecycle scripts are loaded; [`FormScriptSet`] validates the records and
//! orders them. The field names of that DocType and the manifest part are the host's metadata,
//! which this crate does not own.
//!
//! # Security
//!
//! A script runs with the triggering user's rights. The document it receives is projected through
//! [`FormSecurity`] first, so a hidden field or hidden child column is simply absent; effects are
//! judged by the same authority, and a write to a field the user cannot write is dropped. The
//! host's [`FormSecurity`] adapter may also apply masking to the view; this crate's simple
//! [`PermissionSecurity`] reports visibility and writability only, because a masked value is
//! still visible (see `## Open questions`).
//!
//! # The corpus
//!
//! `conformance/forms/*.toml` holds the executable specification: a metadata fragment, a
//! document, an event, TypeScript source, and the expected effects, diff, problems and final
//! state. Each file opens with shared `[meta]`, `[children]` and `[doc]` fragments that its cases
//! inherit, so a case is self-contained given its file without repeating the DocType seventy
//! times. `tests/conformance.rs` runs every case on the real engine and checks the state snapshot
//! the client conformance test (WP-UI-12.T2) asserts against. The corpus reads no clock, no
//! random source and no site metadata.
//!
//! # What this crate does not do
//!
//! It does not wire itself into the HTTP endpoint: `spot-api-services` is outside this task's
//! owned paths. [`FormScriptRunner::run`] takes the endpoint's diff as `server_patch`; its
//! [`FormOutcome::merged_diff`] is what the endpoint returns. See `## Open questions` in the
//! report for the one adapter the host still needs.

pub mod codes;
mod effect;
mod path;
mod problem;
mod record;
mod runner;
mod schema;
mod security;
mod state;
mod validate;

pub use codes as error_codes;
pub use effect::{
    Effect, EffectBatch, MAX_BATCH_DEPTH, MessageLevel, OptionItem, PROTOCOL_VERSION,
};
pub use path::{PathError, TargetPath};
pub use problem::{EffectProblem, ScriptFailure};
pub use record::{FORM_SCRIPT_DOCTYPE, FormEvent, FormScript, FormScriptSet, MAX_PRIORITY};
pub use runner::{FormOutcome, FormScriptRunner, RunRequest, RunnerLimits, document_for, id_for};
pub use schema::{SCHEMA_PATH, effects_v1_schema, effects_v1_schema_text};
pub use security::{
    DialogRegistry, FieldAccess, FormMeta, FormSecurity, MapMeta, PermissionSecurity,
    StaticDialogs, StaticSecurity, field_facts,
};
pub use state::{ActionState, DialogState, FormState, MessageState};
pub use validate::{EffectContext, EffectLimits, Validated, Validator};

/// The catalogue code every caller checks first: a form script failed safely.
pub const SCRIPT_FAILED: &str = codes::SCRIPT_FAILED;
