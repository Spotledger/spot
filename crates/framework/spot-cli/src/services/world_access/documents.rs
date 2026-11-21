//! The documents part of the world (WP-AI-07.T7): the real [`DocumentOps`] over a
//! [`DocumentLifecycle`], the seam through which a document is read and changed by the document
//! engine's own lifecycle (never a row write).
//!
//! # The most conservative part
//!
//! This is the one part that touches business data, so it is written to do the least:
//!
//! - **Every change goes through the lifecycle as the remedy's actor** (`live_as(actor)`): the
//!   lifecycle receives the identity and applies that actor's own permission check, validation,
//!   field guard and audit exactly as for a user acting directly. A refusal is a refusal. Without an
//!   actor (`live()` alone) every change is refused: the part never acts as a system principal.
//! - **The hard floor holds here too.** `RemedyCtx::apply_document` already refuses a direct edit of
//!   a submitted financial record with the framework's own error; the part refuses it again
//!   (`DocOp::is_allowed_financial_route` is the gate) so a caller that reaches `run` directly is
//!   stopped before the lifecycle is asked.
//! - **State names only.** A `DocumentView` carries the lifecycle state and the financial flag; a
//!   `DocumentChange` carries the operation's stable name in `detail` (never a field name, a reason
//!   text or a value). No error or log line holds a field value.
//! - **A rehearsal reads through and changes an overlay.** The lifecycle cannot list documents, so
//!   a snapshot of all of them is impossible; the rehearsal ops read the live state *read-only* and
//!   keep their own changes in memory (the same state rules as the fixture), so a rehearsal never
//!   writes to the document engine and a changed document reads as changed only in that rehearsal.
//!
//! # Why a seam, and why `from_site` finds nothing yet
//!
//! The document engine's lifecycle is reached through the site's `DocumentService` with an
//! `Invocation` for a user; no module provides it as a port a later module can take, and the
//! remedy operations (recompute a derived field, repost, relink) are not lifecycle calls it
//! exposes. So the part is written over a small seam, [`DocumentLifecycle`], that a later task
//! implements over the engine, and `from_site` answers `None` (readiness says `no documents`).
//! See the report's open questions.
//!
//! The ops are synchronous and block on the runtime handle captured when the site was built
//! (`Handle::block_on`, never `block_in_place`).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_remedies::{DocOp, DocRef, DocState, DocumentChange, DocumentOps, DocumentView, OpError};
use tokio::runtime::Handle;

use super::parts::{DocumentsPart, capture_runtime};
use crate::services::SiteCtx;
use crate::services::remedies::world::WorldError;

/// The refusal for a direct edit of a submitted financial record.
const HARD_FLOOR: &str =
    "a submitted financial record changes only through cancel, amend or a journal entry";

/// A lifecycle call: the answer, or a fixed reason it could not be given (no data in it).
pub type DeskFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

/// The seam over the document engine's lifecycle.
pub trait DocumentLifecycle: Send + Sync {
    /// The state and financial flag of `doc`; `None` when it does not exist.
    fn view<'a>(&'a self, doc: &'a str) -> DeskFuture<'a, Option<DocumentView>>;

    /// Runs `op` on `doc` through the lifecycle **as `actor`** and answers the state afterwards.
    /// The implementation applies the actor's own permission and never acts as a system principal;
    /// a refusal is `Err` with a fixed reason.
    fn run<'a>(&'a self, actor: &'a str, doc: &'a str, op: &'a DocOp) -> DeskFuture<'a, DocState>;
}

/// The documents part over a lifecycle.
pub struct LifecycleDocumentsPart {
    lifecycle: Arc<dyn DocumentLifecycle>,
    handle: Handle,
}

impl LifecycleDocumentsPart {
    /// A part over `lifecycle`; `handle` is the runtime the ops block on.
    #[must_use]
    pub fn new(lifecycle: Arc<dyn DocumentLifecycle>, handle: Handle) -> Self {
        Self { lifecycle, handle }
    }
}

/// The part for a lifecycle, or `None` when there is none or no runtime to block on.
#[must_use]
pub fn from_parts(
    lifecycle: Option<Arc<dyn DocumentLifecycle>>,
    handle: Option<Handle>,
) -> Option<Arc<dyn DocumentsPart>> {
    Some(Arc::new(LifecycleDocumentsPart::new(lifecycle?, handle?)))
}

