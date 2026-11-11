//! The request a create attempt runs inside, as the process engine carries it, and the actor text
//! the audit uses.
//!
//! A guard never builds this value from a command id: a command id is an idempotency key a caller
//! chooses, so it proves nothing. The guards authenticate a command id through
//! [`RequestAuthority`](crate::RequestAuthority) instead, and only a grant the engine issued to the
//! acting person for the current attempt yields a context. [`CaseContext::parse`] stays for the
//! engine's own use, where the value comes from a running step, never from caller input.

use serde::{Deserialize, Serialize};
use spot_app_api::context::Actor;

/// The request a create attempt runs inside, as the process engine carries it.
///
/// It identifies the request (case) and the node (step) being run; whether that node may create a
/// given record type is answered by the verified step
/// ([`RequestAuthority::may_create`](crate::RequestAuthority::may_create)), which reads the running
/// request, not this value. The wire form is the `case:node:attempt` idempotency key the process
/// engine already stamps on a document operation (see `spot-bpm-documents`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CaseContext {
    /// The request (case) id.
    pub case: String,
    /// The node (step) id.
    pub node: String,
    /// The engine attempt number.
    pub attempt: u32,
}

impl CaseContext {
    /// A context for `case`, `node` and `attempt`.
    #[must_use]
    pub fn new(case: &str, node: &str, attempt: u32) -> Self {
        Self {
            case: case.to_owned(),
            node: node.to_owned(),
            attempt,
        }
    }

    /// Parses the `case:node:attempt` wire form. A malformed value is `None`, never a guess.
    ///
    /// This is a plain parser for the engine's own use, where the text comes from a running step.
    /// A guard must **not** use it to turn a caller-chosen command id into a request context:
    /// [`RequestAuthority::authenticate`](crate::RequestAuthority::authenticate) is the only thing
    /// that may decide a request context is real.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':');
        let case = parts.next()?;
        let node = parts.next()?;
        let attempt = parts.next()?;
        if parts.next().is_some() || case.is_empty() || node.is_empty() {
            return None;
        }
        let attempt = attempt.parse().ok()?;
        Some(Self::new(case, node, attempt))
    }

    /// The `case:node:attempt` wire form.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}:{}:{}", self.case, self.node, self.attempt)
    }
}

/// The actor text an audit record and a request grant use for `actor`: the typed principal
/// reference `user:<id>` or `consumer:<id>`, or `system` (ADR-083 section 2.2 item 4).
#[must_use]
pub fn actor_text(actor: &Actor) -> String {
    actor.principal_text()
}
