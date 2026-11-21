//! The request store: what the authority has to know about running requests, and the in-memory
//! implementation tests and single-site hosts run on.
//!
//! The engine owns this data — it opens a request, runs a step, and moves a step to its next
//! attempt. The authority reads it and never writes it: a grant is only as current as the store
//! says, which is what makes "a newer attempt invalidates the older grants" true without the
//! authority keeping a second, drifting copy of the same fact.
//!
//! No SQL lives here (ADR-045): the storage crates implement [`RequestStore`] over their own
//! tables.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

/// What the authority may ask about the requests an engine is running.
///
/// All three questions are answered from one place, so a grant cannot be authentic against one
/// answer and a create be judged against another.
pub trait RequestStore: Send + Sync {
    /// The process `request` runs, or `None` when there is no such **open** request.
    ///
    /// A closed, cancelled or finished request answers `None` here, and every grant ever issued
    /// for it stops authenticating — that is what makes a leaked grant die with its request.
    fn process_of(&self, request: &str) -> Option<String>;

    /// The attempt `request`'s `node` is at now, or `None` when the step is not open.
    ///
    /// A grant authenticates only while its attempt is this one.
    fn current_attempt(&self, request: &str, node: &str) -> Option<u32>;

    /// Whether `request`'s `node` may create a `doctype` record at all.
    ///
    /// The step's declaration, not the attempt's state: whether this attempt may create anything
    /// else (a wait, a failure) is the engine's question.
    fn may_create(&self, request: &str, node: &str, doctype: &str) -> bool;
}

/// One request store's state.
#[derive(Debug, Default)]
struct StoreState {
    /// request -> the process it runs.
    processes: BTreeMap<String, String>,
    /// (request, node) -> the attempt the step is at.
    attempts: BTreeMap<(String, String), u32>,
    /// (request, node, doctype) -> the step may create that type.
    creatable: BTreeSet<(String, String, String)>,
}

/// An in-memory [`RequestStore`], for tests and single-site hosts.
///
/// It is writable on purpose: a test moves a step to its next attempt exactly the way the engine
/// would, and then proves the older grant no longer authenticates.
#[derive(Debug, Default)]
pub struct MemoryRequestStore {
    state: RwLock<StoreState>,
}

impl MemoryRequestStore {
    /// An empty store: no request is open, so nothing authenticates.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, StoreState> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, StoreState> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Opens `request` running `process`.
    pub fn open(&self, request: &str, process: &str) {
        self.write()
            .processes
            .insert(request.to_owned(), process.to_owned());
    }

    /// Closes `request`: its process is gone, so every grant for it stops authenticating.
    pub fn close(&self, request: &str) {
        self.write().processes.remove(request);
    }

    /// Records that `request`'s `node` is now at `attempt`.
    ///
    /// This is the engine's move to a new attempt: the previous attempt's grants stop
    /// authenticating the moment this lands.
    pub fn start_attempt(&self, request: &str, node: &str, attempt: u32) {
        self.write()
            .attempts
            .insert((request.to_owned(), node.to_owned()), attempt);
    }

    /// Lets `request`'s `node` create `doctype` records.
    pub fn allow(&self, request: &str, node: &str, doctype: &str) {
        self.write()
            .creatable
            .insert((request.to_owned(), node.to_owned(), doctype.to_owned()));
    }

    /// Wraps the store for sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl RequestStore for MemoryRequestStore {
    fn process_of(&self, request: &str) -> Option<String> {
        self.read().processes.get(request).cloned()
    }

    fn current_attempt(&self, request: &str, node: &str) -> Option<u32> {
        self.read()
            .attempts
            .get(&(request.to_owned(), node.to_owned()))
            .copied()
    }

    fn may_create(&self, request: &str, node: &str, doctype: &str) -> bool {
        self.read()
            .creatable
            .contains(&(request.to_owned(), node.to_owned(), doctype.to_owned()))
    }
}
