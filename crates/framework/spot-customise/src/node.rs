//! The node side of the swap: the seams the pipeline publishes through, and a guard that makes a
//! node refuse to serve metadata it cannot reproduce.
//!
//! [`MetaGate`] wraps a node's [`LiveMeta`]: while it holds a recorded divergence, `current`
//! returns an error instead of a snapshot (a node that cannot build what its peers announced
//! must not guess), and a successful adoption clears it. [`Node`] is a follower with a gate: the
//! host calls [`Node::poll`] on its schedule; a `metalive.divergence` is reported once and the
//! node keeps refusing until a later poll succeeds.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_meta::Definition;
use spot_meta_live::{Follower, LiveError, LiveMeta, MemoryVersionStore, Snapshot, Version};

use crate::error::CustomiseError;

/// Where the effective definitions of each published version are kept, so a node that is told
/// of version N can load N and build it.
pub trait DefinitionSink: Send + Sync {
    /// Records the definitions of `version`. Idempotent.
    fn put_definitions(&self, version: Version, definitions: Vec<Definition>);
}

impl DefinitionSink for MemoryVersionStore {
    fn put_definitions(&self, version: Version, definitions: Vec<Definition>) {
        MemoryVersionStore::put_definitions(self, version, definitions);
    }
}

/// The future a [`SwapListener`] returns.
pub type ListenerFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Told after every local swap, so the host can point its document engine at the new metadata
/// (`Engine::set_meta`) and drop caches.
pub trait SwapListener: Send + Sync {
    /// The node now serves `snapshot`.
    fn swapped<'a>(&'a self, snapshot: &'a Arc<Snapshot>) -> ListenerFuture<'a>;
}

/// A node's metadata handle that refuses to serve while it is diverged.
pub struct MetaGate {
    live: Arc<LiveMeta>,
    diverged: Mutex<Option<String>>,
}

impl MetaGate {
    /// A gate over `live`, serving.
    #[must_use]
    pub fn new(live: Arc<LiveMeta>) -> Self {
        Self {
            live,
            diverged: Mutex::new(None),
        }
    }

    /// The live metadata behind the gate, for writers (the pipeline, the follower).
    #[must_use]
    pub fn live(&self) -> &Arc<LiveMeta> {
        &self.live
    }

    /// The current snapshot, or the divergence that stops this node serving.
    ///
    /// # Errors
    ///
    /// [`CustomiseError::Divergence`] while a divergence is recorded.
    pub fn current(&self) -> Result<Arc<Snapshot>, CustomiseError> {
        match self.report() {
            Some(message) => Err(CustomiseError::Divergence { message }),
            None => Ok(self.live.current()),
        }
    }

    /// The recorded divergence, if any.
    #[must_use]
    pub fn report(&self) -> Option<String> {
        self.lock().clone()
    }

    /// Records `error` when it is a divergence (the node stops serving) and returns the error
    /// as this crate's type.
    pub fn observe(&self, error: &LiveError) -> CustomiseError {
        if error.is_divergence() {
            let message = format!("{} ({})", error, error.code());
            *self.lock() = Some(message.clone());
            return CustomiseError::Divergence { message };
        }
        let detail = match error {
            LiveError::Build { meta, plans, .. } => meta
                .iter()
                .map(ToString::to_string)
                .chain(plans.iter().map(ToString::to_string))
                .collect::<Vec<_>>()
                .join("; "),
            other => other.to_string(),
        };
        CustomiseError::Swap {
            message: format!("{detail} ({})", error.code()),
        }
    }

    /// Clears the divergence after a successful adoption.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.diverged
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for MetaGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaGate")
            .field("diverged", &self.report().is_some())
            .finish_non_exhaustive()
    }
}

/// A follower node with a [`MetaGate`].
pub struct Node {
    follower: Follower,
    gate: MetaGate,
}

impl Node {
    /// Wraps `follower`.
    #[must_use]
    pub fn new(follower: Follower) -> Self {
        let gate = MetaGate::new(Arc::clone(follower.meta()));
        Self { follower, gate }
    }

    /// Reads the version store once and adopts a newer version. Returns the adopted version.
    ///
    /// # Errors
    ///
    /// [`CustomiseError::Divergence`] when the announced content clashes with this node's (the
    /// node then refuses to serve until a later poll adopts cleanly), or
    /// [`CustomiseError::Swap`] for any other failure.
    pub async fn poll(&self) -> Result<Option<Version>, CustomiseError> {
        match self.follower.poll_once().await {
            Ok(adopted) => {
                self.gate.clear();
                Ok(adopted)
            }
            Err(error) => Err(self.gate.observe(&error)),
        }
    }

    /// The snapshot this node serves, or the divergence that stops it.
    ///
    /// # Errors
    ///
    /// [`CustomiseError::Divergence`] while diverged.
    pub fn snapshot(&self) -> Result<Arc<Snapshot>, CustomiseError> {
        self.gate.current()
    }

    /// The recorded divergence, if any.
    #[must_use]
    pub fn divergence(&self) -> Option<String> {
        self.gate.report()
    }

    /// The follower, for hosts that drive the bus themselves.
    #[must_use]
    pub fn follower(&self) -> &Follower {
        &self.follower
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node").finish_non_exhaustive()
    }
}
