//! The AI audit trail, hosted (WP-AI-11.T8): agent actions chained, document timeline entries
//! read back masked, and the review digest produced on a schedule.
//!
//! # Step 0: what existed and what this module adds
//!
//! | piece | before this task | here |
//! |---|---|---|
//! | (a) AI action entries appended to the chain | `spot-mcp::audit::ChainSink` could chain one tool call, but nothing in `spot serve` hosted a chain for AI actions, and nothing hosted the document-audit lane appender either (`spot-doc-audit`'s `LaneAppender` has no host in `spot-cli`) | [`AiAudit::append`] over a chain of its own, scope [`scope_of`] (`ai:<site>`) |
//! | (b) document timeline entries from the chain | `spot-doc-audit::DocAudit::history` reads document changes only; no AI entry reaches a timeline | [`AiAudit::timeline`] |
//! | (c) the review digest | missing | [`AiAudit::build_digest`] and the service's schedule |
//!
//! The document lanes (`spot-doc-audit`) are **not** hosted by this module: hosting the lane
//! appender belongs with WP-SEC-06.T8 and needs the async appender state of WP-SEC-06.T6. The AI
//! trail is a chain of its own on the same store (`spot-audit-store`), so it verifies with the
//! same verifier and the same database guard, and a lane failure cannot stall it.
//!
//! # What an entry holds
//!
//! One entry per tool call, approval, rejection or undo, kind `ai.tool_call`, `ai.approval`,
//! `ai.rejection` or `ai.undo`. The actor is `agent:<id>` with `on_behalf_of = user:<id>` (the
//! acting user), or, for an approval or a rejection, the reviewing `user:<id>`. The payload holds
//! the tool, the outcome, the agent and acting user ids, the event id, optionally the id of the
//! action an undo or decision refers to, and the **masked arguments** ([`ArgumentMask`]): a
//! value is kept only when the mask lets it through, a long value is a length and a digest, and
//! a hidden value is a marker. No prompt, no model output and no tool result is ever stored.
//!
//! # Exactly once
//!
//! Every action carries an event id. The service holds the ids of the most recent
//! [`Settings::dedupe_window`] entries (read from the chain at start) and an event id seen again
//! answers [`Receipt::Duplicate`] without a second entry. A document change made by the action
//! is written by the engine in its own transaction with its own audit event (WP-SEC-06.T1); this
//! entry is the AI-side record and is correlated by the shared `command_id` argument.
//!
//! # Failure behaviour
//!
//! The service is **optional**: a failing check or a stopped loop degrades the site and says so.
//! [`AiAuditPort::append`] returns an error when the entry cannot be made durable, and the caller
//! (the agent runtime) must then refuse the action: nothing the AI does goes unrecorded. The
//! digest loop never stops the site.
//!
//! # The timeline
//!
//! [`AiAudit::timeline`] first asks the [`ReadAccess`] port what the viewer may see of the
//! document: nothing (the answer is an empty list, the same as for a missing document), the
//! entries with their argument values hidden, or the entries as recorded. An error from the
//! access check is a refusal, never an allow. Without a [`ReadAccess`] provider installed by another
//! module the host uses the engine-backed one ([`read_access::EngineReadAccess`], WP-AI-11.T9),
//! which loads the document through the live engine as the viewer and asks the field guard:
//! [`Access::None`] for a document the viewer may not read (or that does not exist, or is
//! another site's), [`Access::Masked`] when a protected field is not shown to them as its real
//! value, [`Access::Full`] otherwise. [`DenyAll`] remains for a site whose name is not a valid
//! site id.
//!
//! # The review digest
//!
//! Every [`Settings::digest_interval_seconds`] the loop looks at the last complete period of
//! [`Settings::digest_period_seconds`], and when no digest of that period exists it builds one
//! from the chain entries only (counts of calls, approvals, rejections, undone actions and
//! refused actions, and an anomaly for every agent whose volume is more than
//! [`Settings::spike_factor`] times the previous period's, with at least
//! [`Settings::spike_min_actions`] actions) and appends it as an `ai.digest` entry. The latest
//! digest is the port's [`AiAuditPort::latest_digest`]. Delivering it to a reviewer's notice
//! queue is an open question of the task report (no notice port exists to take it).
//!
//! # Tenancy and secrets
//!
//! One database is one site; the chain scope names the site and no caller chooses it. Nothing
//! logged or returned by an error carries an argument value: only stable `ai_audit_host.*`
//! codes.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use spot_audit_chain::{
    AuditChain, AuditError as ChainError, AuditRecord, AuditStore, ChainEntry, CheckpointPolicy,
    CheckpointSigner, KeyRing, PayloadValue, Subject, Verifier,
};
use spot_audit_store::PgAuditStore;
use spot_core::Clock;
use spot_doc_audit::sha256_hex;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "ai_audit/read_access.rs"]
pub mod read_access;
#[path = "ai_audit/timeline_port.rs"]
pub mod timeline_port;

/// Start order: after the framework's own services and the capture invariant.
pub const ORDER: u32 = 1060;

/// The fixed prefix of the AI chain's scope; the site name follows.
pub const SCOPE_PREFIX: &str = "ai:";

/// The root of every kind this module writes (`ai.tool_call`, ...). Built from parts at the use
/// site so the kind is a value, not a catalogue code.
const KIND_ROOT: &str = "ai";

/// The append could not be made durable (the caller must refuse the action).
pub const APPEND_FAILED: &str = "ai_audit_host.append_failed";
/// The action is malformed (an empty id, an actor that is not an id, a self-approval).
pub const RECORD_INVALID: &str = "ai_audit_host.record_invalid";
/// The viewer's access could not be decided, so the timeline was refused.
pub const TIMELINE_REFUSED: &str = "ai_audit_host.timeline_refused";
/// The chain could not be read.
pub const CHAIN_UNAVAILABLE: &str = "ai_audit_host.chain_unavailable";
/// The review digest could not be built or recorded.
pub const DIGEST_FAILED: &str = "ai_audit_host.digest_failed";

