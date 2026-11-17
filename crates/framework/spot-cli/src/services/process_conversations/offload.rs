//! Calls that must not run on a runtime thread.
//!
//! `PgConversationStore` is synchronous and refuses a call from any thread that belongs to a
//! tokio runtime (it blocks on the shared bridge, and a blocked worker could deadlock the very
//! runtime the bridge's answer needs). The engine, however, runs a node handler on a runtime
//! worker, and the link host calls the receiver from one too. The two adapters here take every
//! such call off the runtime thread:
//!
//! - [`OffRuntime`] is a `ConversationStore` that forwards each call to the store it wraps on a
//!   short-lived scoped plain thread, while the caller waits (`block_in_place` on a multi-thread
//!   runtime, so the runtime keeps its other workers).
//! - [`HostHandler`] does the same for a node handler: the send and wait nodes read and write the
//!   conversation store from `run`.
//!
//! Both **fail closed**: a thread that cannot start, or one that panics, is a store failure
//! (`ConversationError::Store`) or a failed node, never a silent success, and the failure carries
//! no text from the work.

use std::sync::Arc;

use spot_bpm_conversations::{
    ConversationError, ConversationRecord, ConversationStore, Result as ConversationResult,
};
use spot_bpm_engine::{NodeCtx, NodeHandler, NodeKind, NodeOutcome};
use spot_link_exchange::{Conversation, MessageId, RejectCode};
use spot_tenant_link::LinkId;
use tokio::runtime::{Handle, RuntimeFlavor};

/// The failure code of a node whose thread did not start or died.
pub const CODE_THREAD: &str = "process_conversations.thread";

/// Runs `work` on a scoped plain thread and answers with its result, or `None` when the thread
/// could not start or panicked. On a multi-thread runtime the caller leaves the worker first.
fn on_plain_thread<T, F>(name: &str, work: F) -> Option<T>
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    let run = move || {
        std::thread::scope(|scope| {
            let spawned = std::thread::Builder::new()
                .name(name.to_owned())
                .spawn_scoped(scope, work);
            match spawned {
                Ok(handle) => handle.join().ok(),
                Err(_) => None,
            }
        })
    };
    match Handle::try_current() {
        Ok(runtime) if runtime.runtime_flavor() != RuntimeFlavor::CurrentThread => {
            tokio::task::block_in_place(run)
        }
        _ => run(),
    }
}

/// A [`ConversationStore`] whose every call runs off the runtime thread.
pub struct OffRuntime<S>(pub Arc<S>);

impl<S: ConversationStore> OffRuntime<S> {
    fn call<T: Send>(
        &self,
        work: impl FnOnce(&S) -> ConversationResult<T> + Send,
    ) -> ConversationResult<T> {
        let inner: &S = &self.0;
        on_plain_thread("process-conversations-store", move || work(inner))
            .unwrap_or(Err(ConversationError::Store))
    }
}

impl<S: ConversationStore> ConversationStore for OffRuntime<S> {
    fn load(
        &self,
        link: &LinkId,
        conversation: &Conversation,
    ) -> ConversationResult<Option<ConversationRecord>> {
        self.call(|store| store.load(link, conversation))
    }

    fn create(&self, record: ConversationRecord) -> ConversationResult<ConversationRecord> {
        self.call(move |store| store.create(record))
    }

    fn save(&self, record: ConversationRecord) -> ConversationResult<ConversationRecord> {
        self.call(move |store| store.save(record))
    }

    fn waiting(&self) -> ConversationResult<Vec<ConversationRecord>> {
        self.call(|store| store.waiting())
    }

    fn of_case(&self, case: &str) -> ConversationResult<Vec<ConversationRecord>> {
        self.call(|store| store.of_case(case))
    }

    fn open_on(&self, link: &LinkId) -> ConversationResult<usize> {
        self.call(|store| store.open_on(link))
    }

    fn rejection(&self, link: &LinkId, id: &MessageId) -> ConversationResult<Option<RejectCode>> {
        self.call(|store| store.rejection(link, id))
    }

    fn note_rejection(
        &self,
        link: LinkId,
        id: MessageId,
        code: RejectCode,
    ) -> ConversationResult<()> {
        self.call(move |store| store.note_rejection(link, id, code))
    }
}

/// A node handler run off the runtime thread, so the conversation store it calls is not.
pub struct HostHandler<H> {
    inner: H,
}

impl<H: NodeHandler> HostHandler<H> {
    /// `inner`, hosted.
    #[must_use]
    pub fn new(inner: H) -> Self {
        Self { inner }
    }
}

impl<H: NodeHandler> NodeHandler for HostHandler<H> {
    fn kind(&self) -> NodeKind {
        self.inner.kind()
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        let inner = &self.inner;
        on_plain_thread("process-conversations-node", move || inner.run(ctx)).unwrap_or_else(|| {
            NodeOutcome::fail(
                CODE_THREAD,
                "the conversation node's thread did not complete",
            )
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_bpm_conversations::MemoryConversationStore;

    #[test]
    fn a_panicking_call_is_a_store_failure_not_a_success() {
        let store = OffRuntime(Arc::new(MemoryConversationStore::new()));
        let failed: ConversationResult<()> = store.call(|_| panic!("boom"));
        assert!(matches!(failed, Err(ConversationError::Store)));
    }

    #[test]
    fn calls_reach_the_wrapped_store_from_a_plain_thread() {
        let store = OffRuntime(Arc::new(MemoryConversationStore::new()));
        assert_eq!(store.waiting().unwrap().len(), 0);
        let seen = store.call(|_| Ok(std::thread::current().name().map(str::to_owned)));
        assert_eq!(
            seen.unwrap().as_deref(),
            Some("process-conversations-store")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_call_from_a_runtime_worker_runs_on_a_plain_thread() {
        let store = OffRuntime(Arc::new(MemoryConversationStore::new()));
        let inside = store.call(|_| Ok(Handle::try_current().is_ok()));
        assert!(!inside.unwrap());
    }
}
