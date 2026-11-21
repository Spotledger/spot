//! Withdrawing one grant without the whole link (WP-CTL-09.T11).
//!
//! A link carries one grant per topic, so a grant is named by its link and topic. A revoked grant
//! behaves like an absent one at every check ([`crate::LinkService::authorize`], and so
//! `prepare_send`): the refusal is `TopicNotGranted`, and the link's other grants keep working.
//! The list is a seam ([`GrantRevocations`]); [`MemoryGrantRevocations`] is the in-process list, and
//! a host that must keep it across restarts implements the trait over its own store.

use std::collections::BTreeSet;
use std::sync::Mutex;

use crate::ids::{LinkId, Topic};

/// Which grants have been withdrawn.
pub trait GrantRevocations: Send + Sync {
    /// Whether the grant of `topic` on `link` was withdrawn.
    fn is_revoked(&self, link: &LinkId, topic: &Topic) -> bool;
    /// Withdraws it; `true` when it was not withdrawn before.
    fn revoke(&self, link: &LinkId, topic: &Topic) -> bool;
}

/// The in-process revocation list.
#[derive(Debug, Default)]
pub struct MemoryGrantRevocations {
    revoked: Mutex<BTreeSet<(String, String)>>,
}

impl MemoryGrantRevocations {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<(String, String)>> {
        self.revoked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many grants are withdrawn.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether none is withdrawn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

impl GrantRevocations for MemoryGrantRevocations {
    fn is_revoked(&self, link: &LinkId, topic: &Topic) -> bool {
        self.lock().contains(&(link.to_string(), topic.to_string()))
    }

    fn revoke(&self, link: &LinkId, topic: &Topic) -> bool {
        self.lock().insert((link.to_string(), topic.to_string()))
    }
}
