//! The transports a release and a discard drive: [`Dispatcher`] and [`Notifier`], with recording
//! fakes for tests.

use std::collections::HashSet;
use std::sync::Mutex;

use futures::future::BoxFuture;
use spot_core::SiteId;

use crate::effect::{EffectRecord, Sequence};
use crate::error::Result;

/// The one call a release makes: hand a held effect to its real transport, with its original
/// idempotency key.
pub trait Dispatcher: Send + Sync {
    /// Dispatches `effect`. The transport must treat `effect.key` as an idempotency key, so a
    /// crash mid-release followed by a retry never double-sends.
    fn dispatch<'a>(&'a self, effect: &'a EffectRecord) -> BoxFuture<'a, Result<()>>;
}

/// What a discard tells a person about, when a class discards with
/// [`OnDiscard::DropAndNotify`](crate::class::OnDiscard::DropAndNotify).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The site.
    pub site: SiteId,
    /// The class key.
    pub class: String,
    /// The kind.
    pub kind: String,
    /// The idempotency key, for the person to trace the original.
    pub key: String,
    /// Why the effect was discarded.
    pub reason: String,
}

/// Raises a task for a person. Carries no payload and no personal data.
pub trait Notifier: Send + Sync {
    /// Raises the notice.
    fn notify<'a>(&'a self, notice: Notice) -> BoxFuture<'a, Result<()>>;
}

/// A dispatcher/summary event sink for a discard: the summary event a discard emits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscardSummary {
    /// The site.
    pub site: SiteId,
    /// The migration.
    pub migration_id: String,
    /// How many effects were discarded.
    pub discarded: u64,
    /// How many raised a task for a person.
    pub notified: u64,
    /// Why.
    pub reason: String,
}

/// One recorded dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchCall {
    /// The idempotency key.
    pub key: String,
    /// The class key.
    pub class: String,
    /// The ordering source.
    pub source: String,
    /// The original sequence.
    pub sequence: Sequence,
}

/// A dispatcher that records calls and deduplicates on the idempotency key, standing in for an
/// idempotent transport. Configured keys fail.
#[derive(Default)]
pub struct RecordingDispatcher {
    seen: Mutex<HashSet<String>>,
    calls: Mutex<Vec<DispatchCall>>,
    fail_keys: Mutex<HashSet<String>>,
}

impl RecordingDispatcher {
    /// An empty recording dispatcher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `key` fail.
    pub fn fail_key(&self, key: impl Into<String>) {
        self.fail_keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.into());
    }

    /// Every unique dispatch, in the order it was first made.
    #[must_use]
    pub fn calls(&self) -> Vec<DispatchCall> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// How many unique effects were dispatched.
    #[must_use]
    pub fn unique_count(&self) -> usize {
        self.calls().len()
    }

    /// The calls of one source, in order.
    #[must_use]
    pub fn calls_for_source(&self, source: &str) -> Vec<DispatchCall> {
        self.calls()
            .into_iter()
            .filter(|c| c.source == source)
            .collect()
    }
}

impl std::fmt::Debug for RecordingDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingDispatcher")
            .field("unique", &self.unique_count())
            .finish_non_exhaustive()
    }
}

impl Dispatcher for RecordingDispatcher {
    fn dispatch<'a>(&'a self, effect: &'a EffectRecord) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self
                .fail_keys
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&effect.key)
            {
                return Err(crate::error::Error::DispatchFailed {
                    key: effect.key.clone(),
                    class: effect.class.key(),
                    reason: "the fake transport was told to fail this key".to_owned(),
                });
            }
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut seen = self
                .seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if seen.insert(effect.key.clone()) {
                calls.push(DispatchCall {
                    key: effect.key.clone(),
                    class: effect.class.key(),
                    source: effect.source.clone(),
                    sequence: effect.sequence,
                });
            }
            Ok(())
        })
    }
}

/// A notifier that records the tasks it raised.
#[derive(Default)]
pub struct RecordingNotifier {
    notices: Mutex<Vec<Notice>>,
}

impl RecordingNotifier {
    /// An empty recording notifier.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The notices raised, in order.
    #[must_use]
    pub fn notices(&self) -> Vec<Notice> {
        self.notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl std::fmt::Debug for RecordingNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingNotifier")
            .field("notices", &self.notices().len())
            .finish_non_exhaustive()
    }
}

impl Notifier for RecordingNotifier {
    fn notify<'a>(&'a self, notice: Notice) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.notices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(notice);
            Ok(())
        })
    }
}
