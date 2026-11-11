//! The document engine: the real host of the app API (WP-DOC-04.T1; ADR-51, ADR-057).
//!
//! [`Engine`] implements [`DocumentService`], [`HookContext`](spot_app_api::HookContext) and
//! [`Repository`](spot_app_api::Repository) over **any** [`StorageBackend`]. It takes the
//! DocType definitions from [`spot_meta::EffectiveMeta`], the steps and seams from a
//! [`spot_seams::Registry`], and the values layer (defaults, coercion, validation, the storage
//! mapping) from this crate. The in-memory `StubHost` of the app API contract is its reference:
//! the engine behaves the same, and adds what the stub leaves out (storage, compiled metadata,
//! retries).
//!
//! This part covers **create, update, delete, load and field changes**, and the posted-document
//! operations of WP-DOC-04.T2: **post, cancel, correct (cancel and amend), rename, update of a
//! posted document and whitelisted methods** (ADR-42, ADR-16).
//!
//! # One transaction per top-level operation
//!
//! Each top-level operation opens one storage transaction labelled `<doctype>_<op>` (storage
//! names have no dots), at isolation `ReadCommitted` (ADR-46). The operation's document is
//! loaded with a row lock for update, and a change to an existing document is written under the
//! version the caller expected, so a lost update is impossible: it fails with
//! `document.stale_version`. A retryable storage error (serialization failure, deadlock,
//! unavailable) reruns the **whole** operation, with backoff, at most
//! [`RetryPolicy::max_retries`] times; each retry is recorded ([`Engine::retries`]). A
//! `CommitOutcomeUnknown` is never retried: the caller gets `server.unavailable` and retries
//! with its idempotency key, as nothing here can tell whether the commit happened.
//!
//! # The ADR-51 order
//!
//! The order is [`lifecycle::sequence`](spot_app_api::lifecycle::sequence). Around it:
//!
//! ```text
//! create:  defaults → normalise → before_* → normalise → validate_* → check → [write + event]
//!          → after_* → [commit] → *_committed
//! update:  (loaded, patched) → normalise → before_* → normalise → validate_* → check
//!          → [write + event] → after_* → [commit] → *_committed
//! delete:  before_delete → validate_delete → [delete + event] → after_delete → [commit]
//!          → delete_committed
//! ```
//!
//! *check* is [`validate::check`](crate::validate::check) against the effective metadata, so
//! `validate_*` steps see a coerced but not yet checked document and may add their own
//! messages and errors first. Steps come from the registry's plans, with overrides applied and
//! `run_wrapped` handled by the wrapper's context.
//!
//! # Phases, nesting and the outbox
//!
//! Every [`HookContext`](spot_app_api::HookContext) call is checked against the phase's
//! [`PhaseRules`](spot_app_api::PhaseRules). A phase violation, or any [`AppError`] from a step
//! in phases 1 to 3, aborts the operation and rolls the transaction back.
//!
//! A write from `after_*` runs the target's full lifecycle **in the same transaction**, inside a
//! savepoint, so an error the step handles leaves no half-done work behind. Jobs
//! ([`TOPIC_JOB`]), app events and realtime notices ([`TOPIC_NOTICE`]) enqueued in phase 3 are
//! written to the outbox in the same transaction, and exist only if it commits. Phase 4 (and
//! only it) enqueues in a small transaction of its own.
//!
//! # Committed steps
//!
//! After the commit, the `*_committed` steps run in order, each with its own context; their
//! writes are new top-level operations. A failure is logged and returned by
//! [`Engine::committed_failures`], and never rolls anything back. **In-process delivery is the
//! v0 behaviour** ([`InProcessDelivery`]): the steps run in the caller's task right after the
//! commit, so a crash between commit and delivery loses them. The platform's later delivery path
//! reads the lifecycle events from the outbox through the job engine (WP-PLT-06) and runs the
//! steps from there; that swap is one implementation of [`CommittedDelivery`].
//!
//! # Permissions
//!
//! Every operation is decided by the [`authz`] module's authorizer before it runs, fields it
//! writes are checked, reads are masked, link targets are scope-checked and lists are filtered
//! (WP-PLT-03.T2; ADR-070 section 12). The engine's `load`, `query` and write entry points all
//! go through it, so it is the single enforcement point for every channel.
//!
//! # Operation receipts
//!
//! Every lifecycle operation tells its caller what it did (WP-DOC-12.T1; ADR-080 section 6.4).
//! The steps report the effects they caused with `HookContext::report_effect`; the engine
//! collects them per operation, checks them against what the pipeline declares, and returns the
//! receipt with the result of the `*_with_receipt` methods ([`Engine::post_with_receipt`] and
//! its siblings). The [`DocumentService`] methods are unchanged and drop the receipt, because
//! the contract trait has no room for one. The `receipt` module says where a receipt is stored
//! and what it never holds; [`Engine::get_receipt`] reads one back later.
//!
//! # Not here
//!
//! Step time budgets (EXT-04) are not part of this task.

