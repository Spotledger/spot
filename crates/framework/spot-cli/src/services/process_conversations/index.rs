//! The `message_arrived` triggers of the published processes, read from the site's definition
//! store.
//!
//! The trigger intake of the process engine (`process_triggers`) hosts the document, schedule and
//! webhook kinds. The conversation kind has its own registry here, so this module keeps its own
//! small index: the latest published version of every process, restricted to the trigger nodes
//! whose kind is `message_arrived`. The index is rebuilt only when the published set changed (a
//! publish, a retire or a reload needs no restart), and a version that does not parse or whose
//! triggers do not build is counted, never guessed at.

use std::collections::BTreeMap;

use spot_bpm_conversations::KIND_MESSAGE_ARRIVED;
use spot_bpm_definitions::{DefinitionStore, MAX_PAGE, StoredVersion, VersionState};
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_model::{VersionStatus, from_text};
use spot_bpm_triggers::{TriggerIndex, TriggerSpec};

use crate::{Error, Result};

/// What a read of the definition store found.
pub(super) struct Loaded {
    /// The `message_arrived` triggers of the latest published versions.
    pub(super) index: TriggerIndex,
    /// A fingerprint of the published set: equal fingerprints mean the same index.
    pub(super) fingerprint: String,
    /// Published versions that did not parse or whose triggers did not build.
    pub(super) broken: usize,
}

/// Reads the published versions and builds the index.
///
/// # Errors
/// The definition store's failure (its stable code only); the caller keeps the index it has.
pub(super) async fn load(defs: &PgDefinitionStore) -> Result<Loaded> {
    let store_error = |e: spot_bpm_definitions::DefinitionError| {
        Error(format!(
            "reading the process definitions failed: {}",
            e.code()
        ))
    };
    let mut latest: BTreeMap<String, StoredVersion> = BTreeMap::new();
    for name in defs.names().await.map_err(store_error)? {
        let mut after = None;
        loop {
            let page = defs
                .list(&name, after, MAX_PAGE)
                .await
                .map_err(store_error)?;
            let Some(last) = page.last().map(|v| v.version) else {
                break;
            };
            for stored in page {
                if stored.state == VersionState::Published {
                    latest.insert(name.clone(), stored);
                }
            }
            after = Some(last);
        }
    }
    let fingerprint = latest
        .values()
        .map(|v| format!("{}:{}:{}", v.name, v.version, v.content_hash))
        .collect::<Vec<_>>()
        .join(",");
    let mut index = TriggerIndex::new();
    let mut broken = 0;
    for stored in latest.values() {
        let version = from_text(&stored.text).ok().map(|mut v| {
            v.version = stored.version;
            v.status = VersionStatus::Published;
            v
        });
        match version.and_then(|v| TriggerIndex::from_versions(&[v]).ok()) {
            Some(one) => {
                for entry in one.entries() {
                    if is_message_arrived(&entry.spec) {
                        index.add_spec(
                            &entry.process,
                            &entry.app,
                            entry.version,
                            entry.node.clone(),
                            entry.spec.clone(),
                        );
                    }
                }
            }
            None => broken += 1,
        }
    }
    Ok(Loaded {
        index,
        fingerprint,
        broken,
    })
}

/// Whether a trigger spec is the conversation kind.
pub(super) fn is_message_arrived(spec: &TriggerSpec) -> bool {
    matches!(spec, TriggerSpec::Custom(custom) if custom.kind == KIND_MESSAGE_ARRIVED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_bpm_conversations::message_arrived_spec;

    #[test]
    fn only_the_conversation_kind_is_kept() {
        assert!(is_message_arrived(&message_arrived_spec("c", "m")));
    }
}
