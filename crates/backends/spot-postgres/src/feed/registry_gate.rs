//! The events-contract registry check on the publish path (WP-JRN-01.T3).
//!
//! Every event enters the stream through `enqueue`, which calls [`gate`] before any SQL: an
//! unregistered platform topic, a name that is not a topic and a retired topic are refused with
//! the stable code [`CODE`]; an unregistered app topic is allowed and warned about once per
//! distinct topic per process. Nothing turns the gate off.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_events_contract::CONTRACT_VERSION;
use spot_events_contract::registry::{self, Publishable};

/// The stable error code of a refusal.
pub(crate) const CODE: &str = "feed_registry.unregistered_topic";

/// Distinct app topics that are logged; later ones are only counted.
const MAX_LOGGED: usize = 256;

static LOGGED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static UNLOGGED: AtomicU64 = AtomicU64::new(0);

/// Why a topic may not be published. The message names the topic and the contract version only.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GateRefusal {
    message: String,
}

impl GateRefusal {
    fn new(topic: &str, why: &str) -> Self {
        Self {
            message: format!(
                "{CODE}: the topic `{topic}` {why} (events contract {CONTRACT_VERSION})"
            ),
        }
    }

    /// The text for the storage error.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// Checks `topic` against the registry. `Ok` means the event may be written.
pub(crate) fn gate(topic: &str) -> Result<(), GateRefusal> {
    match registry::check_publishable(topic) {
        Err(_) => Err(GateRefusal::new(topic, "has no registered event class")),
        Ok(Publishable::Registered(_)) => match registry::lookup(topic) {
            Some(entry) if entry.retired => Err(GateRefusal::new(topic, "is retired")),
            _ => Ok(()),
        },
        Ok(Publishable::Warn) => {
            warn_once(topic);
            Ok(())
        }
    }
}

fn warn_once(topic: &str) {
    let first = {
        let mut seen = LOGGED.lock().unwrap_or_else(|p| p.into_inner());
        if seen.contains(topic) {
            false
        } else if seen.len() < MAX_LOGGED {
            seen.insert(topic.to_owned());
            true
        } else {
            UNLOGGED.fetch_add(1, Ordering::Relaxed);
            false
        }
    };
    if first {
        tracing::warn!(
            topic,
            contract_version = CONTRACT_VERSION,
            "publishing an app topic that has no registry entry; a later contract version refuses it"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn registered_platform_topic_passes() {
        assert!(gate("doc_state").is_ok());
    }

    #[test]
    fn unregistered_platform_topics_and_non_topics_are_refused() {
        for t in [
            "doc_nonsense",
            "sys_nonsense",
            "identity_nonsense",
            "Bad Topic",
            "",
        ] {
            let r = gate(t).unwrap_err();
            assert!(r.message().starts_with(CODE), "{t}");
            assert!(r.message().contains(CONTRACT_VERSION));
        }
    }

    #[test]
    fn a_retired_topic_is_refused() {
        let r = gate("doc_edge").unwrap_err();
        assert!(r.message().contains("retired"));
    }

    #[test]
    fn an_app_topic_passes() {
        assert!(gate("gate_unit_app_topic").is_ok());
        assert!(gate("gate_unit_app_topic").is_ok());
    }
}