const MAX_ID_BYTES: usize = 128;
const MAX_ARGS: usize = 32;
const MAX_ARGS_JSON_BYTES: usize = 6 * 1024;
const LONG_VALUE_BYTES: usize = 128;
const MAX_TIMELINE: usize = 200;
const SCAN_PAGE: u64 = 500;
const MAX_SCAN_ENTRIES: u64 = 50_000;
const HIDDEN: &str = "***";

/// A boxed future, as the port's methods return.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A stable error: only a code, never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiAuditError {
    /// One of the `ai_audit_host.*` codes of this module.
    pub code: &'static str,
}

impl AiAuditError {
    fn new(code: &'static str) -> Self {
        Self { code }
    }
}

impl std::fmt::Display for AiAuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for AiAuditError {}

/// `[services.ai_audit]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service runs. Off by default.
    pub enabled: bool,
    /// Seconds between looks for a period that needs its digest; never below 60.
    pub digest_interval_seconds: u64,
    /// The length of one digest period in seconds; at least 3600.
    pub digest_period_seconds: u64,
    /// An agent's volume is an anomaly above this multiple of the previous period's; at least 2.
    pub spike_factor: u64,
    /// The fewest actions of one agent that can be an anomaly; at least 1.
    pub spike_min_actions: u64,
    /// How many recent entries' event ids are remembered for exactly-once; 100 to 1,000,000.
    pub dedupe_window: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            digest_interval_seconds: 3600,
            digest_period_seconds: 86_400,
            spike_factor: 5,
            spike_min_actions: 20,
            dedupe_window: 10_000,
        }
    }
}

impl Settings {
    /// Refuses a value outside its documented range.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the key.
    pub fn validate(&self) -> Result<()> {
        let bad = |key: &str| Err(Error(format!("services.ai_audit.{key} is out of range")));
        if self.digest_interval_seconds < 60 {
            return bad("digest_interval_seconds");
        }
        if self.digest_period_seconds < 3600 || i64::try_from(self.digest_period_seconds).is_err() {
            return bad("digest_period_seconds");
        }
        if self.spike_factor < 2 {
            return bad("spike_factor");
        }
        if self.spike_min_actions < 1 {
            return bad("spike_min_actions");
        }
        if !(100..=1_000_000).contains(&self.dedupe_window) {
            return bad("dedupe_window");
        }
        Ok(())
    }
}

/// What kind of AI act an entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// An agent called a tool.
    ToolCall,
    /// A person approved a proposed action.
    Approval,
    /// A person rejected a proposed action.
    Rejection,
    /// An action was undone.
    Undo,
}

impl ActionKind {
    fn segment(self) -> &'static str {
        match self {
            Self::ToolCall => "tool_call",
            Self::Approval => "approval",
            Self::Rejection => "rejection",
            Self::Undo => "undo",
        }
    }

    fn from_segment(segment: &str) -> Option<Self> {
        match segment {
            "tool_call" => Some(Self::ToolCall),
            "approval" => Some(Self::Approval),
            "rejection" => Some(Self::Rejection),
            "undo" => Some(Self::Undo),
            _ => None,
        }
    }
}

/// How the action ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It ran (or was approved).
    Ok,
    /// It was refused before it ran.
    Refused,
    /// It ran and failed.
    Failed,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

/// One AI act, as the agent runtime reports it. Arguments are unmasked here and masked by the
/// service's [`ArgumentMask`] before anything is written.
#[derive(Clone, Debug)]
pub struct AiAction {
    /// What happened.
    pub kind: ActionKind,
    /// A unique id of this act: a retry carries the same one.
    pub event_id: String,
    /// The agent's id.
    pub agent: String,
    /// The user the agent acts for; for an approval or a rejection, the reviewing user.
    pub acting_user: String,
    /// The tool called (or proposed).
    pub tool: String,
    /// The tool's arguments, name and value.
    pub arguments: Vec<(String, String)>,
    /// How it ended.
    pub outcome: Outcome,
    /// The document it concerns, as `(doctype, id)`.
    pub subject: Option<(String, String)>,
    /// For an approval, a rejection or an undo, the event id of the action it refers to.
    pub refers_to: Option<String>,
    /// Unix microseconds, from the caller's clock.
    pub at_micros: i64,
}

/// What an argument value becomes when written.
pub trait ArgumentMask: Send + Sync {
    /// `Some(text)` keeps a value (long text is still reduced to a digest by the service);
    /// `None` hides it.
    fn mask(&self, tool: &str, name: &str, value: &str) -> Option<String>;
}

/// The default mask: hides any argument whose name suggests a secret, keeps the rest.
pub struct SecretNameMask;

impl ArgumentMask for SecretNameMask {
    fn mask(&self, _tool: &str, name: &str, value: &str) -> Option<String> {
        const WORDS: [&str; 8] = [
            "password",
            "secret",
            "token",
            "key",
            "credential",
            "auth",
            "iban",
            "card",
        ];
        let lower = name.to_ascii_lowercase();
        if WORDS.iter().any(|w| lower.contains(w)) {
            None
        } else {
            Some(value.to_owned())
        }
    }
}

/// How much of a document a viewer may see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Nothing: the document answers like a missing one.
    None,
    /// The document with its protected values hidden.
    Masked,
    /// The document as recorded.
    Full,
}

/// What the viewer may see of a document. An `Err` is a refusal.
pub trait ReadAccess: Send + Sync {
    /// The viewer's access to `doctype`/`id`.
    fn access<'a>(
        &'a self,
        viewer: &'a str,
        doctype: &'a str,
        id: &'a str,
    ) -> Fut<'a, std::result::Result<Access, String>>;
}

/// The access that refuses everything: the default until the engine-backed provider exists.
pub struct DenyAll;

