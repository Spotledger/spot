//! The document lifecycle port (WP-AI-07.T11): a document is viewed, and cancelled, amended,
//! recomputed or reposted through the document engine **as the remedy's own actor**, so the
//! `document.*` remedies run on a live site. It provides `dyn DocumentLifecycle`, the seam the
//! world's documents part (`world_access/documents.rs`) sits on. Operator guide:
//! `docs/running-spot/document_lifecycle.md`.
//!
//! # Start order
//!
//! `ORDER` is 1262: after the permission and engine assembly the site is built from (the module
//! reads them from the site environment, and the engine itself is looked up on every call because a
//! metadata reload replaces it), below the world composer (1265) and the remedy host (1270) that
//! read the port. The port is provided when the module has the site environment; a context built
//! for a test provides nothing.
//!
//! # A document reference
//!
//! The world names a document by one text: `<doctype>/<record id>`, for example
//! `sales_invoice/0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d6e`. A reference that does not parse is "no such
//! document".
//!
//! # What it does
//!
//! - **`view`** reads the document **as the platform** for two fixed facts only, never a field
//!   value: its lifecycle state (`draft`, `submitted` for a posted document, `cancelled`; a DocType
//!   without a lifecycle is `draft`) and whether it is **financial** (the DocType has a field
//!   classified `financial`). A document that does not exist on this site, or whose DocType is
//!   unknown, is `None`. The engine is the site's own, so another site's document is "no such
//!   document".
//! - **`run`** goes through the **guarded** engine (`GuardedEngine`) with an invocation for the
//!   actor, so the engine's own permission check, validation, field guard, links and audit apply
//!   exactly as for that user acting directly. The actor is a user record id, with or without an
//!   `agent:` or `user:` prefix (an agent is a user record whose assignments are its role set).
//!   An actor that is not one is refused. The platform's system principal is never used to
//!   change a document.
//! - **The hard floor** holds again here: a submitted financial record is never edited, only
//!   cancelled or amended (the part refuses it first; this module refuses it again).
//!
//! | Operation   | What it does                                                                  |
//! |-------------|--------------------------------------------------------------------------------|
//! | `Cancel`    | the engine's cancel of a posted document (the reason is the remedy's, not stored) |
//! | `Amend`     | the engine's correct: cancels the document and creates its amended draft; the answer is the original's state (cancelled) |
//! | `Recompute` | runs the DocType's `field_changed:<field>` steps on the loaded document; when the field's value changed, saves only that field through a normal update |
//! | `Repost`    | for a **non-financial** document, an update with no change, which runs the save steps (validation, links, hooks) again |
//! | `Relink`    | not supported: the engine validates links but has no operation that re-resolves one |
//! | `JournalEntry` | not supported: a journal entry is not a document lifecycle call           |
//!
//! `Recompute` refuses a **protected** field (one in a protection group) and a field the DocType
//! does not have: a value computed from masked inputs must never be written.
//!
//! # No data in a refusal
//!
//! Every refusal is one of the fixed texts below; no cell value, field value, message or document
//! name beyond the id appears in an error or a log line. A log line carries the actor id, the
//! document reference (DocType and id) and the operation's stable name.
//!
//! # Settings
//!
//! `[services.document_lifecycle]`: `enabled` (default `true`; `false` provides no port, and the
//! world's documents part reads `no documents`). Unknown keys are refused.
//!
//! # Ports
//!
//! Provides `dyn DocumentLifecycle`. Takes nothing from other modules.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use serde::Deserialize;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{Name, RecordId, Version};
use spot_app_api::{DocState, DocTypeDef, Document, DocumentPatch, ErrorClass};
use spot_core::{Clock, SiteId};
use spot_remedies::{DocOp, DocState as RemedyState, DocumentView};

use crate::error;
use crate::live::SiteLive;
use crate::permissions::GuardedEngine;
use crate::services::SiteCtx;
use crate::services::world_access::documents::{DeskFuture, DocumentLifecycle};

#[path = "document_lifecycle/refusal.rs"]
mod refusal;

pub use refusal::{
    BAD_STATE, DENIED, FAILED, HARD_FLOOR, INVALID, LINKED, NO_DOCUMENT, NOT_ATTACHED,
    NOT_SUPPORTED, PROTECTED_FIELD, REPOST_FINANCIAL, STALE, UNKNOWN_ACTOR, UNKNOWN_FIELD,
    refusal_of,
};

/// Start order: below the world composer (1265) and the remedy host (1270).
pub const ORDER: u32 = 1262;

/// The classification tag that makes a DocType financial.
pub const FINANCIAL_TAG: &str = "financial";

/// The component name the platform's read-only look at a document carries.
const COMPONENT: &str = "document_lifecycle";