use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use crate::secrets::{SecretAuditRecord, SecretVault};
use spot_app_api::context::{AppCache, DocumentService, Invocation, Outcome};
use spot_app_api::prelude::{Clock, OffsetDateTime, RecordId, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, DocState, DocTypeDef, Document, DocumentPatch,
    ErrorClass,
};
use spot_meta::EffectiveMeta;
use spot_seams::Registry;
use spot_storage_contract::{Catalog, Collation, ErrorKind, StorageBackend};

mod authz;
mod cache;
mod capture;
mod committed;
mod ctx;
mod durable;
mod links;
mod naming;
mod ops;
mod outbox;
mod participant;
mod receipt;
mod secrets;
mod work;

pub use authz::{
    AllowAllAuthorizer, AppPermissionCheck, Authorizer, DenyAllAppPermissions,
    MemoryAppPermissions, MemoryPrincipals, PermissionAuthorizer, PrincipalSource, QueryPlan,
    WriteChange, apply_masks, create_changes, empty_page, mask_document, patch_changes,
};
pub use cache::MemoryCache;
pub use capture::{CaptureConfig, Exclusion, ExclusionKind, TOPIC_DOC_STATE};
pub use committed::{
    CommittedBatch, CommittedDelivery, CommittedStep, CommittedSteps, InProcessDelivery,
    OutboxSink, StepOutcome, StepSource,
};
pub use durable::{
    ARGS_SCHEMA, CommittedJobArgs, DurableDelivery, EngineRunner, JOB_COMMITTED_STEPS,
    committed_point, operation_named,
};
pub use naming::{BusinessZone, FixedZone, UtcZone};
pub use outbox::{TOPIC_JOB, TOPIC_NOTICE, TOPIC_SECRET_AUDIT, job_from_event, notice_from_event};
pub use participant::{
    Denial, DenialSink, NodeId, TxParticipant, WriteCause, WriteIntent, WriteRecord,
};
pub use receipt::{
    MAX_EFFECTS_PER_RECEIPT, MAX_REFS_PER_RECEIPT, ReceiptRecord, Receipted, TOPIC_RECEIPT,
    TOPIC_RECEIPT_EFFECT, UndeclaredEffect, rows_digest,
};

use ops::Req;
use participant::Participants;

/// Where record ids come from: the ids of events and of child rows the engine creates. Hosts use
/// time-ordered UUIDv7s; tests may count.
pub trait IdGenerator: Send + Sync + 'static {
    /// A fresh id.
    fn next_id(&self) -> RecordId;
}

/// UUIDv7 ids from the system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct V7Ids;

impl IdGenerator for V7Ids {
    fn next_id(&self) -> RecordId {
        RecordId::new_v7()
    }
}

/// How a transaction that failed with a retryable storage error is rerun (ADR-46).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The most times one operation is rerun after the first attempt.
    pub max_retries: u32,
    /// The wait before the first retry; it doubles for each further one.
    pub base_delay: Duration,
    /// The longest wait between two attempts.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// The wait before retry number `attempt` (from 1).
    pub fn delay(&self, attempt: u32) -> Duration {
        let doublings = attempt.saturating_sub(1).min(16);
        self.base_delay
            .saturating_mul(1u32 << doublings)
            .min(self.max_delay)
    }
}

impl Default for RetryPolicy {
    /// Three retries, waiting 10 ms, 20 ms and 40 ms.
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(200),
        }
    }
}