/// The documents part for this site: `None` until a service provides a [`DocumentLifecycle`] (none
/// does yet).
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn DocumentsPart>> {
    from_parts(ctx.port::<dyn DocumentLifecycle>(), capture_runtime())
}

impl DocumentsPart for LifecycleDocumentsPart {
    fn live(&self) -> Box<dyn DocumentOps + '_> {
        Box::new(LiveOps {
            part: self,
            actor: None,
        })
    }

    fn live_as(&self, actor: &str) -> Box<dyn DocumentOps + '_> {
        Box::new(LiveOps {
            part: self,
            actor: Some(actor.to_owned()),
        })
    }

    fn rehearsal(&self) -> Result<Box<dyn DocumentOps>, WorldError> {
        Ok(Box::new(OverlayOps {
            lifecycle: Arc::clone(&self.lifecycle),
            handle: self.handle.clone(),
            overlay: BTreeMap::new(),
        }))
    }
}

fn floor_refused(view: &DocumentView, op: &DocOp) -> bool {
    view.financial && view.state == DocState::Submitted && !op.is_allowed_financial_route()
}

/// The live ops, with the identity a change is made under (if any).
struct LiveOps<'a> {
    part: &'a LifecycleDocumentsPart,
    actor: Option<String>,
}

impl DocumentOps for LiveOps<'_> {
    fn view(&self, doc: &DocRef) -> Option<DocumentView> {
        self.part
            .handle
            .block_on(self.part.lifecycle.view(doc.as_str()))
            .ok()?
    }

    fn run(&mut self, doc: &DocRef, op: &DocOp) -> Result<DocumentChange, OpError> {
        let name = op.name();
        let Some(actor) = self.actor.as_deref() else {
            return Err(OpError::new(name, "an acting identity is required"));
        };
        let view = self
            .part
            .handle
            .block_on(self.part.lifecycle.view(doc.as_str()))
            .map_err(|reason| OpError::new(name, reason))?
            .ok_or_else(|| OpError::new(name, &format!("no document `{}`", doc.as_str())))?;
        if floor_refused(&view, op) {
            return Err(OpError::new(name, HARD_FLOOR));
        }
        let after = self
            .part
            .handle
            .block_on(self.part.lifecycle.run(actor, doc.as_str(), op))
            .map_err(|reason| OpError::new(name, reason))?;
        Ok(DocumentChange {
            doc: doc.clone(),
            before: view.state,
            after,
            detail: name.to_owned(),
        })
    }
}

/// The rehearsal ops: reads fall through to the lifecycle (read-only), changes stay in memory.
struct OverlayOps {
    lifecycle: Arc<dyn DocumentLifecycle>,
    handle: Handle,
    overlay: BTreeMap<String, DocumentView>,
}

impl DocumentOps for OverlayOps {
    fn view(&self, doc: &DocRef) -> Option<DocumentView> {
        if let Some(view) = self.overlay.get(doc.as_str()) {
            return Some(*view);
        }
        self.handle
            .block_on(self.lifecycle.view(doc.as_str()))
            .ok()?
    }

    fn run(&mut self, doc: &DocRef, op: &DocOp) -> Result<DocumentChange, OpError> {
        let name = op.name();
        let mut view = self
            .view(doc)
            .ok_or_else(|| OpError::new(name, &format!("no document `{}`", doc.as_str())))?;
        if floor_refused(&view, op) {
            return Err(OpError::new(name, HARD_FLOOR));
        }
        let before = view.state;
        // The same state rules as the fixture: repost and amend leave the document submitted,
        // cancel cancels it, the rest change no state.
        match op {
            DocOp::Repost | DocOp::Amend => view.state = DocState::Submitted,
            DocOp::Cancel { .. } => view.state = DocState::Cancelled,
            DocOp::Recompute { .. } | DocOp::Relink { .. } | DocOp::JournalEntry { .. } => {}
        }
        self.overlay.insert(doc.as_str().to_owned(), view);
        Ok(DocumentChange {
            doc: doc.clone(),
            before,
            after: view.state,
            detail: name.to_owned(),
        })
    }
}