/// `[services.document_lifecycle]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module provides the port at all.
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Where the guarded document service comes from, looked up on every call (a metadata reload
/// replaces the engine, so nothing is captured).
pub trait EngineSource: Send + Sync {
    /// The guarded service for the engine serving now, or `None` while none is attached.
    fn service(&self) -> Option<Arc<dyn DocumentService>>;
}

/// The production [`EngineSource`]: the site's live engine and authorizer behind a
/// [`GuardedEngine`], the same wrapper the native API and the MCP server use.
pub struct LiveEngine {
    live: Arc<SiteLive>,
}

impl LiveEngine {
    /// The source over a site's live hub.
    #[must_use]
    pub fn new(live: Arc<SiteLive>) -> Self {
        Self { live }
    }
}

impl EngineSource for LiveEngine {
    fn service(&self) -> Option<Arc<dyn DocumentService>> {
        let engine = self.live.engine()?;
        let authorizer = self.live.authorizer()?;
        let guarded: Arc<dyn DocumentService> = Arc::new(GuardedEngine::new(
            engine,
            authorizer,
            Arc::new(AtomicUsize::new(0)),
        ));
        Some(guarded)
    }
}

/// `<doctype>/<record id>` as its two parts; `None` when it is not one.
#[must_use]
pub fn parse_ref(text: &str) -> Option<(String, RecordId)> {
    let (doctype, id) = text.split_once('/')?;
    Name::new(doctype).ok()?;
    let id = spot_bpm_documents::ids::parse_record_id(id)?;
    Some((doctype.to_owned(), id))
}

/// The user record an actor names: `agent:<id>`, `user:<id>` or the bare id.
#[must_use]
pub fn actor_user(actor: &str) -> Option<RecordId> {
    let id = actor
        .strip_prefix("agent:")
        .or_else(|| actor.strip_prefix("user:"))
        .unwrap_or(actor);
    spot_bpm_documents::ids::parse_record_id(id)
}

/// Whether a DocType is financial: any of its fields is classified `financial`.
#[must_use]
pub fn is_financial(def: &DocTypeDef) -> bool {
    def.fields.iter().any(|field| {
        field
            .security
            .classification
            .iter()
            .any(|tag| tag.as_str() == FINANCIAL_TAG)
    })
}

/// The remedy world's state of a document: `Posted` is `Submitted`, and a document without a
/// lifecycle is a `Draft`.
#[must_use]
pub fn state_of(doc: &Document) -> RemedyState {
    match doc.state() {
        Some(DocState::Posted) => RemedyState::Submitted,
        Some(DocState::Cancelled) => RemedyState::Cancelled,
        Some(DocState::Draft) | None => RemedyState::Draft,
    }
}

/// Whether the hard floor forbids `op` on a document that looks like `view`.
#[must_use]
pub fn floor_refused(view: &DocumentView, op: &DocOp) -> bool {
    view.financial && view.state == RemedyState::Submitted && !op.is_allowed_financial_route()
}

type Desk<T> = std::result::Result<T, &'static str>;

/// A document as the platform sees it for the two facts `view` reports.
struct Seen {
    view: DocumentView,
    def: Arc<DocTypeDef>,
}

/// The lifecycle over the site's engine.
pub struct LiveLifecycle {
    engines: Arc<dyn EngineSource>,
    site: SiteId,
    clock: Arc<dyn Clock>,
}

impl LiveLifecycle {
    /// A lifecycle over `engines` for `site`; `clock` stamps the actor's invocations.
    #[must_use]
    pub fn new(engines: Arc<dyn EngineSource>, site: SiteId, clock: Arc<dyn Clock>) -> Self {
        Self {
            engines,
            site,
            clock,
        }
    }

    fn service(&self) -> Desk<Arc<dyn DocumentService>> {
        self.engines.service().ok_or(NOT_ATTACHED)
    }

    /// The platform's read-only invocation: used to look at state and metadata, never to change.
    fn platform(&self) -> Desk<Invocation> {
        let component = Name::new(COMPONENT).map_err(|_| FAILED)?;
        Ok(Invocation {
            site: self.site.clone(),
            actor: Actor::System { component },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
    }

    /// The invocation of the actor's own identity.
    fn acting(&self, user: RecordId) -> Invocation {
        Invocation {
            site: self.site.clone(),
            actor: Actor::User {
                id: user,
                via: None,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        }
    }

    /// What the platform sees of `doctype/id`: `None` when there is no such document.
    async fn seen(
        &self,
        service: &dyn DocumentService,
        doctype: &str,
        id: RecordId,
    ) -> Desk<Option<Seen>> {
        let inv = self.platform()?;
        let doc = match service.load(&inv, doctype, id).await {
            Ok(outcome) => outcome.value,
            Err(e) if matches!(e.class, ErrorClass::NotFound | ErrorClass::UnknownDocType) => {
                return Ok(None);
            }
            Err(_) => return Err(FAILED),
        };
        let def = service.meta(doctype).await.map_err(|_| FAILED)?;
        Ok(Some(Seen {
            view: DocumentView {
                state: state_of(&doc),
                financial: is_financial(&def),
            },
            def,
        }))
    }

    /// The document as the actor may load it, and its version.
    async fn loaded_as(
        service: &dyn DocumentService,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
    ) -> Desk<(Document, Version)> {
        let doc = service
            .load(inv, doctype, id)
            .await
            .map_err(|e| refusal_of(&e))?
            .value;
        let version = doc.version().ok_or(FAILED)?;
        Ok((doc, version))
    }

    /// Recomputes `field`: runs its `field_changed` steps and saves the field when its value
    /// changed. A value that did not change writes nothing, so a second recompute is a no-op.
    async fn recompute(
        service: &dyn DocumentService,
        inv: &Invocation,
        seen: &Seen,
        doctype: &str,
        id: RecordId,
        field: &str,
    ) -> Desk<()> {
        let name = Name::new(field).map_err(|_| UNKNOWN_FIELD)?;
        let Some(def) = seen.def.field(field) else {
            return Err(UNKNOWN_FIELD);
        };
        if def.security.protection_group.is_some() {
            return Err(PROTECTED_FIELD);
        }
        let (loaded, version) = Self::loaded_as(service, inv, doctype, id).await?;
        let before = loaded.get(field).clone();
        let adjusted = service
            .field_changed(inv, loaded, field)
            .await
            .map_err(|e| refusal_of(&e))?
            .value;
        let after = adjusted.get(field).clone();
        if after == before {
            return Ok(());
        }
        let mut patch = DocumentPatch::default();
        patch.fields.insert(name, after);
        service
            .update(inv, doctype, id, patch, version)
            .await
            .map_err(|e| refusal_of(&e))?;
        Ok(())
    }

    async fn view_of(&self, doc: &str) -> Desk<Option<DocumentView>> {
        let Some((doctype, id)) = parse_ref(doc) else {
            return Ok(None);
        };
        let service = self.service()?;
        Ok(self
            .seen(&*service, &doctype, id)
            .await?
            .map(|seen| seen.view))
    }

    async fn run_op(&self, actor: &str, doc: &str, op: &DocOp) -> Desk<RemedyState> {
        let (doctype, id) = parse_ref(doc).ok_or(NO_DOCUMENT)?;
        let user = actor_user(actor).ok_or(UNKNOWN_ACTOR)?;
        let service = self.service()?;
        let seen = self
            .seen(&*service, &doctype, id)
            .await?
            .ok_or(NO_DOCUMENT)?;
        if floor_refused(&seen.view, op) {
            return Err(HARD_FLOOR);
        }
        let inv = self.acting(user);
        match op {
            DocOp::Cancel { .. } => {
                let (_, version) = Self::loaded_as(&*service, &inv, &doctype, id).await?;
                service
                    .cancel(&inv, &doctype, id, version)
                    .await
                    .map_err(|e| refusal_of(&e))?;
            }
            DocOp::Amend => {
                let (_, version) = Self::loaded_as(&*service, &inv, &doctype, id).await?;
                service
                    .correct(&inv, &doctype, id, version)
                    .await
                    .map_err(|e| refusal_of(&e))?;
            }
            DocOp::Repost => {
                if seen.view.financial {
                    return Err(REPOST_FINANCIAL);
                }
                let (_, version) = Self::loaded_as(&*service, &inv, &doctype, id).await?;
                service
                    .update(&inv, &doctype, id, DocumentPatch::default(), version)
                    .await
                    .map_err(|e| refusal_of(&e))?;
            }
            DocOp::Recompute { field } => {
                Self::recompute(&*service, &inv, &seen, &doctype, id, field).await?;
            }
            DocOp::Relink { .. } | DocOp::JournalEntry { .. } => return Err(NOT_SUPPORTED),
        }
        // Ids and the operation's stable name only: no field, value or message.
        tracing::info!(actor = %actor, doc = %doc, op = op.name(), "document lifecycle: operation done");
        let after = self
            .seen(&*service, &doctype, id)
            .await?
            .ok_or(NO_DOCUMENT)?;
        Ok(after.view.state)
    }
}

impl DocumentLifecycle for LiveLifecycle {
    fn view<'a>(&'a self, doc: &'a str) -> DeskFuture<'a, Option<DocumentView>> {
        Box::pin(async move { self.view_of(doc).await })
    }

    fn run<'a>(
        &'a self,
        actor: &'a str,
        doc: &'a str,
        op: &'a DocOp,
    ) -> DeskFuture<'a, RemedyState> {
        Box::pin(async move { self.run_op(actor, doc, op).await })
    }
}

/// Registers the lifecycle port. Inert when `services.document_lifecycle.enabled = false`, and
/// in a context with no site environment.
///
/// # Errors
/// A setting that does not decode, or a port another module already provided.
pub fn register(ctx: &mut SiteCtx<'_>) -> crate::Result<()> {
    let settings: Settings = ctx.settings_or_default("document_lifecycle")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let Ok(env) = ctx.env().copied() else {
        return Ok(());
    };
    let site = SiteId::new(&env.site.site).map_err(error)?;
    let live = Arc::clone(env.live);
    let lifecycle: Arc<dyn DocumentLifecycle> = Arc::new(LiveLifecycle::new(
        Arc::new(LiveEngine::new(live)),
        site,
        Arc::clone(ctx.clock()),
    ));
    ctx.provide(lifecycle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "document_lifecycle/tests.rs"]
mod tests;