/// One rerun of an operation, for telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryRecord {
    /// The transaction label, `<doctype>_<op>`.
    pub label: String,
    /// Which retry this was, from 1.
    pub attempt: u32,
    /// The storage error that caused it.
    pub cause: ErrorKind,
    /// When it was decided.
    pub at: OffsetDateTime,
}

/// A committed step that failed. The document it ran for is committed regardless.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedFailure {
    /// The hook point, for example `create_committed`.
    pub point: String,
    /// The DocType of the document the step ran for.
    pub doctype: String,
    /// The document.
    pub id: RecordId,
    /// What the step returned.
    pub error: AppError,
    /// When it failed.
    pub at: OffsetDateTime,
}

/// One permission refusal, for telemetry. The engine counts every refusal the authorizer
/// raises, alongside the retry log (WP-PLT-03.T2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenialRecord {
    /// The error code: `permission.denied` or `permission.field_denied`.
    pub code: String,
    /// The action that was refused, when the error names one.
    pub action: String,
    /// The DocType, when the error names one.
    pub doctype: String,
    /// The reason codes from the decision, in the error's `reason` detail.
    pub reason: String,
    /// When it was refused.
    pub at: OffsetDateTime,
}

/// Metadata, its compiled catalog and the step registry at one moment: what one operation
/// runs against from start to finish, however the engine is reconfigured meanwhile (ADR-05).
pub(crate) struct Snapshot {
    pub(crate) meta: Arc<EffectiveMeta>,
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) seams: Arc<Registry>,
    /// The compiled naming plans of `meta` (WP-DOC-05.T2).
    pub(crate) naming: naming::NamingTable,
}

impl Snapshot {
    /// The app view of a DocType.
    pub(crate) fn doctype(&self, name: &str) -> AppResult<Arc<DocTypeDef>> {
        self.meta.doctype(name).ok_or_else(|| {
            AppError::new(
                ErrorClass::UnknownDocType,
                "meta.unknown_doctype",
                format!("The DocType `{name}` does not exist."),
            )
        })
    }
}

/// The document engine over a storage backend.
pub struct Engine<B: StorageBackend> {
    pub(crate) backend: B,
    state: RwLock<Arc<Snapshot>>,
    /// The controllers whose whitelisted methods the engine dispatches (ADR-16). Swapped when
    /// apps are installed or removed, like the seam registry.
    controllers: RwLock<Arc<Vec<Arc<dyn Controller>>>>,
    /// The transaction participants run around every document write (WP-DOC-04.T3).
    participants: RwLock<Participants<B::Tx>>,
    /// Told about each refused operation, after its rollback (WP-DOC-04.T5).
    denial_sink: RwLock<Option<Arc<dyn DenialSink>>>,
    /// How long the denial sink may take; a longer call is dropped and logged.
    denial_timeout: Duration,
    /// The deployment node this engine writes for, if the deployment has more than one.
    node: Option<NodeId>,
    collation: Collation,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) ids: Arc<dyn IdGenerator>,
    pub(crate) retry: RetryPolicy,
    pub(crate) delivery: Arc<dyn CommittedDelivery>,
    pub(crate) cache: Arc<dyn AppCache>,
    /// The engine's one dependency on permissions (WP-PLT-03.T2). Defaults to
    /// [`AllowAllAuthorizer`]; a host installs a [`PermissionAuthorizer`] with
    /// [`Engine::with_authorizer`].
    pub(crate) authorizer: Arc<dyn Authorizer>,
    /// Who holds an app-registered permission, for methods declaring `right = Permission(..)`
    /// (WP-DOC-11.T3). Defaults to [`DenyAllAppPermissions`]: fail closed.
    pub(crate) app_permissions: Arc<dyn AppPermissionCheck>,
    /// Seals, opens and blind-indexes `secret` fields; without one every secret operation
    /// fails closed with `secretfield.not_configured` (WP-SEC-03.T3).
    pub(crate) vault: Option<Arc<dyn SecretVault>>,
    /// The business time zone of each site, for the date parts of a document name (ADR-077).
    pub(crate) zone: Arc<dyn BusinessZone>,
    /// Where the `hash` naming strategy gets its ids.
    pub(crate) naming_ids: Arc<dyn spot_naming::NamingIdSource>,
    /// What is captured as `doc_state` events, and what is excluded (WP-JRN-02.T2).
    pub(crate) capture: capture::Capture,
    failures: Mutex<Vec<CommittedFailure>>,
    retries: Mutex<Vec<RetryRecord>>,
    denials: Mutex<Vec<DenialRecord>>,
    /// Refusals answered as `method.not_found` so as not to reveal the method or document
    /// (WP-DOC-11.T3); kept apart from `denials`, which holds what callers saw as a refusal.
    masked_denials: Mutex<Vec<DenialRecord>>,
    /// The last secret operations of this process, for [`Engine::secret_audit_log`]. A view for
    /// diagnostics only: the record is the journal in the outbox (see `engine/secrets.rs`).
    secret_audit: Mutex<Vec<SecretAuditRecord>>,
    /// Where each `(site, DocType)`'s rewrap sweep stands, for the lookup rotation window. The
    /// site is part of the blind-index input, so a sweep under one site says nothing about
    /// another's records (WP-SEC-03.T7).
    rewrap_sweeps: Mutex<std::collections::BTreeMap<(String, String), secrets::Sweep>>,
    /// Whether an operation whose steps report an undeclared effect fails (WP-DOC-12.T1). On by
    /// default in this crate's unit tests, off otherwise: see [`Engine::with_strict_effects`].
    pub(crate) strict_effects: bool,
    /// Effects reported without a declaration (the last 256), for [`Engine::undeclared_effects`].
    undeclared: Mutex<Vec<UndeclaredEffect>>,
}

