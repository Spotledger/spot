//! The Case origin rule: a Case records the `(process, version)` it started on in its first
//! event, so the hub resolves the same version. A hub that lacks the version **holds** the Case's
//! events (dependency-ordered closure) until the definition arrives; it never re-pins.

use serde::{Deserialize, Serialize};

use crate::store::DefinitionStore;

/// The pin a Case carries from its first event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseOrigin {
    /// The process.
    pub process: String,
    /// The version the Case started on.
    pub version: u64,
}

/// What a receiver does with a Case's events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The version is installed: run the Case on it.
    Run(u64),
    /// The version is not installed: hold the Case's events until it arrives. Never another
    /// version.
    Hold,
}

impl CaseOrigin {
    /// A Case's origin.
    pub fn new(process: impl Into<String>, version: u64) -> Self {
        Self {
            process: process.into(),
            version,
        }
    }

    /// The fields to put in the Case's first event.
    pub fn event_fields(&self) -> serde_json::Value {
        serde_json::json!({ "process": self.process, "process_version": self.version })
    }

    /// Reads the origin back from a first event; `None` when absent or malformed.
    pub fn from_event(fields: &serde_json::Value) -> Option<Self> {
        Some(Self {
            process: fields.get("process")?.as_str()?.to_owned(),
            version: fields.get("process_version")?.as_u64()?,
        })
    }

    /// Resolves the pinned version against what is installed.
    pub fn resolve(&self, store: &dyn DefinitionStore) -> Resolution {
        if store.hash_of(&self.process, self.version).is_some() {
            Resolution::Run(self.version)
        } else {
            Resolution::Hold
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use crate::record::{CompatClass, ProcessRecord};
    use crate::store::MemoryStore;

    use super::*;

    #[test]
    fn the_origin_round_trips_and_a_missing_version_is_held_never_repinned() {
        let origin = CaseOrigin::new("order", 2);
        assert_eq!(
            CaseOrigin::from_event(&origin.event_fields()),
            Some(origin.clone())
        );
        assert_eq!(CaseOrigin::from_event(&serde_json::json!({})), None);
        let mut store = MemoryStore::new();
        // Version 3 is installed, version 2 is not: the Case is held, not moved to 3.
        store
            .install(
                &ProcessRecord::new(
                    "acme",
                    "order",
                    3,
                    1,
                    CompatClass::Compatible,
                    r#"{"process":"order"}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(origin.resolve(&store), Resolution::Hold);
        store
            .install(
                &ProcessRecord::new(
                    "acme",
                    "order",
                    2,
                    1,
                    CompatClass::Compatible,
                    r#"{"process":"order"}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(origin.resolve(&store), Resolution::Run(2));
    }
}
