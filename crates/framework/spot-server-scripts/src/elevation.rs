//! Script elevation (WP-EXT-06.T8; plan/FIXES.md G2): a script runs as the user who triggered it,
//! and only an explicit, audited, per-script flag lets it act as the platform instead.
//!
//! # The rules
//!
//! - **Default deny.** A script is elevated only when its record says `elevated` *and* the host
//!   wired an [`ElevationWiring`]. A flagged script on a host without the wiring runs as its user.
//! - **One door.** [`Elevation`] is a marker nobody outside this module can build: only
//!   [`Elevation::granted`] makes one, and only from a record that says it is elevated.
//!   [`ElevatedAccess`], the one access that acts as the platform, needs one to be built.
//! - **Audit first, fail closed.** [`ElevationSession::begin`] asks the audit sink to *reserve*
//!   the run before the engine starts; a sink that cannot is a refused run and no elevated write
//!   happens. [`ElevationSession::finish`] records the run (even a failed or killed one); a sink
//!   that cannot record turns the run into a failure.
//! - **The triggering user stays visible.** The script's `ctx.user` is still the triggering
//!   user, and every audit entry names them.
//! - **Not for scheduled scripts.** A scheduled script has no such field and a document carrying
//!   one is refused.
//! - **Nothing of the script in the audit**: the entry has the script's name, kind, user,
//!   DocType, operation, outcome code and two counts, never source, arguments or values.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value as Json;
use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{Name, OffsetDateTime, Query, RecordId, Rows, Value, Version};
use spot_app_api::{
    Actor, AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass,
    Invocation,
};

use crate::access::{ScriptAccessFactory, ScriptDocumentAccess};
use crate::codes;

/// The named app permission that lets a user elevate a script. The named-permission grammar
/// (`spot_permissions::AppPermissionName`) allows lowercase letters, digits and `_` only, so the
/// name has no dot; a dotted name would be refused when the `scripting` app installs and the site
/// would not start (WP-EXT-06.T10).
pub const PERMISSION_ELEVATE: &str = "scripts_elevate";
/// The storage field holding whether the script is elevated.
pub const FIELD_ELEVATED: &str = "elevated";
/// The storage field holding why.
pub const FIELD_ELEVATION_REASON: &str = "elevation_reason";
/// The storage field holding who set the flag (read-only).
pub const FIELD_ELEVATED_BY: &str = "elevated_by";
/// The storage field holding when the flag was set (read-only).
pub const FIELD_ELEVATED_AT: &str = "elevated_at";
/// The component name an elevated run acts under.
pub const PLATFORM_COMPONENT: &str = "script_elevated";
/// The shortest reason, in characters.
pub const REASON_MIN: usize = 20;
/// The longest reason, in characters.
pub const REASON_MAX: usize = 500;
/// What a save that reset the flag tells the user.
pub const RESET_MESSAGE: &str = "The source changed, so the script is no longer elevated; ask a holder of the elevate permission to elevate it again.";

/// Why a script is elevated, as its record says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElevationFlag {
    /// The stated reason (20 to 500 characters).
    pub reason: String,
    /// Who set the flag, as the stamp recorded it.
    pub by: Option<String>,
    /// When, as the stamp recorded it.
    pub at: Option<String>,
}

fn text<'a>(doc: &'a Document, field: &str) -> Option<&'a str> {
    match doc.get(field) {
        Value::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

fn is_on(doc: &Document) -> bool {
    matches!(doc.get(FIELD_ELEVATED), Value::Bool(true))
}

/// Reads the elevation of a record: `None` when it is not elevated.
///
/// # Errors
///
/// `serverscript.elevation_reason_required` (a field error on the reason) when the record is
/// elevated and its reason is not 20 to 500 characters.
pub fn read_flag(doc: &Document) -> AppResult<Option<ElevationFlag>> {
    if !is_on(doc) {
        return Ok(None);
    }
    let reason = text(doc, FIELD_ELEVATION_REASON)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let length = reason.chars().count();
    if !(REASON_MIN..=REASON_MAX).contains(&length) {
        return Err(reason_required());
    }
    Ok(Some(ElevationFlag {
        reason,
        by: text(doc, FIELD_ELEVATED_BY).map(str::to_owned),
        at: text(doc, FIELD_ELEVATED_AT).map(str::to_owned),
    }))
}

fn reason_required() -> AppError {
    AppError::field(
        FieldPath::field(FIELD_ELEVATION_REASON),
        codes::ELEVATION_REASON_REQUIRED,
        format!("An elevated script needs a reason of {REASON_MIN} to {REASON_MAX} characters."),
    )
}

/// A record that may be elevated.
pub trait Elevatable {
    /// The record's flag, when it is elevated.
    fn elevation(&self) -> Option<&ElevationFlag>;
}

/// The proof that a record is elevated. It cannot be built outside this module.
#[derive(Debug)]
pub struct Elevation(());

impl Elevation {
    /// The proof for `record`, only when it says it is elevated.
    pub(crate) fn granted(record: &impl Elevatable) -> Option<Elevation> {
        record.elevation().map(|_| Elevation(()))
    }
}

/// A document-event script's elevation, kept beside its [`crate::ServerScript`] by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptElevation {
    /// The script's name.
    pub name: String,
    /// Its flag.
    pub flag: Option<ElevationFlag>,
}

