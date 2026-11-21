//! The start key and the [`CaseStarter`] seam: exactly one Case per `(process, node, event)`.
//!
//! A start key is `(process, trigger node, source event id)`. It is the same on every node and
//! after every restart, so a repeated key returns the Case the starter already created
//! ([`StartOutcome::Existing`]) instead of a second one. The in-memory starter here is the
//! reference a host swaps for its engine-backed one.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::Value as Json;
use spot_bpm_model::NodeId;

use crate::error::Result;

/// The exactly-once identity of one Case start.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StartKey {
    /// The process being started.
    pub process: String,
    /// The trigger node it starts through.
    pub node: NodeId,
    /// The source event's id (an event id, a slot key, a webhook idempotency id).
    pub source_event: String,
}

impl StartKey {
    /// A start key from its three parts.
    #[must_use]
    pub fn new(process: impl Into<String>, node: NodeId, source_event: impl Into<String>) -> Self {
        Self {
            process: process.into(),
            node,
            source_event: source_event.into(),
        }
    }
}

/// The input a start carries: the pinned version, the start variables and the initiator.
#[derive(Debug, Clone, PartialEq)]
pub struct StartInput {
    /// The process version the Case pins.
    pub version: u32,
    /// The start variables (canonical JSON).
    pub variables: Json,
    /// The user the Case runs as, when there is one.
    pub initiator: Option<String>,
}

/// What a start did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// A new Case was created.
    Started {
        /// The new Case's id.
        case: String,
    },
    /// The key was already started; this is the Case it started.
    Existing {
        /// The existing Case's id.
        case: String,
    },
}

impl StartOutcome {
    /// The Case's id, however the start ended.
    #[must_use]
    pub fn case_id(&self) -> &str {
        match self {
            Self::Started { case } | Self::Existing { case } => case,
        }
    }

    /// Whether the start created a new Case.
    #[must_use]
    pub fn is_new(&self) -> bool {
        matches!(self, Self::Started { .. })
    }
}

/// Starts Cases, once per [`StartKey`].
#[async_trait]
pub trait CaseStarter: Send + Sync {
    /// Starts the Case for `key` with `input`, or returns the existing one.
    ///
    /// # Errors
    ///
    /// The host's store failure.
    async fn start(&self, key: StartKey, input: StartInput) -> Result<StartOutcome>;
}

/// A [`CaseStarter`] in one map: the reference implementation and the exactly-once test bed.
///
/// Case ids are deterministic (`case-1`, `case-2`, ...), so a test reads the same ids every run
/// without a clock or randomness. The map is behind one mutex held only across the check and the
/// insert, so two nodes racing on the same key still create one Case.
#[derive(Debug, Default)]
pub struct InMemoryCaseStarter {
    cases: Mutex<BTreeMap<StartKey, String>>,
    next: AtomicU64,
}

impl InMemoryCaseStarter {
    /// An empty starter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many Cases have been started.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no Case has been started.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The Case id already started for `key`, when there is one.
    #[must_use]
    pub fn case_for(&self, key: &StartKey) -> Option<String> {
        self.lock().get(key).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<StartKey, String>> {
        self.cases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl CaseStarter for InMemoryCaseStarter {
    async fn start(&self, key: StartKey, _input: StartInput) -> Result<StartOutcome> {
        let mut cases = self.lock();
        if let Some(existing) = cases.get(&key) {
            return Ok(StartOutcome::Existing {
                case: existing.clone(),
            });
        }
        let id = format!("case-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        cases.insert(key, id.clone());
        Ok(StartOutcome::Started { case: id })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(event: &str) -> StartKey {
        StartKey::new("order_to_cash", NodeId::new("start").unwrap(), event)
    }

    fn input() -> StartInput {
        StartInput {
            version: 1,
            variables: json!({}),
            initiator: None,
        }
    }

    #[tokio::test]
    async fn a_repeated_key_returns_the_existing_case() {
        let starter = InMemoryCaseStarter::new();
        let first = starter.start(key("e1"), input()).await.unwrap();
        assert!(first.is_new());
        assert_eq!(first.case_id(), "case-1");
        let second = starter.start(key("e1"), input()).await.unwrap();
        assert_eq!(
            second,
            StartOutcome::Existing {
                case: "case-1".into()
            }
        );
        assert_eq!(starter.len(), 1);
    }

    #[tokio::test]
    async fn different_events_start_different_cases() {
        let starter = InMemoryCaseStarter::new();
        starter.start(key("e1"), input()).await.unwrap();
        starter.start(key("e2"), input()).await.unwrap();
        assert_eq!(starter.len(), 2);
    }
}