impl ReadAccess for DenyAll {
    fn access<'a>(
        &'a self,
        _viewer: &'a str,
        _doctype: &'a str,
        _id: &'a str,
    ) -> Fut<'a, std::result::Result<Access, String>> {
        Box::pin(async { Ok(Access::None) })
    }
}

/// What an append did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Receipt {
    /// Chained at this sequence number.
    Appended {
        /// The entry's sequence number.
        seq: u64,
    },
    /// The event id was already chained; nothing was written.
    Duplicate,
}

/// One timeline entry as a viewer sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineEntry {
    /// The chain sequence number.
    pub seq: u64,
    /// Unix microseconds.
    pub at_micros: i64,
    /// `tool_call`, `approval`, `rejection` or `undo`.
    pub kind: String,
    /// The agent's id.
    pub agent: String,
    /// The acting (or reviewing) user.
    pub acting_user: String,
    /// The tool.
    pub tool: String,
    /// `ok`, `refused` or `failed`.
    pub outcome: String,
    /// The masked arguments; a hidden value shows as `***`.
    pub arguments: Vec<(String, String)>,
}

/// An agent whose volume jumped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anomaly {
    /// The agent id.
    pub agent: String,
    /// Its actions this period.
    pub count: u64,
    /// Its actions in the period before.
    pub previous: u64,
}

/// The review digest of one period, built from chain entries only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewDigest {
    /// The period number: `start / period_seconds`.
    pub period: u64,
    /// Period start, Unix seconds.
    pub start: i64,
    /// Period end (exclusive), Unix seconds.
    pub end: i64,
    /// Tool calls.
    pub tool_calls: u64,
    /// Approvals.
    pub approvals: u64,
    /// Rejections.
    pub rejections: u64,
    /// Undone actions.
    pub undone: u64,
    /// Actions that were refused or failed.
    pub refused_or_failed: u64,
    /// Agents with a volume spike.
    pub anomalies: Vec<Anomaly>,
}

/// The AI audit trail of one site, as the agent runtime and the timeline route see it.
pub trait AiAuditPort: Send + Sync {
    /// Chains one action. An error means the entry is not durable and the action must be refused.
    fn append(&self, action: AiAction) -> Fut<'_, std::result::Result<Receipt, AiAuditError>>;
    /// The AI entries of one document that `viewer` may see, newest first.
    fn timeline<'a>(
        &'a self,
        viewer: &'a str,
        doctype: &'a str,
        id: &'a str,
        limit: usize,
    ) -> Fut<'a, std::result::Result<Vec<TimelineEntry>, AiAuditError>>;
    /// The newest review digest this node knows of.
    fn latest_digest(&self) -> Option<ReviewDigest>;
}

/// A signer that never signs: the chain's automatic checkpoints are off.
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }
    fn sign(&self, _message: &[u8]) -> std::result::Result<Vec<u8>, ChainError> {
        Err(ChainError::Signer("no checkpoint key is configured".into()))
    }
}

/// The chain scope of `site`'s AI trail.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("{SCOPE_PREFIX}{site}")
}

fn kind_of(segment: &str) -> String {
    format!("{KIND_ROOT}.{segment}")
}

fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

fn text(record: &AuditRecord, key: &str) -> Option<String> {
    match record.payload.get(key) {
        Some(PayloadValue::Text(t)) => Some(t.clone()),
        _ => None,
    }
}

fn int(record: &AuditRecord, key: &str) -> u64 {
    match record.payload.get(key) {
        Some(PayloadValue::Int(i)) => u64::try_from(*i).unwrap_or(0),
        _ => 0,
    }
}

