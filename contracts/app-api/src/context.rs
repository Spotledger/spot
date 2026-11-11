//! What controller code works with: the invocation, the repository, hook and method contexts,
//! and the document service that hosts expose to the native API and to tests.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use spot_core::SiteId;
use spot_events_contract::{CommandId, Topic};
use spot_storage_contract::{Name, Query, RecordId, Rows, Version};
use time::OffsetDateTime;

pub use spot_events_contract::{Actor, PrincipalKind, PrincipalRef, SYSTEM_PRINCIPAL};

use crate::BoxFuture;
use crate::document::{Document, DocumentPatch};
use crate::effects::EffectReceipt;
use crate::error::AppResult;
use crate::lifecycle::HookPoint;
use crate::message::Message;
use crate::meta::DocTypeDef;
use crate::seams::Seams;

/// Who is doing what, and when. One per API request, job run or script call; passed unchanged
/// to every nested operation, so events carry the original command id (ADR-056).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The site (tenant).
    pub site: SiteId,
    /// The user, consumer or platform component acting.
    pub actor: Actor,
    /// The command id: the API's `Idempotency-Key`, or one a job assigns.
    pub command_id: Option<CommandId>,
    /// Preferred language for messages, for example `ur-PK`.
    pub language: Option<String>,
    /// The business time of the invocation, fixed for its whole run (every hook sees the same
    /// `now`, so derived dates never straddle midnight).
    pub now: OffsetDateTime,
}

/// A job to run later, enqueued through the outbox so it runs only if the transaction commits
/// (WP-PLT-06).
#[derive(Clone, Debug, PartialEq)]
pub struct JobRequest {
    /// The job, qualified by app: `erp_selling:send_overdue_reminders`.
    pub job: String,
    /// Arguments, in the native API value encoding.
    pub args: Json,
    /// Deduplicates enqueues: a second job with the same key while the first is pending is
    /// dropped.
    pub idempotency_key: Option<String>,
    /// Earliest start time.
    pub not_before: Option<OffsetDateTime>,
}

/// Documents and queries, as controller code sees them.
///
/// Every write runs the target document's **full lifecycle** (its own four phases), inside the
/// caller's transaction in phase 3 and in a new transaction of its own in phase 4. Reads and
/// queries are filtered by the invocation's permissions. Writes that the caller's phase does not
/// allow fail with `platform.phase_violation` (see [`PhaseRules`](crate::PhaseRules)); so does
/// writing the document the hook runs for, which only `before_*` may change, through
/// [`HookContext::doc_mut`].
pub trait Repository: Send {
    /// A DocType's metadata, at the version the operation runs against.
    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>>;

    /// Loads a document with its child tables.
    fn load<'a>(&'a mut self, doctype: &'a str, id: RecordId)
    -> BoxFuture<'a, AppResult<Document>>;

    /// Runs a query (the storage query IR, ADR-053); results are permission-filtered.
    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>>;

    /// Creates a document through its create lifecycle; returns it as saved.
    fn create(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>>;

    /// Updates a document through its update (or update-posted) lifecycle.
    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>>;

    /// Posts a draft.
    fn post<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>>;

    /// Cancels a posted document.
    fn cancel<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>>;

    /// Deletes a draft (or a document of a DocType that is not posted).
    fn delete<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<()>>;
}

/// Site-scoped, lossy cache (ADR-20): any entry may disappear; always be able to reload.
pub trait AppCache: Send + Sync {
    /// A cached value.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Vec<u8>>>;
    /// Stores a value with an optional time to live.
    fn set<'a>(&'a self, key: &'a str, value: Vec<u8>, ttl: Option<Duration>) -> BoxFuture<'a, ()>;
    /// Removes a value.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, ()>;
}

/// What a lifecycle step gets. Hosts check every call against the phase's rules.
pub trait HookContext: Repository {
    /// Where the step runs.
    fn point(&self) -> &HookPoint;

    /// Who is acting, and when.
    fn invocation(&self) -> &Invocation;

    /// The document the step runs for. In `after_*` and `*_committed` it is the document as
    /// written; in `after_delete` and `delete_committed`, as it was before deletion.
    fn doc(&self) -> &Document;

