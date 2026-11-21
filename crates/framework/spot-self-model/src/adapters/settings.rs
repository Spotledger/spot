//! Setting source: setting changes as configuration records.
//!
//! `spot-settings` stores values, including secrets, and this crate must never carry one.
//! The adapter therefore takes a caller-written, non-sensitive change sentence; it never reads
//! a value.

use crate::SelfModelError;
use crate::adapters::{push_edge, push_node};
use crate::evidence::{ChangeKind, EvidenceRef, HistoryEntry};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData};

/// One setting change, with no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRecord {
    /// The setting key.
    pub key: String,
    /// The scope's canonical text.
    pub scope: String,
    /// Unix microseconds.
    pub at: i64,
    /// One non-sensitive sentence such as "the retry limit was lowered".
    pub change: String,
    /// What kind of change it was.
    pub change_kind: ChangeKind,
    /// The change that caused this one, when known.
    pub cause: Option<NodeRef>,
    /// The settings change record.
    pub evidence: EvidenceRef,
}

/// Builds setting nodes and their history.
#[derive(Clone, Debug, Default)]
pub struct SettingsAdapter {
    settings: Vec<SettingRecord>,
}

impl SettingsAdapter {
    /// An adapter with no settings yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one setting change.
    #[must_use]
    pub fn with_setting(mut self, setting: SettingRecord) -> Self {
        self.settings.push(setting);
        self
    }
}

impl SourceAdapter for SettingsAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Settings
    }

    fn epoch(&self) -> u64 {
        u64::try_from(self.settings.len()).unwrap_or(u64::MAX)
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        for setting in &self.settings {
            if !setting.evidence.is_complete() {
                return Err(SelfModelError::EvidenceMissing {
                    what: format!("setting:{}", setting.key),
                });
            }
            let setting_ref = NodeRef::new(NodeKind::Setting, setting.key.clone());
            push_node(
                &mut data.nodes,
                Node::new(
                    NodeKind::Setting,
                    setting.key.clone(),
                    vec![setting.evidence.clone()],
                )?
                .with_attribute("scope", setting.scope.clone())
                .with_attribute("last_change", setting.change.clone()),
            );
            if let Some(cause) = &setting.cause {
                push_node(
                    &mut data.nodes,
                    Node::new(
                        cause.kind,
                        cause.name.clone(),
                        vec![setting.evidence.clone()],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        cause.id(),
                        setting_ref.id(),
                        EdgeKind::Affects,
                        Some(setting.at),
                        vec![setting.evidence.clone()],
                    )?,
                );
            }
            data.history.push(HistoryEntry::new(
                setting_ref.id(),
                setting.at,
                setting.change_kind,
                setting.change.clone(),
                setting.evidence.clone(),
            ));
        }
        Ok(data)
    }
}