fn to_int(value: u64) -> PayloadValue {
    PayloadValue::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

/// The masked arguments of `action` as the entry's JSON text.
fn arguments_json(action: &AiAction, mask: &dyn ArgumentMask) -> String {
    let mut object = serde_json::Map::new();
    for (name, value) in action.arguments.iter().take(MAX_ARGS) {
        let shown = match mask.mask(&action.tool, name, value) {
            None => serde_json::json!({ "m": true }),
            Some(kept) if kept.len() > LONG_VALUE_BYTES => {
                serde_json::json!({ "len": kept.len(), "digest": sha256_hex(kept.as_bytes()) })
            }
            Some(kept) => serde_json::json!({ "v": kept }),
        };
        object.insert(name.clone(), shown);
    }
    let json = serde_json::Value::Object(object).to_string();
    if json.len() > MAX_ARGS_JSON_BYTES {
        // Too many or too long names: keep only the digest of what was masked.
        return serde_json::json!({ "_digest": { "len": json.len(), "digest": sha256_hex(json.as_bytes()) } })
            .to_string();
    }
    json
}

/// The chain record of `action`.
///
/// # Errors
///
/// [`RECORD_INVALID`] for an empty or control-character id, an approval or a rejection by a
/// non-user, or a record the chain does not accept.
pub fn record_of(
    action: &AiAction,
    mask: &dyn ArgumentMask,
) -> std::result::Result<AuditRecord, AiAuditError> {
    let invalid = || AiAuditError::new(RECORD_INVALID);
    if !(id_ok(&action.event_id)
        && id_ok(&action.agent)
        && id_ok(&action.acting_user)
        && id_ok(&action.tool))
    {
        return Err(invalid());
    }
    let decision = matches!(action.kind, ActionKind::Approval | ActionKind::Rejection);
    // No self-approval: the reviewer is a person, and an agent id can never be one.
    if decision && action.acting_user == action.agent {
        return Err(invalid());
    }
    let (actor, on_behalf_of) = if decision {
        (format!("user:{}", action.acting_user), None)
    } else {
        (
            format!("agent:{}", action.agent),
            Some(format!("user:{}", action.acting_user)),
        )
    };
    let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
    payload.insert(
        "event_id".into(),
        PayloadValue::Text(action.event_id.clone()),
    );
    payload.insert("agent".into(), PayloadValue::Text(action.agent.clone()));
    payload.insert(
        "user".into(),
        PayloadValue::Text(action.acting_user.clone()),
    );
    payload.insert("tool".into(), PayloadValue::Text(action.tool.clone()));
    payload.insert(
        "outcome".into(),
        PayloadValue::Text(action.outcome.as_str().to_owned()),
    );
    payload.insert(
        "args".into(),
        PayloadValue::Text(arguments_json(action, mask)),
    );
    if let Some(target) = &action.refers_to {
        if !id_ok(target) {
            return Err(invalid());
        }
        payload.insert("refers_to".into(), PayloadValue::Text(target.clone()));
    }
    let record = AuditRecord {
        kind: kind_of(action.kind.segment()),
        actor,
        on_behalf_of,
        subject: action.subject.as_ref().map(|(doctype, id)| Subject {
            doctype: doctype.clone(),
            id: id.clone(),
        }),
        at: action.at_micros,
        payload,
        masked: true,
    };
    record.validate().map_err(|_| invalid())?;
    Ok(record)
}

/// The ids of the entries already chained, in append order, bounded.
struct Dedupe {
    seen: HashSet<String>,
    order: VecDeque<String>,
    loaded: bool,
}

impl Dedupe {
    fn remember(&mut self, id: String, cap: usize) {
        if self.seen.insert(id.clone()) {
            self.order.push_back(id);
        }
        while self.order.len() > cap {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
    }
}

/// The AI audit trail over one chain store.
pub struct AiAudit<S: AuditStore> {
    chain: AuditChain<S>,
    scope: String,
    mask: Arc<dyn ArgumentMask>,
    access: Arc<dyn ReadAccess>,
    settings: Settings,
    dedupe: tokio::sync::Mutex<Dedupe>,
    digest: Mutex<Option<ReviewDigest>>,
}

impl<S: AuditStore> AiAudit<S> {
    /// The AI trail of `site` over `store`.
    pub fn new(
        store: S,
        site: &str,
        mask: Arc<dyn ArgumentMask>,
        access: Arc<dyn ReadAccess>,
        settings: Settings,
    ) -> Self {
        let scope = scope_of(site);
        Self {
            chain: AuditChain::new(
                store,
                scope.clone(),
                Arc::new(NoSigner),
                CheckpointPolicy { every: 0 },
            ),
            scope,
            mask,
            access,
            settings,
            dedupe: tokio::sync::Mutex::new(Dedupe {
                seen: HashSet::new(),
                order: VecDeque::new(),
                loaded: false,
            }),
            digest: Mutex::new(None),
        }
    }

    /// The chain scope.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    fn cap(&self) -> usize {
        usize::try_from(self.settings.dedupe_window).unwrap_or(10_000)
    }

    /// The newest `count` entries, newest first, read in pages.
    async fn recent(
        &self,
        head: u64,
        count: u64,
    ) -> std::result::Result<Vec<ChainEntry>, AiAuditError> {
        let mut out = Vec::new();
        let mut upper = head;
        let floor = head.saturating_sub(count);
        while upper > floor && upper > 0 {
            let lower = upper.saturating_sub(SCAN_PAGE - 1).max(floor + 1).max(1);
            let mut page = self
                .chain
                .read_range(lower, upper)
                .await
                .map_err(|_| AiAuditError::new(CHAIN_UNAVAILABLE))?;
            page.reverse();
            out.extend(page);
            if lower == 1 {
                break;
            }
            upper = lower - 1;
        }
        Ok(out)
    }

    async fn head_seq(&self) -> std::result::Result<u64, AiAuditError> {
        Ok(self
            .chain
            .head()
            .await
            .map_err(|_| AiAuditError::new(CHAIN_UNAVAILABLE))?
            .map_or(0, |e| e.seq))
    }

    /// Reads the recent event ids and the newest digest from the chain, once.
    async fn load(&self, dedupe: &mut Dedupe) -> std::result::Result<(), AiAuditError> {
        if dedupe.loaded {
            return Ok(());
        }
        let head = self.head_seq().await?;
        let entries = self.recent(head, self.settings.dedupe_window).await?;
        let mut newest_digest = None;
        // Oldest first, so the order queue evicts the oldest first.
        for entry in entries.iter().rev() {
            if let Some(id) = text(&entry.record, "event_id") {
                dedupe.remember(id, self.cap());
            }
            if entry.record.kind == kind_of("digest") {
                newest_digest = Some(digest_of(&entry.record));
            }
        }
        if let Some(digest) = newest_digest
            && let Ok(mut slot) = self.digest.lock()
        {
            *slot = Some(digest);
        }
        dedupe.loaded = true;
        Ok(())
    }

    /// Chains `action` once per event id.
    ///
    /// # Errors
    ///
    /// [`RECORD_INVALID`] for a malformed action, [`CHAIN_UNAVAILABLE`] when the recent ids
    /// cannot be read, [`APPEND_FAILED`] when the entry is not durable.
    pub async fn append(&self, action: AiAction) -> std::result::Result<Receipt, AiAuditError> {
        let record = record_of(&action, &*self.mask)?;
        let mut dedupe = self.dedupe.lock().await;
        self.load(&mut dedupe).await?;
        if dedupe.seen.contains(&action.event_id) {
            return Ok(Receipt::Duplicate);
        }
        let entry = self.chain.append(record).await.map_err(|_| {
            tracing::warn!(scope = %self.scope, code = APPEND_FAILED, "an AI audit entry could not be chained");
            AiAuditError::new(APPEND_FAILED)
        })?;
        dedupe.remember(action.event_id, self.cap());
        Ok(Receipt::Appended { seq: entry.seq })
    }

    /// The AI entries of `doctype`/`id` that `viewer` may see, newest first, at most `limit`
    /// (and never more than 200).
    ///
    /// # Errors
    ///
    /// [`TIMELINE_REFUSED`] when the access check fails (fail closed), [`CHAIN_UNAVAILABLE`]
    /// when the chain cannot be read.
    pub async fn timeline(
        &self,
        viewer: &str,
        doctype: &str,
        id: &str,
        limit: usize,
    ) -> std::result::Result<Vec<TimelineEntry>, AiAuditError> {
        let access = match self.access.access(viewer, doctype, id).await {
            Ok(access) => access,
            Err(_) => return Err(AiAuditError::new(TIMELINE_REFUSED)),
        };
        if access == Access::None {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_TIMELINE);
        let head = self.head_seq().await?;
        let mut out = Vec::new();
        for entry in self.recent(head, MAX_SCAN_ENTRIES).await? {
            if out.len() >= limit {
                break;
            }
            let record = &entry.record;
            let Some(subject) = &record.subject else {
                continue;
            };
            if subject.doctype != doctype || subject.id != id {
                continue;
            }
            let Some(segment) = record.kind.strip_prefix("ai.") else {
                continue;
            };
            let Some(kind) = ActionKind::from_segment(segment) else {
                continue;
            };
            out.push(TimelineEntry {
                seq: entry.seq,
                at_micros: record.at,
                kind: kind.segment().to_owned(),
                agent: text(record, "agent").unwrap_or_default(),
                acting_user: text(record, "user").unwrap_or_default(),
                tool: text(record, "tool").unwrap_or_default(),
                outcome: text(record, "outcome").unwrap_or_default(),
                arguments: shown_arguments(&text(record, "args").unwrap_or_default(), access),
            });
        }
        Ok(out)
    }

    /// Builds the digest of `period` (`start = period * period_seconds`) from chain entries only.
    ///
    /// # Errors
    ///
    /// [`CHAIN_UNAVAILABLE`] when the chain cannot be read.
    pub async fn build_digest(
        &self,
        period: u64,
    ) -> std::result::Result<ReviewDigest, AiAuditError> {
        let length = i64::try_from(self.settings.digest_period_seconds).unwrap_or(i64::MAX);
        let start = i64::try_from(period).unwrap_or(0).saturating_mul(length);
        let end = start.saturating_add(length);
        let previous_start = start.saturating_sub(length);
        let head = self.head_seq().await?;
        let mut digest = ReviewDigest {
            period,
            start,
            end,
            ..ReviewDigest::default()
        };
        let mut now_by_agent: BTreeMap<String, u64> = BTreeMap::new();
        let mut before_by_agent: BTreeMap<String, u64> = BTreeMap::new();
        for entry in self.recent(head, MAX_SCAN_ENTRIES).await? {
            let record = &entry.record;
            let Some(kind) = record
                .kind
                .strip_prefix("ai.")
                .and_then(ActionKind::from_segment)
            else {
                continue;
            };
            let at = record.at.div_euclid(1_000_000);
            if at < previous_start {
                // Entries are in time order of appending: everything older is out of both periods.
                break;
            }
            let agent = text(record, "agent").unwrap_or_default();
            if at >= end {
                continue;
            }
            if at < start {
                *before_by_agent.entry(agent).or_default() += 1;
                continue;
            }
            match kind {
                ActionKind::ToolCall => digest.tool_calls += 1,
                ActionKind::Approval => digest.approvals += 1,
                ActionKind::Rejection => digest.rejections += 1,
                ActionKind::Undo => digest.undone += 1,
            }
            if text(record, "outcome").as_deref() != Some("ok") {
                digest.refused_or_failed += 1;
            }
            *now_by_agent.entry(agent).or_default() += 1;
        }
        for (agent, count) in now_by_agent {
            let previous = before_by_agent.get(&agent).copied().unwrap_or(0);
            if count >= self.settings.spike_min_actions
                && count > self.settings.spike_factor.saturating_mul(previous.max(1))
            {
                digest.anomalies.push(Anomaly {
                    agent,
                    count,
                    previous,
                });
            }
        }
        Ok(digest)
    }

    /// Records the digest of `period` unless one exists. The digest is itself a chain entry
    /// (`ai.digest`, actor `system`, event id `digest:<period>`), so the exactly-once rule
    /// makes a second run a no-op and a restart does not repeat it.
    ///
    /// # Errors
    ///
    /// [`DIGEST_FAILED`] when it cannot be built or recorded.
    pub async fn record_digest(
        &self,
        period: u64,
        at_micros: i64,
    ) -> std::result::Result<Receipt, AiAuditError> {
        let failed = |_| AiAuditError::new(DIGEST_FAILED);
        let event_id = format!("digest:{period}");
        {
            let mut dedupe = self.dedupe.lock().await;
            self.load(&mut dedupe).await.map_err(failed)?;
            if dedupe.seen.contains(&event_id) {
                return Ok(Receipt::Duplicate);
            }
        }
        let digest = self.build_digest(period).await.map_err(failed)?;
        let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
        payload.insert("event_id".into(), PayloadValue::Text(event_id.clone()));
        payload.insert("period".into(), to_int(digest.period));
        payload.insert("start".into(), PayloadValue::Int(digest.start));
        payload.insert("end".into(), PayloadValue::Int(digest.end));
        payload.insert("tool_calls".into(), to_int(digest.tool_calls));
        payload.insert("approvals".into(), to_int(digest.approvals));
        payload.insert("rejections".into(), to_int(digest.rejections));
        payload.insert("undone".into(), to_int(digest.undone));
        payload.insert("refused".into(), to_int(digest.refused_or_failed));
        let anomalies: Vec<String> = digest
            .anomalies
            .iter()
            .take(50)
            .map(|a| format!("{}={}/{}", a.agent, a.count, a.previous))
            .collect();
        payload.insert("anomalies".into(), PayloadValue::Text(anomalies.join(",")));
        let record = AuditRecord {
            kind: kind_of("digest"),
            actor: "system".to_owned(),
            on_behalf_of: None,
            subject: None,
            at: at_micros,
            payload,
            masked: true,
        };
        let mut dedupe = self.dedupe.lock().await;
        let entry = self
            .chain
            .append(record)
            .await
            .map_err(|_| AiAuditError::new(DIGEST_FAILED))?;
        dedupe.remember(event_id, self.cap());
        if let Ok(mut slot) = self.digest.lock() {
            *slot = Some(digest);
        }
        Ok(Receipt::Appended { seq: entry.seq })
    }

    /// Verifies the whole AI chain: every hash, link and sequence number.
    ///
    /// # Errors
    ///
    /// [`CHAIN_UNAVAILABLE`] when the chain cannot be read (a run that did not happen is not a
    /// pass).
    pub async fn verify(
        &self,
    ) -> std::result::Result<spot_audit_chain::VerifyReport, AiAuditError> {
        let keys = KeyRing::new();
        Verifier::new(&keys)
            .verify_range(self.chain.store(), &self.scope, 1, u64::MAX)
            .await
            .map_err(|_| AiAuditError::new(CHAIN_UNAVAILABLE))
    }

    /// The chain, for a test that tampers or reads entries.
    pub fn chain(&self) -> &AuditChain<S> {
        &self.chain
    }
}

/// The digest an `ai.digest` entry holds.
fn digest_of(record: &AuditRecord) -> ReviewDigest {
    let anomalies = text(record, "anomalies")
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| {
            let (agent, counts) = part.rsplit_once('=')?;
            let (count, previous) = counts.split_once('/')?;
            Some(Anomaly {
                agent: agent.to_owned(),
                count: count.parse().ok()?,
                previous: previous.parse().ok()?,
            })
        })
        .collect();
    ReviewDigest {
        period: int(record, "period"),
        start: match record.payload.get("start") {
            Some(PayloadValue::Int(i)) => *i,
            _ => 0,
        },
        end: match record.payload.get("end") {
            Some(PayloadValue::Int(i)) => *i,
            _ => 0,
        },
        tool_calls: int(record, "tool_calls"),
        approvals: int(record, "approvals"),
        rejections: int(record, "rejections"),
        undone: int(record, "undone"),
        refused_or_failed: int(record, "refused"),
        anomalies,
    }
}