/// How long a [`DenialSink`] may take by default.
const DEFAULT_DENIAL_TIMEOUT: Duration = Duration::from_secs(2);

/// Retry records kept for inspection; older ones are dropped.
const RETRY_LOG: usize = 256;

/// Denial records kept for inspection; older ones are dropped.
const DENIAL_LOG: usize = 256;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Guarded data stays consistent: every critical section is a plain push or read.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn compile(meta: &EffectiveMeta, collation: &Collation) -> AppResult<Arc<Catalog>> {
    meta.catalog(collation.clone()).map(Arc::new).map_err(|e| {
        AppError::new(
            ErrorClass::Internal,
            "meta.validation_failed",
            format!("The metadata does not compile to a storage catalog: {e}"),
        )
    })
}

impl<B: StorageBackend> Engine<B> {
    /// An engine over `backend`, running `meta` with the steps and seams of `seams`.
    ///
    /// The catalog is compiled here, with the collation `und`; call [`Engine::prepare`] once to
    /// make the store able to hold it. The engine reads time only through `clock` and takes the
    /// ids of events and child rows from `ids`.
    ///
    /// # Errors
    ///
    /// `meta.validation_failed` if the metadata does not compile to a catalog.
    pub fn new(
        backend: B,
        meta: Arc<EffectiveMeta>,
        seams: Arc<Registry>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> AppResult<Self> {
        let collation = Collation::new("und").map_err(|e| {
            AppError::new(
                ErrorClass::Internal,
                "meta.validation_failed",
                e.to_string(),
            )
        })?;
        let catalog = compile(&meta, &collation)?;
        let naming = naming::NamingTable::build(&meta)?;
        let cache = Arc::new(MemoryCache::new(clock.clone()));
        Ok(Self {
            backend,
            state: RwLock::new(Arc::new(Snapshot {
                meta,
                catalog,
                seams,
                naming,
            })),
            controllers: RwLock::new(Arc::new(Vec::new())),
            participants: RwLock::new(Arc::new(Vec::new())),
            denial_sink: RwLock::new(None),
            denial_timeout: DEFAULT_DENIAL_TIMEOUT,
            node: None,
            collation,
            clock,
            ids,
            retry: RetryPolicy::default(),
            delivery: Arc::new(InProcessDelivery),
            cache,
            authorizer: Arc::new(AllowAllAuthorizer),
            app_permissions: Arc::new(DenyAllAppPermissions),
            vault: None,
            zone: Arc::new(UtcZone),
            naming_ids: Arc::new(spot_naming::UuidV7Source),
            capture: capture::Capture::default(),
            failures: Mutex::new(Vec::new()),
            retries: Mutex::new(Vec::new()),
            denials: Mutex::new(Vec::new()),
            masked_denials: Mutex::new(Vec::new()),
            secret_audit: Mutex::new(Vec::new()),
            rewrap_sweeps: Mutex::new(std::collections::BTreeMap::new()),
            strict_effects: cfg!(test),
            undeclared: Mutex::new(Vec::new()),
        })
    }

    /// The same engine enforcing permissions through `authorizer` (WP-PLT-03.T2). Without this
    /// the engine keeps its pre-permission behaviour ([`AllowAllAuthorizer`]).
    #[must_use]
    pub fn with_authorizer(mut self, authorizer: Arc<dyn Authorizer>) -> Self {
        self.authorizer = authorizer;
        self
    }

    /// The same engine deciding app permissions (a method declaring `right = Permission(name)`)
    /// through `check` (WP-DOC-11.T3). Without this nobody holds any: such a method is refused.
    #[must_use]
    pub fn with_app_permissions(mut self, check: Arc<dyn AppPermissionCheck>) -> Self {
        self.app_permissions = check;
        self
    }

    /// The same engine sealing, opening and blind-indexing `secret` fields with `vault`
    /// (WP-SEC-03.T3). Without one the engine refuses every secret operation.
    ///
    /// Use one vault per tenant where the key arrangement allows it. Whatever it is, the site
    /// of each invocation is also part of every value's associated data and of every blind
    /// index, so tenants that share a key still cannot read or match each other's values.
    #[must_use]
    pub fn with_secrets(mut self, vault: Arc<dyn SecretVault>) -> Self {
        self.vault = Some(vault);
        self
    }

    /// The same engine dating document names in the business zone `zone` gives for each site
    /// (WP-DOC-05.T2). Without this every site is dated in UTC.
    #[must_use]
    pub fn with_business_zone(mut self, zone: Arc<dyn BusinessZone>) -> Self {
        self.zone = zone;
        self
    }

    /// The same engine taking the ids of `hash`-named documents from `ids` (tests make them
    /// deterministic).
    #[must_use]
    pub fn with_naming_ids(mut self, ids: Arc<dyn spot_naming::NamingIdSource>) -> Self {
        self.naming_ids = ids;
        self
    }

    /// The same engine with another retry policy.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The same engine delivering committed steps through `delivery` (default:
    /// [`InProcessDelivery`]).
    #[must_use]
    pub fn with_delivery(mut self, delivery: Arc<dyn CommittedDelivery>) -> Self {
        self.delivery = delivery;
        self
    }

    /// The same engine using `cache` for the steps' `cache()` (default: a [`MemoryCache`]).
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<dyn AppCache>) -> Self {
        self.cache = cache;
        self
    }