impl Elevatable for ScriptElevation {
    fn elevation(&self) -> Option<&ElevationFlag> {
        self.flag.as_ref()
    }
}

// ------------------------------------------------------------------------------------------
// The save rules

/// What a save does about the flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavePlan {
    /// Whether the saved record is elevated.
    pub elevated: bool,
    /// The stamp to write (`by`, `at`), when the flag was newly set or its basis changed.
    pub stamp: Option<(String, String)>,
    /// Whether the flag was reset because the source changed under a user who may not elevate.
    pub reset: bool,
}

/// Who is saving and what they hold.
#[derive(Clone, Debug)]
pub struct Saver {
    /// The saving user, as the audit names them.
    pub user: String,
    /// Whether they hold [`PERMISSION_ELEVATE`].
    pub may_elevate: bool,
    /// The save's time, as the stamp renders it.
    pub at: String,
}

/// Decides what a save of `doc` does about elevation. `previous` is the stored version, `None` for
/// a create. `scheduled` is true for a scheduled script, which can never be elevated.
///
/// # Errors
///
/// - `serverscript.elevation_forbidden`: the saver may not elevate and the save turns the flag
///   on or edits an elevated script without changing its source;
/// - `serverscript.elevation_reason_required`: an elevated record without a 20 to 500 character
///   reason;
/// - `serverscript.elevation_not_allowed`: a scheduled script carrying the flag.
pub fn check_save(
    doc: &Document,
    previous: Option<&Document>,
    saver: &Saver,
    scheduled: bool,
) -> AppResult<SavePlan> {
    if scheduled {
        if is_on(doc) {
            return Err(AppError::field(
                FieldPath::field(FIELD_ELEVATED),
                codes::ELEVATION_NOT_ALLOWED,
                "A scheduled script runs as its run_as user and cannot be elevated.",
            ));
        }
        return Ok(SavePlan {
            elevated: false,
            stamp: None,
            reset: false,
        });
    }
    let was_on = previous.is_some_and(is_on);
    if !is_on(doc) {
        // Turning it off needs no permission.
        return Ok(SavePlan {
            elevated: false,
            stamp: None,
            reset: false,
        });
    }
    let source_changed = previous.is_none_or(|p| {
        text(p, crate::record::FIELD_SCRIPT) != text(doc, crate::record::FIELD_SCRIPT)
    });
    let reason_changed = previous
        .is_none_or(|p| text(p, FIELD_ELEVATION_REASON) != text(doc, FIELD_ELEVATION_REASON));
    if !saver.may_elevate {
        if was_on && source_changed {
            // A changed script is a new script: it loses the flag.
            return Ok(SavePlan {
                elevated: false,
                stamp: None,
                reset: true,
            });
        }
        let untouched =
            was_on && !reason_changed && previous.is_some_and(|p| same_but_runtime(p, doc));
        if !untouched {
            return Err(AppError::field(
                FieldPath::field(FIELD_ELEVATED),
                codes::ELEVATION_FORBIDDEN,
                "Only a holder of the elevate permission may elevate a script or edit an elevated one.",
            ));
        }
        return Ok(SavePlan {
            elevated: true,
            stamp: None,
            reset: false,
        });
    }
    read_flag(doc)?;
    let stamp = (!was_on || reason_changed || source_changed)
        .then(|| (saver.user.clone(), saver.at.clone()));
    Ok(SavePlan {
        elevated: true,
        stamp,
        reset: false,
    })
}