    /// The document, to change it: only in `before_*` and in `field_changed` (where it changes
    /// the unsaved draft, never storage). Elsewhere `platform.phase_violation`.
    fn doc_mut(&mut self) -> AppResult<&mut Document>;

    /// Adds a message to the operation's result.
    fn message(&mut self, message: Message);

    /// Enqueues a job (phases 3 and 4). In phase 3 it runs only if the transaction commits.
    fn enqueue_job(&mut self, job: JobRequest) -> AppResult<()>;

    /// Publishes an app event (phases 3 and 4) to the outbox. `data` must match the topic's
    /// registered schema (ADR-056).
    fn publish(&mut self, topic: &Topic, schema_version: u32, data: Json) -> AppResult<()>;

    /// Sends a realtime notification to a user (phases 3 and 4), delivered after commit
    /// (ADR-25).
    fn notify_user(&mut self, user: RecordId, payload: Json) -> AppResult<()>;

    /// Succeeds only where calls to outside systems are allowed (phase 4). Facades for e-mail,
    /// webhooks and HTTP call it first; an in-transaction external call is rejected (ADR-51).
    fn external_allowed(&self) -> AppResult<()>;

    /// Service seams (ADR-08).
    fn seams(&self) -> &dyn Seams;

    /// Reports an effect the step caused, for the operation's receipt (ADR-080 6.4). Only an
    /// effect the step declared may be reported; internal effects are reported in phase 3,
    /// external ones when announced in phase 4. The default does nothing, so existing contexts
    /// compile and an operation whose steps report nothing has an empty receipt.
    fn report_effect(&mut self, receipt: EffectReceipt) {
        let _ = receipt;
    }

    /// The site's cache.
    fn cache(&self) -> &dyn AppCache;

    /// For a step installed as a **wrap** override: runs the wrapped step once, with this same
    /// context. Elsewhere it fails with `platform.phase_violation`.
    fn run_wrapped(&mut self) -> BoxFuture<'_, AppResult<()>>;
}

/// What a whitelisted method gets (ADR-16). A method runs in its own transaction with full
/// read and write access through the repository, including to its own document, whose changes
/// go through the update lifecycle like any other write.
pub trait MethodContext: Repository {
    /// Who is acting, and when.
    fn invocation(&self) -> &Invocation;
    /// The document the method was called on, as loaded.
    fn doc(&self) -> &Document;
    /// Adds a message to the result.
    fn message(&mut self, message: Message);
    /// Enqueues a job, run only if the method's transaction commits.
    fn enqueue_job(&mut self, job: JobRequest) -> AppResult<()>;
    /// Service seams.
    fn seams(&self) -> &dyn Seams;
}

/// A result with the operation's messages.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome<T> {
    /// The result.
    pub value: T,
    /// Messages collected from every step, in order.
    pub messages: Vec<Message>,
}

/// The document operations a host offers to the native API layer (WP-PLT-05's services) and to
/// tests. Each call is one transaction, with committed handlers run after it commits.
pub trait DocumentService: Send + Sync {
    /// A DocType's metadata.
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>>;

    /// Loads a document and runs its `load` steps.
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;

    /// Creates a document.
    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;

    /// Updates a draft, or the allowed fields of a posted document.
    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;

    /// Posts a draft.
    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;

    /// Cancels a posted document.
    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;

    /// Cancels a posted document and creates its amended draft in one transaction (ADR-42);
    /// returns the draft, whose `amended_from` links the original.
    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;

    /// Deletes a draft.
    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>>;

    /// Changes the human-readable name (the DocType's title field).
    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;

    /// Calls a whitelisted method; returns the document afterwards and the method's result.
    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, Json)>>>;

    /// Runs `field_changed:<field>` steps on an unsaved draft and returns the adjusted draft.
    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>>;
}

/// The name of an amended draft's link to the document it corrects (ADR-42).
pub const AMENDED_FROM: &str = "amended_from";

/// Checks a job name: `<app>:<job>`, both storage-style names.
pub fn valid_job_name(job: &str) -> bool {
    job.split_once(':')
        .is_some_and(|(a, j)| Name::new(a).is_ok() && Name::new(j).is_ok())
}