    /// Makes the store able to hold the current catalog (additive, see
    /// [`StorageBackend::prepare`]).
    ///
    /// # Errors
    ///
    /// The mapped storage error.
    pub async fn prepare(&self) -> AppResult<()> {
        let snap = self.snapshot();
        self.backend.prepare(&snap.catalog).await.map_err(|e| {
            spot_app_api::error::map_storage_error(&e, spot_app_api::error::StorageOp::Other)
        })
    }

    /// Switches to new effective metadata: compiles the catalog, prepares storage for it, and
    /// applies it to the operations that start afterwards. Running operations finish on the
    /// metadata they started with.
    ///
    /// # Errors
    ///
    /// `meta.validation_failed`, or the mapped storage error; the engine keeps the old metadata.
    pub async fn set_meta(&self, meta: Arc<EffectiveMeta>) -> AppResult<()> {
        let catalog = compile(&meta, &self.collation)?;
        let naming = naming::NamingTable::build(&meta)?;
        self.backend.prepare(&catalog).await.map_err(|e| {
            spot_app_api::error::map_storage_error(&e, spot_app_api::error::StorageOp::Other)
        })?;
        let seams = self.snapshot().seams.clone();
        self.swap(Snapshot {
            meta,
            catalog,
            seams,
            naming,
        });
        Ok(())
    }

