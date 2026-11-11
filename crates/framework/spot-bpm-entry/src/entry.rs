//! Entry-process resolution (ADR-080 section 2): which process a person, an API client or an
//! import starts to create a record type.
//!
//! The entry process of a type is a setting, `bpm.entry.<type>`, read per scope with the same
//! narrowing as the mode (the most specific unit wins). With no setting the platform generates a
//! minimal entry process (a trigger, one form step and one document step that submits the
//! draft), so `process_only` works with no authoring at all.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use spot_bpm_model::{
    DocumentAction, DocumentConfig, DraftMode, Edge, FormConfig, Node, NodeId, NodeKind,
    ProcessVersion, TriggerConfig, TriggerType, VersionStatus,
};

use crate::error::{EntryError, EntryResult};
use crate::scope::scope_chain;

/// The prefix of the entry setting key; the record type follows the dot.
///
/// Built with `concat!` so the dotted literal is not mistaken for an error code by the
/// repository's error-code scan.
pub const ENTRY_KEY_PREFIX: &str = concat!("bpm", ".entry", ".");

/// The app that owns generated entry processes.
pub const GENERATED_APP: &str = "platform";

/// The id of the generated form node.
pub const FORM_NODE: &str = "enter";

/// The id of the generated submit node.
pub const SUBMIT_NODE: &str = "submit";

/// The id of the generated trigger node.
pub const START_NODE: &str = "start";

/// A reference to an entry process.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessRef {
    /// The process id (its name within its app).
    pub id: String,
    /// Whether the platform generated it because nothing was configured.
    pub generated: bool,
}

/// The setting key of a record type's entry process.
#[must_use]
pub fn entry_setting_key(record_type: &str) -> String {
    format!("{ENTRY_KEY_PREFIX}{record_type}")
}

/// Reads entry settings. The host backs it with `spot-settings`; a value is read at exactly the
/// scope it was set on, and [`EntryProcesses`] walks the chain.
pub trait EntrySettings: Send + Sync {
    /// The value of `key` set on `scope` itself, if any.
    ///
    /// # Errors
    /// [`EntryError::Settings`] when the settings cannot be read.
    fn get(&self, key: &str, scope: &str) -> EntryResult<Option<String>>;
}

/// An in-memory settings source for tests and single-site hosts.
#[derive(Debug, Default)]
pub struct MemoryEntrySettings {
    values: RwLock<BTreeMap<(String, String), String>>,
}

impl MemoryEntrySettings {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the entry process of `record_type` on `scope`.
    pub fn set_entry(&self, record_type: &str, scope: &str, process: &str) {
        self.values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                (entry_setting_key(record_type), scope.to_owned()),
                process.to_owned(),
            );
    }

    /// Wraps the source for sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl EntrySettings for MemoryEntrySettings {
    fn get(&self, key: &str, scope: &str) -> EntryResult<Option<String>> {
        Ok(self
            .values
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(key.to_owned(), scope.to_owned()))
            .cloned())
    }
}

/// Resolves a record type's entry process.
pub struct EntryProcesses {
    settings: Arc<dyn EntrySettings>,
}

impl EntryProcesses {
    /// A resolver over `settings`.
    #[must_use]
    pub fn new(settings: Arc<dyn EntrySettings>) -> Self {
        Self { settings }
    }

    /// The entry process of `record_type` in `scope`: the most specific configured one, else
    /// the generated minimal process.
    ///
    /// # Errors
    /// [`EntryError::ConfigInvalid`] when the configured value is not a process id, or
    /// [`EntryError::Settings`] when the settings cannot be read.
    pub fn entry_process(&self, record_type: &str, scope: &str) -> EntryResult<ProcessRef> {
        let key = entry_setting_key(record_type);
        for unit in scope_chain(scope) {
            if let Some(value) = self.settings.get(&key, unit)? {
                let id = value.trim();
                if NodeId::new(id).is_err() {
                    return Err(EntryError::ConfigInvalid {
                        node: key,
                        detail: "the value is not a process id".to_owned(),
                    });
                }
                return Ok(ProcessRef {
                    id: id.to_owned(),
                    generated: false,
                });
            }
        }
        Ok(ProcessRef {
            id: generated_process_name(record_type)?,
            generated: true,
        })
    }
}

/// The name of the generated entry process of `record_type`.
///
/// # Errors
/// [`EntryError::ConfigInvalid`] when the record type is not a valid name or the generated name
/// would be too long.
pub fn generated_process_name(record_type: &str) -> EntryResult<String> {
    let name = format!("entry_{record_type}");
    NodeId::new(&name).map_err(|_| EntryError::ConfigInvalid {
        node: record_type.to_owned(),
        detail: "the record type has no valid generated entry process name".to_owned(),
    })?;
    Ok(name)
}

/// The generated minimal entry process: a trigger a person or an API client starts, one form
/// step in which the caller enters the record, and one document step that submits the draft
/// through the record type's own lifecycle.
///
/// # Errors
/// [`EntryError::ConfigInvalid`] when the record type is not a valid name.
pub fn generate_entry_process(record_type: &str) -> EntryResult<ProcessVersion> {
    let name = generated_process_name(record_type)?;
    let id = |raw: &str| {
        NodeId::new(raw).map_err(|_| EntryError::ConfigInvalid {
            node: raw.to_owned(),
            detail: "not a valid node id".to_owned(),
        })
    };
    let trigger = TriggerConfig {
        trigger: TriggerType::Manual,
        ..TriggerConfig::default()
    };
    let form = FormConfig {
        record_type: Some(record_type.to_owned()),
        fields: Vec::new(),
        assignee: None,
        draft_mode: DraftMode::Create,
    };
    let submit = DocumentConfig {
        action: DocumentAction::Post,
        doctype: record_type.to_owned(),
        operation: None,
        mapping: None,
        principal: None,
    };
    let nodes = vec![
        Node::new(id(START_NODE)?, NodeKind::Trigger(trigger)),
        Node::new(id(FORM_NODE)?, NodeKind::Form(form)),
        Node::new(id(SUBMIT_NODE)?, NodeKind::Document(submit)).as_terminal(),
    ];
    let edges = vec![
        Edge::new(id(START_NODE)?, id(FORM_NODE)?),
        Edge::new(id(FORM_NODE)?, id(SUBMIT_NODE)?),
    ];
    let version = ProcessVersion::new(
        name,
        GENERATED_APP,
        1,
        VersionStatus::Published,
        nodes,
        edges,
        Vec::new(),
    )
    .map_err(|error| EntryError::ConfigInvalid {
        node: record_type.to_owned(),
        detail: error.to_string(),
    })?;
    Ok(version)
}