/// The arguments of an entry as `access` lets a viewer see them.
fn shown_arguments(json: &str, access: Access) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    object
        .iter()
        .map(|(name, value)| {
            let shown = if access == Access::Masked || value.get("m").is_some() {
                HIDDEN.to_owned()
            } else if let Some(v) = value.get("v").and_then(|v| v.as_str()) {
                v.to_owned()
            } else if let Some(d) = value.get("digest").and_then(|d| d.as_str()) {
                format!("sha256:{d}")
            } else {
                HIDDEN.to_owned()
            };
            (name.clone(), shown)
        })
        .collect()
}

impl<S: AuditStore + 'static> AiAuditPort for AiAudit<S> {
    fn append(&self, action: AiAction) -> Fut<'_, std::result::Result<Receipt, AiAuditError>> {
        Box::pin(AiAudit::append(self, action))
    }

    fn timeline<'a>(
        &'a self,
        viewer: &'a str,
        doctype: &'a str,
        id: &'a str,
        limit: usize,
    ) -> Fut<'a, std::result::Result<Vec<TimelineEntry>, AiAuditError>> {
        Box::pin(AiAudit::timeline(self, viewer, doctype, id, limit))
    }

    fn latest_digest(&self) -> Option<ReviewDigest> {
        self.digest.lock().ok().and_then(|d| d.clone())
    }
}