    /// Switches to a new step and seam registry (an app was installed or removed).
    pub fn set_seams(&self, seams: Arc<Registry>) {
        let old = self.snapshot();
        self.swap(Snapshot {
            meta: old.meta.clone(),
            catalog: old.catalog.clone(),
            seams,
            naming: old.naming.clone(),
        });
    }

    /// The same engine dispatching whitelisted methods (ADR-16) to `controllers`.
    ///
    /// The engine finds a method by scanning the controllers registered for the document's
    /// DocType. A host that builds its controllers through [`RegistryBuilder`](spot_seams) hands
    /// them in here; `spot-controllers` (WP-DOC-10) will do the same once it exists.
    #[must_use]
    pub fn with_controllers(self, controllers: Vec<Arc<dyn Controller>>) -> Self {
        self.set_controllers(controllers);
        self
    }

    /// Replaces the controllers whitelisted methods are dispatched to (an app was installed or
    /// removed). Operations that already started keep the controllers they began with.
    pub fn set_controllers(&self, controllers: Vec<Arc<dyn Controller>>) {
        let mut guard = self.controllers.write().unwrap_or_else(|p| p.into_inner());
        *guard = Arc::new(controllers);
    }

    /// Replaces the transaction participants (an app was installed or removed), like
    /// [`set_seams`](Self::set_seams). They run in the given order around every document write,
    /// in the document's own transaction; see [`TxParticipant`]. A write that already started
    /// keeps the participants it began with.
    pub fn set_participants(&self, participants: Vec<Arc<dyn TxParticipant<B::Tx>>>) {
        let mut guard = self.participants.write().unwrap_or_else(|p| p.into_inner());
        *guard = Arc::new(participants);
    }

    /// The same engine writing as `node` of a multi-node deployment (ADR-062): participants see
    /// it as [`WriteIntent::node`] and [`WriteRecord::node`]. Unset in a single-node site.
    #[must_use]
    pub fn with_node(mut self, node: NodeId) -> Self {
        self.node = Some(node);
        self
    }

    /// Installs the sink told about each refused operation (WP-DOC-04.T5), replacing any
    /// earlier one. It is called once per refused top-level operation, after the transaction has
    /// rolled back, for a refusal by permissions, a version conflict, a validation failure or a
    /// participant. Never for a missing document, a transient storage error or a read. With no
    /// sink installed nothing is reported.
    pub fn set_denial_sink(&self, sink: Arc<dyn DenialSink>) {
        let mut guard = self.denial_sink.write().unwrap_or_else(|p| p.into_inner());
        *guard = Some(sink);
    }

    /// The same engine giving the denial sink `timeout` instead of two seconds.
    #[must_use]
    pub fn with_denial_timeout(mut self, timeout: Duration) -> Self {
        self.denial_timeout = timeout;
        self
    }