/// Whether `a` and `b` have the same user-editable fields (the read-only run-time summary fields
/// and the stamp are ignored).
fn same_but_runtime(a: &Document, b: &Document) -> bool {
    let ignored = [
        crate::record::FIELD_COMPILED_HASH,
        crate::record::FIELD_LAST_ERROR,
        crate::record::FIELD_LAST_RUN_COUNT,
        FIELD_ELEVATED_BY,
        FIELD_ELEVATED_AT,
    ];
    let fields = |d: &Document| {
        d.fields()
            .iter()
            .filter(|(name, _)| !ignored.contains(&name.as_str()))
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    fields(a) == fields(b)
}

/// Writes a plan's outcome onto the document being saved.
///
/// # Errors
///
/// A field name that is not a storage name (never, for the fixed names here).
pub fn apply_plan(doc: &mut Document, plan: &SavePlan) -> AppResult<()> {
    let name = |field: &str| {
        Name::new(field).map_err(|_| {
            AppError::new(
                ErrorClass::Internal,
                codes::RECORD_INVALID,
                "a fixed field name",
            )
        })
    };
    doc.set(name(FIELD_ELEVATED)?, Value::Bool(plan.elevated));
    if !plan.elevated {
        doc.set(name(FIELD_ELEVATION_REASON)?, Value::Null);
        doc.set(name(FIELD_ELEVATED_BY)?, Value::Null);
        doc.set(name(FIELD_ELEVATED_AT)?, Value::Null);
    }
    if let Some((by, at)) = &plan.stamp {
        doc.set(name(FIELD_ELEVATED_BY)?, Value::Text(by.clone()));
        doc.set(name(FIELD_ELEVATED_AT)?, Value::Text(at.clone()));
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// The run

/// Which kind of script an elevated run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElevatedKind {
    /// A document-event script.
    DocumentEvent,
    /// An API script.
    Api,
}

/// One elevated run, as the audit sees it. No script text, arguments or values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElevatedRun {
    /// The script's name.
    pub script: String,
    /// Its kind.
    pub kind: ElevatedKind,
    /// The user whose action triggered it, as the record id text.
    pub triggering_user: String,
    /// The DocType it ran for (document events), when there is one.
    pub doctype: Option<String>,
    /// What it ran for (`validate`, `on_submit`, `call`, ...).
    pub operation: String,
    /// When it started.
    pub started: OffsetDateTime,
    /// `reserved` before it ran, then `ok` or the error's stable code.
    pub outcome: String,
    /// How many host calls it made.
    pub host_calls: u32,
    /// How many of them wrote (insert, update or method call).
    pub written_documents: u32,
}

/// Where elevated runs are recorded: a host implements it on the site's audit chain.
pub trait ElevationAudit: Send + Sync {
    /// Reserves the run before the engine starts. An error refuses the run: no audit, no
    /// elevated write.
    fn reserve<'a>(&'a self, run: &'a ElevatedRun) -> BoxFuture<'a, AppResult<()>>;
    /// Records the finished run, whatever its outcome.
    fn record<'a>(&'a self, run: &'a ElevatedRun) -> BoxFuture<'a, AppResult<()>>;
}

/// An in-memory [`ElevationAudit`] for tests, with switches that make either step fail.
#[derive(Default)]
pub struct FakeElevationAudit {
    reserved: Mutex<Vec<ElevatedRun>>,
    recorded: Mutex<Vec<ElevatedRun>>,
    fail_reserve: std::sync::atomic::AtomicBool,
    fail_record: std::sync::atomic::AtomicBool,
}

impl FakeElevationAudit {
    /// An empty, working sink.
    #[must_use]
    pub fn new() -> FakeElevationAudit {
        FakeElevationAudit::default()
    }

    /// Makes `reserve` fail (or work again).
    pub fn set_fail_reserve(&self, fail: bool) {
        self.fail_reserve.store(fail, Ordering::SeqCst);
    }

    /// Makes `record` fail (or work again).
    pub fn set_fail_record(&self, fail: bool) {
        self.fail_record.store(fail, Ordering::SeqCst);
    }

    /// The runs reserved, in order.
    #[must_use]
    pub fn reserved(&self) -> Vec<ElevatedRun> {
        lock(&self.reserved).clone()
    }

    /// The runs recorded, in order.
    #[must_use]
    pub fn recorded(&self) -> Vec<ElevatedRun> {
        lock(&self.recorded).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn audit_failed() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        codes::ELEVATION_AUDIT_FAILED,
        "The elevated run could not be audited, so it was not allowed.",
    )
}

impl ElevationAudit for FakeElevationAudit {
    fn reserve<'a>(&'a self, run: &'a ElevatedRun) -> BoxFuture<'a, AppResult<()>> {
        let failing = self.fail_reserve.load(Ordering::SeqCst);
        if !failing {
            lock(&self.reserved).push(run.clone());
        }
        Box::pin(async move { if failing { Err(audit_failed()) } else { Ok(()) } })
    }

    fn record<'a>(&'a self, run: &'a ElevatedRun) -> BoxFuture<'a, AppResult<()>> {
        let failing = self.fail_record.load(Ordering::SeqCst);
        if !failing {
            lock(&self.recorded).push(run.clone());
        }
        Box::pin(async move { if failing { Err(audit_failed()) } else { Ok(()) } })
    }
}

/// What a host supplies to elevate: how to open an access that acts as the platform, and where
/// to audit.
#[derive(Clone)]
pub struct ElevationWiring {
    /// Opens the access for the platform invocation an elevated run acts under.
    pub factory: Arc<dyn ScriptAccessFactory>,
    /// The audit sink.
    pub audit: Arc<dyn ElevationAudit>,
}

/// The platform invocation an elevated run's host calls act under: the same site, time, command
/// id and language, with the platform as the actor.
///
/// # Errors
///
/// Never in practice (the component name is fixed and valid).
pub fn platform_invocation(base: &Invocation) -> AppResult<Invocation> {
    let component = Name::new(PLATFORM_COMPONENT).map_err(|_| {
        AppError::new(
            ErrorClass::Internal,
            codes::RECORD_INVALID,
            "a fixed component name",
        )
    })?;
    Ok(Invocation {
        actor: Actor::System { component },
        ..base.clone()
    })
}

/// How many host calls an elevated run made and how many wrote.
#[derive(Default)]
pub struct ElevationCounts {
    host_calls: AtomicU32,
    written: AtomicU32,
}

/// The one access that acts as the platform. It wraps the access the factory opened for the
/// platform invocation and counts what the script does with it.
pub struct ElevatedAccess {
    inner: Box<dyn ScriptDocumentAccess>,
    counts: Arc<ElevationCounts>,
}

impl ElevatedAccess {
    /// Wraps `inner`. Needs the proof that the record is elevated.
    pub(crate) fn new(
        _proof: &Elevation,
        inner: Box<dyn ScriptDocumentAccess>,
        counts: Arc<ElevationCounts>,
    ) -> ElevatedAccess {
        ElevatedAccess { inner, counts }
    }

    fn read(&self) {
        self.counts.host_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn write(&self) {
        self.counts.host_calls.fetch_add(1, Ordering::SeqCst);
        self.counts.written.fetch_add(1, Ordering::SeqCst);
    }
}

impl ScriptDocumentAccess for ElevatedAccess {
    fn invocation(&self) -> &Invocation {
        self.inner.invocation()
    }

    fn can_call(&self) -> bool {
        self.inner.can_call()
    }

    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.read();
        self.inner.meta(doctype)
    }

    fn get<'a>(&'a mut self, doctype: &'a str, id: RecordId) -> BoxFuture<'a, AppResult<Document>> {
        self.read();
        self.inner.get(doctype, id)
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        self.read();
        self.inner.query(query)
    }

    fn insert(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        self.write();
        self.inner.insert(doc)
    }

    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.write();
        self.inner.update(doctype, id, patch, expected)
    }

    fn call<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.write();
        self.inner.call(doctype, id, method, args)
    }
}