/// The hosted service: the digest loop over an [`AiAudit`].
pub struct AiAuditService<S: AuditStore + 'static> {
    site: String,
    audit: Arc<AiAudit<S>>,
    clock: Arc<dyn Clock>,
    failures: Mutex<u32>,
}

impl<S: AuditStore + 'static> AiAuditService<S> {
    /// A service for `site` over `audit`.
    pub fn new(site: String, audit: Arc<AiAudit<S>>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            site,
            audit,
            clock,
            failures: Mutex::new(0),
        })
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }

    /// One look: records the digest of the last complete period when it is missing.
    ///
    /// # Errors
    ///
    /// The code of the failure.
    pub async fn tick(&self) -> std::result::Result<Receipt, AiAuditError> {
        let now = self.now_micros();
        let period_micros = i64::try_from(self.audit.settings.digest_period_seconds)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000_000);
        let current = now.div_euclid(period_micros.max(1));
        let Some(last_complete) = u64::try_from(current - 1).ok() else {
            // Before the end of the first period there is nothing to summarise.
            return Ok(Receipt::Duplicate);
        };
        let result = self.audit.record_digest(last_complete, now).await;
        if let Ok(mut failures) = self.failures.lock() {
            *failures = if result.is_ok() {
                0
            } else {
                failures.saturating_add(1)
            };
        }
        result
    }

    /// Consecutive failed digest ticks.
    pub fn failures(&self) -> u32 {
        self.failures.lock().map_or(0, |f| *f)
    }

    /// `ai_audit: ok (...)`, or `ai_audit: degraded (...)` after three failed ticks in a row.
    pub fn readiness_text(&self) -> String {
        let failures = self.failures();
        if failures >= 3 {
            return format!("ai_audit: degraded ({failures} digest passes in a row failed)");
        }
        format!("ai_audit: ok (scope {})", self.audit.scope())
    }
}

impl<S: AuditStore + 'static> Service for AiAuditService<S> {
    fn name(&self) -> &'static str {
        "ai_audit"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.audit
                .head_seq()
                .await
                .map(|_| ())
                .map_err(|e| format!("the AI audit chain could not be read ({})", e.code))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let interval = Duration::from_secs(self.audit.settings.digest_interval_seconds.max(60));
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            remove_provided(&self.site);
                            return Ok(());
                        }
                    }
                    _ = ticker.tick() => {
                        if let Err(error) = self.tick().await {
                            tracing::warn!(site = %self.site, code = error.code, "AI audit digest pass failed");
                        }
                    }
                }
            }
        })
    }
}

