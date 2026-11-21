//! The authenticated-grant port: the one place a command id becomes a request context a guard
//! may trust.
//!
//! A command id is an idempotency key an external caller chooses, so a string shaped
//! `request:node:attempt` proves nothing. A guard therefore never parses a command id into a
//! request context; it asks this authority to [`authenticate`](RequestAuthority::authenticate) it,
//! and only a **grant** the engine side issued to exactly the acting person for the **current**
//! attempt of one step is a context. A forged string, a copied request id, another person's grant
//! and a replayed old attempt all authenticate to nothing.
//!
//! Both guards that judge a create use this one port: the `governed` guard in this crate
//! ([`GovernanceGuard`](crate::GovernanceGuard)) and the `process_only` entry guard in
//! `spot-bpm-entry`, which re-exports it. There is deliberately one definition: a second one
//! would be a second place to get the binding wrong.
//!
//! The production authority — an HMAC under a server secret, or a stored one-time token — is
//! WP-BPM-13.T3. [`MemoryRequests`] is the in-memory one for tests and single-site hosts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use crate::error::GovernanceError;

/// A request step the authority has authenticated for one acting person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRequest {
    /// The request.
    pub request: String,
    /// The step the grant was issued for.
    pub node: String,
    /// The attempt the grant was issued for.
    pub attempt: u32,
}

/// The trust anchor: what a guard and the ownership rule know about running requests.
///
/// A command id is an idempotency key an external caller chooses, so a string shaped
/// `request:node:attempt` proves nothing. The context a request step runs with is therefore a
/// **grant** the engine side issues ([`issue`](Self::issue)) and the guard verifies
/// ([`authenticate`](Self::authenticate)); the grant is an opaque token the production authority
/// makes unguessable (an HMAC under a server secret, or a one-time token it stores), and the API,
/// import and script edges reject or strip anything shaped like one.
pub trait RequestAuthority: Send + Sync {
    /// Issues the grant a step runs its lifecycle calls with. It is bound to the acting person
    /// and to the attempt: issuing a newer attempt invalidates the older ones.
    ///
    /// # Errors
    /// [`GovernanceError::Settings`] when the authority cannot issue.
    fn issue(
        &self,
        request: &str,
        node: &str,
        attempt: u32,
        actor: &str,
    ) -> Result<String, GovernanceError>;

    /// Verifies `command_id` as a grant issued to exactly `actor` for the **current** attempt of
    /// its step, and returns what it is for. `None` for anything else: a forged key, a grant
    /// issued to another person, or a replayed old attempt.
    fn authenticate(&self, command_id: &str, actor: &str) -> Option<VerifiedRequest>;

    /// The process a request runs, or `None` when there is no such open request.
    fn process_of(&self, request: &str) -> Option<String>;

    /// Whether the verified step may create a record of `doctype`.
    fn may_create(&self, step: &VerifiedRequest, doctype: &str) -> bool;
}

/// A request authority that knows no request: nothing is allowed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRequests;

impl RequestAuthority for NoRequests {
    fn issue(
        &self,
        _request: &str,
        _node: &str,
        _attempt: u32,
        _actor: &str,
    ) -> Result<String, GovernanceError> {
        Err(GovernanceError::Settings {
            detail: "no request authority is configured".to_owned(),
        })
    }

    fn authenticate(&self, _command_id: &str, _actor: &str) -> Option<VerifiedRequest> {
        None
    }

    fn process_of(&self, _request: &str) -> Option<String> {
        None
    }

    fn may_create(&self, _step: &VerifiedRequest, _doctype: &str) -> bool {
        false
    }
}

#[derive(Debug, Default)]
struct RequestState {
    processes: BTreeMap<String, String>,
    allowed: BTreeMap<(String, String), BTreeSet<String>>,
    // token -> (request, node, attempt, actor)
    grants: BTreeMap<String, (String, String, u32, String)>,
    // (request, node) -> the current attempt
    current: BTreeMap<(String, String), u32>,
    issued: u64,
    // Per-instance random keys, so tokens cannot be guessed from their sequence number.
    keys: std::collections::hash_map::RandomState,
}

/// An in-memory request authority for tests and single-site hosts.
///
/// It mints tokens from a per-instance random key, which is enough to prove the guard refuses
/// everything that is not a registered grant; a production authority holds a server secret.
#[derive(Debug, Default)]
pub struct MemoryRequests {
    state: RwLock<RequestState>,
}

impl MemoryRequests {
    /// An empty authority.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, RequestState> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, RequestState> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records that `request` runs `process`.
    pub fn open(&self, request: &str, process: &str) {
        self.write()
            .processes
            .insert(request.to_owned(), process.to_owned());
    }

    /// Lets `node` of `request` create `doctype`.
    pub fn allow(&self, request: &str, node: &str, doctype: &str) {
        self.write()
            .allowed
            .entry((request.to_owned(), node.to_owned()))
            .or_default()
            .insert(doctype.to_owned());
    }

    /// Wraps the authority for sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl RequestAuthority for MemoryRequests {
    fn issue(
        &self,
        request: &str,
        node: &str,
        attempt: u32,
        actor: &str,
    ) -> Result<String, GovernanceError> {
        let mut state = self.write();
        state.issued += 1;
        let token = {
            use std::hash::BuildHasher;
            format!("g{:016x}", state.keys.hash_one(state.issued))
        };
        state.grants.insert(
            token.clone(),
            (
                request.to_owned(),
                node.to_owned(),
                attempt,
                actor.to_owned(),
            ),
        );
        let current = state
            .current
            .entry((request.to_owned(), node.to_owned()))
            .or_insert(attempt);
        *current = (*current).max(attempt);
        Ok(token)
    }

    fn authenticate(&self, command_id: &str, actor: &str) -> Option<VerifiedRequest> {
        let state = self.read();
        let (request, node, attempt, holder) = state.grants.get(command_id)?;
        let current = state.current.get(&(request.clone(), node.clone()))?;
        (holder == actor && attempt == current).then(|| VerifiedRequest {
            request: request.clone(),
            node: node.clone(),
            attempt: *attempt,
        })
    }

    fn process_of(&self, request: &str) -> Option<String> {
        self.read().processes.get(request).cloned()
    }

    fn may_create(&self, step: &VerifiedRequest, doctype: &str) -> bool {
        self.read()
            .allowed
            .get(&(step.request.clone(), step.node.clone()))
            .is_some_and(|types| types.contains(doctype))
    }
}