    pub(crate) fn denial_sink(&self) -> Option<Arc<dyn DenialSink>> {
        self.denial_sink
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The participants now registered, as one write sees them.
    pub(crate) fn participants(&self) -> Participants<B::Tx> {
        self.participants
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The controller declaring `method` on `doctype`, if any.
    pub(crate) fn find_method(&self, doctype: &str, method: &str) -> Option<Arc<dyn Controller>> {
        let controllers = self
            .controllers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        controllers.iter().find_map(|c| {
            (c.doctype() == doctype && c.methods().iter().any(|m| m.name.as_str() == method))
                .then(|| c.clone())
        })
    }

    fn swap(&self, snap: Snapshot) {
        let mut guard = self.state.write().unwrap_or_else(|p| p.into_inner());
        *guard = Arc::new(snap);
    }

    pub(crate) fn snapshot(&self) -> Arc<Snapshot> {
        self.state.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The storage backend, for inspection and for hosts that share it.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The committed steps that failed so far, oldest first.
    pub fn committed_failures(&self) -> Vec<CommittedFailure> {
        lock(&self.failures).clone()
    }

    /// The reruns decided so far (the last 256), oldest first.
    pub fn retries(&self) -> Vec<RetryRecord> {
        lock(&self.retries).clone()
    }

    /// The refusals of a method call that the caller saw as `method.not_found` because it could
    /// not read the document (the last 256), oldest first (WP-DOC-11.T3). They are audited here
    /// and not in [`denial_log`](Self::denial_log), whose entries are the refusals callers saw.
    pub fn masked_denial_log(&self) -> Vec<DenialRecord> {
        lock(&self.masked_denials).clone()
    }

    /// The permission refusals so far (the last 256), oldest first (WP-PLT-03.T2).
    pub fn denial_log(&self) -> Vec<DenialRecord> {
        lock(&self.denials).clone()
    }

    /// How many permission refusals the engine has raised.
    pub fn denial_count(&self) -> usize {
        lock(&self.denials).len()
    }

    /// Records a refusal in the denial log and returns the error unchanged, so call-sites read
    /// `map_err(|e| self.denied(e))`.
    pub(crate) fn denied(&self, error: AppError) -> AppError {
        self.record_denial(&error);
        error
    }

    pub(crate) fn record_denial(&self, error: &AppError) {
        self.push_denial(&self.denials, error);
    }

    /// Records a refusal that the caller is told is `method.not_found`.
    pub(crate) fn record_masked_denial(&self, error: &AppError) {
        self.push_denial(&self.masked_denials, error);
    }

    fn push_denial(&self, log: &Mutex<Vec<DenialRecord>>, error: &AppError) {
        let field = |key: &str| {
            error
                .details
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let mut log = lock(log);
        if log.len() >= DENIAL_LOG {
            log.remove(0);
        }
        log.push(DenialRecord {
            code: error.code.clone(),
            action: field("action"),
            doctype: field("doctype"),
            reason: field("reason"),
            at: self.clock.now_utc(),
        });
    }

    pub(crate) fn record_failure(&self, failure: CommittedFailure) {
        lock(&self.failures).push(failure);
    }

    pub(crate) fn record_retry(&self, label: &str, attempt: u32, cause: ErrorKind) {
        let mut log = lock(&self.retries);
        if log.len() >= RETRY_LOG {
            log.remove(0);
        }
        log.push(RetryRecord {
            label: label.to_owned(),
            attempt,
            cause,
            at: self.clock.now_utc(),
        });
    }

    /// A document as storage holds it, without running the `load` steps: `None` if it does not
    /// exist. For inspection and tests; users load through [`DocumentService::load`].
    ///
    /// # Errors
    ///
    /// `meta.unknown_doctype`, or the mapped storage error.
    pub async fn peek(&self, doctype: &str, id: RecordId) -> AppResult<Option<Document>> {
        let snap = self.snapshot();
        self.read_document(&snap, doctype, id).await
    }
}

impl<B: StorageBackend> DocumentService for Engine<B> {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async move { self.snapshot().doctype(doctype) })
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(self.load_document(inv, doctype, id))
    }

    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { Ok(self.create_with_receipt(inv, doc).await?.outcome) })
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let done = self
                .update_with_receipt(inv, doctype, id, patch, expected)
                .await?;
            Ok(done.outcome)
        })
    }

    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let done = self.post_with_receipt(inv, doctype, id, expected).await?;
            Ok(done.outcome)
        })
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let done = self.cancel_with_receipt(inv, doctype, id, expected).await?;
            Ok(done.outcome)
        })
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let done = self
                .correct_with_receipt(inv, doctype, id, expected)
                .await?;
            Ok(done.outcome)
        })
    }

    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        Box::pin(async move {
            let done = self.delete_with_receipt(inv, doctype, id, expected).await?;
            Ok(done.outcome)
        })
    }

    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let done = self
                .rename_with_receipt(inv, doctype, id, new_name, expected)
                .await?;
            Ok(done.outcome)
        })
    }

    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: serde_json::Value,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
        Box::pin(async move {
            let req = Req::Method {
                doctype: doctype.to_owned(),
                id,
                method: method.to_owned(),
                args,
                expected,
            };
            self.top(inv, req).await?.into_doc_json()
        })
    }

    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(self.field_changed_draft(inv, draft, field))
    }
}

/// The state a created document starts in: `Draft` for a posted DocType, none otherwise.
pub(crate) fn initial_state(def: &DocTypeDef) -> Option<DocState> {
    def.is_posted.then_some(DocState::Draft)
}
