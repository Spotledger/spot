//! The post-write lifecycle hook: the only path that writes edges.
//!
//! The document engine writes a document in [`Stage::Write`](spot_app_api::Stage::Write) and
//! then runs the `after_*` steps in the **same transaction** (ADR-51 phase 3; the transaction is
//! the engine's `Work<T>` in `spot-document/src/engine/work.rs`). That is the post-write seam:
//! [`LifecycleHook::after_write`] is called there, with the document as it was loaded and as it
//! was written, and a writer bound to that transaction. An edge write that fails returns an
//! error, the step fails, and the transaction rolls back — the document write is undone with it,
//! so a document and its edges are never half-written.
//!
//! Nothing else may write an edge. The pure functions ([`edges_of`](crate::edges_of),
//! [`diff`](crate::diff)) compute *what* the change is; this hook is *where* it lands.
//!
//! [`EdgeSync`] is the reference implementation: the diff of the two documents, applied through
//! [`apply_in`]. A host with a PostgreSQL transaction builds a `PgEdgeWriter` (WP-DAG-04.T2) over
//! that transaction and calls the same hook, so the behaviour is identical on every backend.

use spot_app_api::document::Document;

use crate::diff::diff;
use crate::error::EdgeResult;
use crate::schema::EdgeSchema;
use crate::writer::{EdgeFuture, EdgeWriter, apply_in};

/// The document engine's post-write seam.
///
/// Implementations are stateless (the schema and the writer carry everything), so one value can
/// serve every operation and every DocType.
pub trait LifecycleHook: Send + Sync {
    /// Reconciles the edge set with a document write.
    ///
    /// `old` is the document as loaded (`None` for a create), `new` the document as written
    /// (`None` for a delete). `writer` is bound to the caller's transaction. The returned future
    /// is `Send`, so an async engine can await it inside its transaction.
    fn after_write<'a>(
        &'a self,
        schema: &'a EdgeSchema,
        old: Option<&'a Document>,
        new: Option<&'a Document>,
        writer: &'a mut dyn EdgeWriter,
    ) -> EdgeFuture<'a, ()>;
}

/// The reference hook: the document diff, applied through the writer.
#[derive(Clone, Copy, Debug, Default)]
pub struct EdgeSync;

impl EdgeSync {
    /// The hook.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl LifecycleHook for EdgeSync {
    fn after_write<'a>(
        &'a self,
        schema: &'a EdgeSchema,
        old: Option<&'a Document>,
        new: Option<&'a Document>,
        writer: &'a mut dyn EdgeWriter,
    ) -> EdgeFuture<'a, ()> {
        Box::pin(async move {
            let changes = diff(schema, old, new);
            apply_in(writer, &changes).await
        })
    }
}

/// Reconciles one document write through a hook, without naming the hook value.
///
/// A convenience for callers that hold a `&dyn LifecycleHook`.
pub fn sync_document<'a>(
    hook: &'a dyn LifecycleHook,
    schema: &'a EdgeSchema,
    old: Option<&'a Document>,
    new: Option<&'a Document>,
    writer: &'a mut dyn EdgeWriter,
) -> EdgeFuture<'a, ()> {
    hook.after_write(schema, old, new, writer)
}

/// The result type of a hook call, re-exported for implementors.
pub type HookResult = EdgeResult<()>;