/// One elevated run in progress: reserved with the audit, holding the platform access.
pub struct ElevationSession {
    wiring: ElevationWiring,
    run: ElevatedRun,
    counts: Arc<ElevationCounts>,
    access: ElevatedAccess,
}

/// What identifies the run to the audit.
#[derive(Clone, Debug)]
pub struct RunIdentity {
    /// The script's name.
    pub script: String,
    /// Its kind.
    pub kind: ElevatedKind,
    /// The DocType it runs for, when it has one.
    pub doctype: Option<String>,
    /// What it runs for.
    pub operation: String,
    /// The triggering user, as the record id text.
    pub triggering_user: String,
}

impl ElevationSession {
    /// Reserves the run with the audit and opens the platform access.
    ///
    /// # Errors
    ///
    /// `serverscript.elevation_audit_failed` when the sink cannot reserve: the run does not
    /// start and nothing is written.
    pub async fn begin(
        wiring: &ElevationWiring,
        proof: Elevation,
        identity: RunIdentity,
        base: &Invocation,
    ) -> AppResult<ElevationSession> {
        let run = ElevatedRun {
            script: identity.script,
            kind: identity.kind,
            triggering_user: identity.triggering_user,
            doctype: identity.doctype,
            operation: identity.operation,
            started: base.now,
            outcome: "reserved".to_owned(),
            host_calls: 0,
            written_documents: 0,
        };
        wiring.audit.reserve(&run).await?;
        let counts = Arc::new(ElevationCounts::default());
        let inner = wiring.factory.open(platform_invocation(base)?);
        let access = ElevatedAccess::new(&proof, inner, Arc::clone(&counts));
        Ok(ElevationSession {
            wiring: wiring.clone(),
            run,
            counts,
            access,
        })
    }

    /// The access the script's host calls go through.
    pub fn access(&mut self) -> &mut ElevatedAccess {
        &mut self.access
    }

    /// Records the run with its `outcome` (`ok` or the error's code), even when it failed.
    ///
    /// # Errors
    ///
    /// `serverscript.elevation_audit_failed` when the sink cannot record: the run fails.
    pub async fn finish(mut self, outcome: &str) -> AppResult<()> {
        self.run.outcome = outcome.to_owned();
        self.run.host_calls = self.counts.host_calls.load(Ordering::SeqCst);
        self.run.written_documents = self.counts.written.load(Ordering::SeqCst);
        self.wiring.audit.record(&self.run).await
    }
}