type Provided = Arc<dyn AiAuditPort>;

/// The port a running site hosts, kept across a reload (the service is not rebuilt).
fn provided() -> &'static Mutex<std::collections::HashMap<String, Provided>> {
    static PROVIDED: OnceLock<Mutex<std::collections::HashMap<String, Provided>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The AI audit port of a running site, by site name: for a harness that appends an action
/// itself. `None` before the site assembled it and after it stopped.
pub fn hosted(site: &str) -> Option<Arc<dyn AiAuditPort>> {
    provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(site)
        .cloned()
}

fn remove_provided(site: &str) {
    provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(site);
}

/// Registers the service when `[services.ai_audit] enabled = true`: builds the chain over the
/// site's pool, provides [`AiAuditPort`] as a port and adds the digest loop. A [`ReadAccess`]
/// port provided earlier decides who sees a timeline; without one the engine-backed access of
/// [`read_access`] does.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("ai_audit")?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        let existing = provided()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&site)
            .cloned();
        if let Some(port) = existing {
            ctx.provide::<dyn AiAuditPort>(Arc::clone(&port))?;
            timeline_port::provide(ctx, port)?;
        }
        return Ok(());
    }
    let env = *ctx.env()?;
    // A provider installed earlier wins; otherwise the engine-backed one decides what a viewer
    // may see (WP-AI-11.T9), and only a site name that is not a site id keeps `DenyAll`.
    let access: Arc<dyn ReadAccess> = match ctx.port::<dyn ReadAccess>() {
        Some(access) => access,
        None => read_access::engine_backed(env.live, &env.site.site, ctx.clock()),
    };
    let audit = Arc::new(AiAudit::new(
        PgAuditStore::new(env.backend.pool()),
        &env.site.site,
        Arc::new(SecretNameMask),
        access,
        settings,
    ));
    let service = AiAuditService::new(site.clone(), Arc::clone(&audit), Arc::clone(ctx.clock()));
    ctx.add(service as Arc<dyn Service>);
    let port: Arc<dyn AiAuditPort> = audit;
    ctx.provide::<dyn AiAuditPort>(Arc::clone(&port))?;
    // The timeline route (WP-AI-11.T11): the adapter over this chain, mounted with its group.
    timeline_port::provide(ctx, Arc::clone(&port))?;
    provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(site, port);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_audit_chain::InMemoryStore;

    struct Allow(Access);
    impl ReadAccess for Allow {
        fn access<'a>(
            &'a self,
            _viewer: &'a str,
            _doctype: &'a str,
            _id: &'a str,
        ) -> Fut<'a, std::result::Result<Access, String>> {
            let access = self.0;
            Box::pin(async move { Ok(access) })
        }
    }

    struct Broken;
    impl ReadAccess for Broken {
        fn access<'a>(
            &'a self,
            _viewer: &'a str,
            _doctype: &'a str,
            _id: &'a str,
        ) -> Fut<'a, std::result::Result<Access, String>> {
            Box::pin(async { Err("permission engine timed out".to_owned()) })
        }
    }

    fn audit(access: Arc<dyn ReadAccess>) -> AiAudit<InMemoryStore> {
        AiAudit::new(
            InMemoryStore::new(),
            "acme",
            Arc::new(SecretNameMask),
            access,
            Settings::default(),
        )
    }

    fn action(event: &str, kind: ActionKind, at_secs: i64) -> AiAction {
        AiAction {
            kind,
            event_id: event.to_owned(),
            agent: "bot1".to_owned(),
            acting_user: "alice".to_owned(),
            tool: "update_doc".to_owned(),
            arguments: vec![
                ("title".to_owned(), "Q3 plan".to_owned()),
                ("api_token".to_owned(), "tok-123".to_owned()),
                ("body".to_owned(), "x".repeat(300)),
            ],
            outcome: Outcome::Ok,
            subject: Some(("Task".to_owned(), "T-1".to_owned())),
            refers_to: None,
            at_micros: at_secs * 1_000_000,
        }
    }

    #[tokio::test]
    async fn an_action_is_chained_with_agent_and_user_and_verifies() {
        let a = audit(Arc::new(Allow(Access::Full)));
        let receipt = a
            .append(action("e1", ActionKind::ToolCall, 10))
            .await
            .unwrap();
        assert_eq!(receipt, Receipt::Appended { seq: 1 });
        let entry = a.chain().read_range(1, 1).await.unwrap().remove(0);
        assert_eq!(entry.record.actor, "agent:bot1");
        assert_eq!(entry.record.on_behalf_of.as_deref(), Some("user:alice"));
        assert_eq!(entry.record.kind, "ai.tool_call");
        assert!(a.verify().await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn the_same_event_id_is_chained_once() {
        let a = audit(Arc::new(Allow(Access::Full)));
        a.append(action("e1", ActionKind::ToolCall, 10))
            .await
            .unwrap();
        let again = a
            .append(action("e1", ActionKind::ToolCall, 11))
            .await
            .unwrap();
        assert_eq!(again, Receipt::Duplicate);
        assert_eq!(a.chain().head().await.unwrap().unwrap().seq, 1);
    }

    #[tokio::test]
    async fn arguments_stay_masked_in_the_chain_and_in_the_timeline() {
        let a = audit(Arc::new(Allow(Access::Full)));
        a.append(action("e1", ActionKind::ToolCall, 10))
            .await
            .unwrap();
        let entry = a.chain().read_range(1, 1).await.unwrap().remove(0);
        let stored = text(&entry.record, "args").unwrap();
        assert!(
            !stored.contains("tok-123"),
            "a secret-named argument is not stored"
        );
        assert!(
            !stored.contains(&"x".repeat(300)),
            "a long value is a digest"
        );
        let timeline = a.timeline("alice", "Task", "T-1", 10).await.unwrap();
        let args: BTreeMap<_, _> = timeline[0].arguments.iter().cloned().collect();
        assert_eq!(args["title"], "Q3 plan");
        assert_eq!(args["api_token"], HIDDEN);
        assert!(args["body"].starts_with("sha256:"));
    }

    #[tokio::test]
    async fn a_masked_viewer_sees_no_argument_value() {
        let a = audit(Arc::new(Allow(Access::Masked)));
        a.append(action("e1", ActionKind::ToolCall, 10))
            .await
            .unwrap();
        let timeline = a.timeline("bob", "Task", "T-1", 10).await.unwrap();
        assert!(timeline[0].arguments.iter().all(|(_, v)| v == HIDDEN));
    }

    #[tokio::test]
    async fn a_viewer_without_read_sees_nothing_and_a_failing_check_refuses() {
        let none = audit(Arc::new(Allow(Access::None)));
        none.append(action("e1", ActionKind::ToolCall, 10))
            .await
            .unwrap();
        assert!(
            none.timeline("bob", "Task", "T-1", 10)
                .await
                .unwrap()
                .is_empty()
        );
        let broken = audit(Arc::new(Broken));
        let error = broken.timeline("bob", "Task", "T-1", 10).await.unwrap_err();
        assert_eq!(error.code, TIMELINE_REFUSED);
    }

    #[tokio::test]
    async fn a_timeline_holds_only_its_own_document() {
        let a = audit(Arc::new(Allow(Access::Full)));
        a.append(action("e1", ActionKind::ToolCall, 10))
            .await
            .unwrap();
        let mut other = action("e2", ActionKind::ToolCall, 11);
        other.subject = Some(("Task".to_owned(), "T-2".to_owned()));
        a.append(other).await.unwrap();
        assert_eq!(
            a.timeline("alice", "Task", "T-1", 10).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn an_agent_cannot_approve_and_malformed_actions_are_refused() {
        let a = audit(Arc::new(Allow(Access::Full)));
        let mut approval = action("e1", ActionKind::Approval, 10);
        approval.acting_user = approval.agent.clone();
        assert_eq!(a.append(approval).await.unwrap_err().code, RECORD_INVALID);
        let mut empty = action("", ActionKind::ToolCall, 10);
        empty.event_id.clear();
        assert_eq!(a.append(empty).await.unwrap_err().code, RECORD_INVALID);
    }

    #[tokio::test]
    async fn the_digest_counts_and_flags_a_spike_once() {
        let settings = Settings {
            spike_min_actions: 5,
            ..Settings::default()
        };
        let a = AiAudit::new(
            InMemoryStore::new(),
            "acme",
            Arc::new(SecretNameMask),
            Arc::new(Allow(Access::Full)),
            settings,
        );
        let day = 86_400_i64;
        // Period 1 (previous): 1 call. Period 2: 7 calls, 1 approval, 1 rejection, 1 undo.
        a.append(action("p1", ActionKind::ToolCall, day + 10))
            .await
            .unwrap();
        for i in 0..7 {
            a.append(action(
                &format!("c{i}"),
                ActionKind::ToolCall,
                2 * day + 10 + i,
            ))
            .await
            .unwrap();
        }
        let mut approval = action("a1", ActionKind::Approval, 2 * day + 100);
        approval.acting_user = "carol".to_owned();
        a.append(approval).await.unwrap();
        let mut rejection = action("r1", ActionKind::Rejection, 2 * day + 101);
        rejection.acting_user = "carol".to_owned();
        rejection.outcome = Outcome::Refused;
        a.append(rejection).await.unwrap();
        a.append(action("u1", ActionKind::Undo, 2 * day + 102))
            .await
            .unwrap();

        let digest = a.build_digest(2).await.unwrap();
        assert_eq!(
            (
                digest.tool_calls,
                digest.approvals,
                digest.rejections,
                digest.undone,
                digest.refused_or_failed
            ),
            (7, 1, 1, 1, 1)
        );
        assert_eq!(digest.anomalies.len(), 1);
        assert_eq!(digest.anomalies[0].agent, "bot1");

        let first = a.record_digest(2, 3 * day * 1_000_000).await.unwrap();
        assert!(matches!(first, Receipt::Appended { .. }));
        assert_eq!(
            a.record_digest(2, 3 * day * 1_000_000).await.unwrap(),
            Receipt::Duplicate
        );
        assert_eq!(AiAuditPort::latest_digest(&a).unwrap().tool_calls, 7);
        assert!(a.verify().await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn tampering_with_an_entry_is_detected() {
        let a = audit(Arc::new(Allow(Access::Full)));
        a.append(action("e1", ActionKind::ToolCall, 10))
            .await
            .unwrap();
        a.append(action("e2", ActionKind::ToolCall, 11))
            .await
            .unwrap();
        // A fresh chain over a store whose first entry was altered.
        let mut entries = a.chain().read_range(1, 2).await.unwrap();
        entries[0].record.actor = "agent:other".to_owned();
        let store = InMemoryStore::new();
        for entry in entries {
            // The altered entry keeps its old hash, so the verifier must see the mismatch.
            let seq = entry.seq;
            store
                .append_if_head(a.scope(), seq - 1, entry)
                .await
                .unwrap();
        }
        let keys = KeyRing::new();
        let report = Verifier::new(&keys)
            .verify_range(&store, a.scope(), 1, u64::MAX)
            .await
            .unwrap();
        assert!(!report.is_ok());
        assert_eq!(report.failures[0].seq, 1);
    }

    #[test]
    fn settings_are_range_checked() {
        let mut s = Settings::default();
        assert!(s.validate().is_ok());
        s.spike_factor = 1;
        assert!(s.validate().is_err());
    }
}
