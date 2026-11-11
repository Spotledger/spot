//! [`TriggerIndex`]: the published trigger nodes, and the pure function that matches a source
//! event to them.
//!
//! The index is built from published process versions. Matching reads only the event and the
//! index (no store, no clock, no secret): a document-event condition is evaluated with
//! `spot-expr`, the other kinds compare their fields. A matcher that fails (a broken condition)
//! produces a [`MatchRejection`], so the intake reports it and never drops it silently.

use spot_bpm_model::{NodeId, NodeKind, ProcessVersion, VersionStatus};

use crate::error::Result;
use crate::registry::TriggerRegistry;
use crate::source::SourceEvent;
use crate::spec::TriggerSpec;

/// One published trigger node: the `(process, version)` a source event can start through.
#[derive(Debug, Clone)]
pub struct TriggerMatch {
    /// The process name.
    pub process: String,
    /// The app that owns the process.
    pub app: String,
    /// The version number the Case pins.
    pub version: u32,
    /// The trigger node.
    pub node: NodeId,
    /// The node's spec.
    pub spec: TriggerSpec,
}

/// A trigger whose matcher failed for a source event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRejection {
    /// The process.
    pub process: String,
    /// The trigger node.
    pub node: String,
    /// Why (a condition evaluation error, or an unregistered kind).
    pub reason: String,
}

/// What one matching pass found.
#[derive(Debug, Clone, Default)]
pub struct MatchReport {
    /// The triggers the event matched.
    pub matches: Vec<TriggerMatch>,
    /// The triggers whose matcher failed.
    pub rejected: Vec<MatchRejection>,
}

/// The published trigger nodes of a site.
#[derive(Debug, Clone, Default)]
pub struct TriggerIndex {
    entries: Vec<TriggerMatch>,
}

impl TriggerIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An index built from `versions`; only [`VersionStatus::Published`] versions contribute.
    ///
    /// # Errors
    ///
    /// [`Error::TriggerIncomplete`], [`Error::UnknownOperation`], [`Error::InvalidCron`] or
    /// [`Error::ScheduleInvalid`] when a published trigger node's config does not build a spec.
    pub fn from_versions(versions: &[ProcessVersion]) -> Result<Self> {
        let mut index = Self::new();
        for version in versions {
            index.add_version(version)?;
        }
        Ok(index)
    }

    /// Adds one version's trigger nodes; a non-published version contributes nothing.
    ///
    /// # Errors
    ///
    /// The same spec-building errors as [`TriggerIndex::from_versions`].
    pub fn add_version(&mut self, version: &ProcessVersion) -> Result<()> {
        if version.status != VersionStatus::Published {
            return Ok(());
        }
        for node in &version.nodes {
            if let NodeKind::Trigger(config) = &node.kind {
                let spec = TriggerSpec::from_config(&version.name, &version.app, &node.id, config)?;
                self.add_spec(
                    &version.name,
                    &version.app,
                    version.version,
                    node.id.clone(),
                    spec,
                );
            }
        }
        Ok(())
    }

    /// Adds one trigger spec directly, for a host that holds richer specs than the model carries
    /// (a document condition, a webhook secret reference, manual roles, a custom kind).
    pub fn add_spec(
        &mut self,
        process: &str,
        app: &str,
        version: u32,
        node: NodeId,
        spec: TriggerSpec,
    ) {
        self.entries.push(TriggerMatch {
            process: process.to_owned(),
            app: app.to_owned(),
            version,
            node,
            spec,
        });
    }

    /// The published trigger nodes, in the order they were added.
    #[must_use]
    pub fn entries(&self) -> &[TriggerMatch] {
        &self.entries
    }

    /// Whether the index holds no trigger.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The triggers `event` matches, and the triggers whose matcher failed.
    ///
    /// This is a pure function: every matcher reads only `event` and its spec. An unregistered
    /// kind or a condition evaluation error is a [`MatchRejection`], never a silent drop.
    #[must_use]
    pub fn matching(&self, event: &SourceEvent, registry: &TriggerRegistry) -> MatchReport {
        let mut report = MatchReport::default();
        for entry in &self.entries {
            let Some(matcher) = registry.matcher(entry.spec.kind()) else {
                report.rejected.push(MatchRejection {
                    process: entry.process.clone(),
                    node: entry.node.to_string(),
                    reason: format!("no trigger kind `{}` is registered", entry.spec.kind()),
                });
                continue;
            };
            match matcher.matches(entry, event) {
                Ok(true) => report.matches.push(entry.clone()),
                Ok(false) => {}
                Err(e) => report.rejected.push(MatchRejection {
                    process: entry.process.clone(),
                    node: entry.node.to_string(),
                    reason: e.to_string(),
                }),
            }
        }
        report
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::registry::TriggerRegistry;
    use crate::source::{Source, SourceEvent};
    use crate::spec::DocumentEventSpec;
    use spot_bpm_model::{
        Edge, Node, ProcessVersion, Rule, TriggerConfig, TriggerType, VersionStatus,
    };
    use spot_events_contract::Topic;
    use spot_permissions::Principal;
    use spot_webhooks::{CommittedEvent, EventType};
    use time::OffsetDateTime;

    fn node(id: &str, kind: NodeKind) -> Node {
        Node::new(NodeId::new(id).unwrap(), kind)
    }

    fn document_trigger(doctype: &str, event: &str) -> NodeKind {
        NodeKind::Trigger(TriggerConfig {
            trigger: TriggerType::DocumentEvent,
            doctype: Some(doctype.into()),
            event: Some(event.into()),
            ..TriggerConfig::default()
        })
    }

    fn version(status: VersionStatus, nodes: Vec<Node>) -> ProcessVersion {
        ProcessVersion::new(
            "order_to_cash",
            "erp_selling",
            1,
            status,
            nodes,
            Vec::<Edge>::new(),
            Vec::<Rule>::new(),
        )
        .unwrap()
    }

    fn document_event(
        event_type: EventType,
        doctype: &str,
        after: serde_json::Value,
    ) -> SourceEvent {
        SourceEvent::document(
            CommittedEvent {
                event_id: "evt-1".into(),
                topic: Topic::any(event_type.topic_name()).unwrap(),
                doctype: doctype.into(),
                document_id: "doc-1".into(),
                document_version: 1,
                event_type,
                occurred_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                after_image: after,
                before_image: None,
            },
            Principal::system(),
        )
    }

    #[test]
    fn a_published_document_trigger_matches_its_doctype_and_operation() {
        let index = TriggerIndex::from_versions(&[version(
            VersionStatus::Published,
            vec![node("start", document_trigger("sales_order", "post"))],
        )])
        .unwrap();
        let registry = TriggerRegistry::with_builtin_kinds();
        let report = index.matching(
            &document_event(EventType::Post, "sales_order", serde_json::json!({})),
            &registry,
        );
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].process, "order_to_cash");
        assert_eq!(report.matches[0].version, 1);
        assert_eq!(report.matches[0].node.as_str(), "start");
        assert!(report.rejected.is_empty());

        let wrong = index.matching(
            &document_event(EventType::Create, "sales_order", serde_json::json!({})),
            &registry,
        );
        assert!(wrong.matches.is_empty());
    }

    #[test]
    fn a_draft_version_contributes_nothing() {
        let index = TriggerIndex::from_versions(&[version(
            VersionStatus::Draft,
            vec![node("start", document_trigger("sales_order", "post"))],
        )])
        .unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn a_document_condition_filters_matches() {
        let mut index = TriggerIndex::new();
        index.add_spec(
            "order_to_cash",
            "erp_selling",
            1,
            NodeId::new("start").unwrap(),
            TriggerSpec::DocumentEvent(DocumentEventSpec {
                doctype: "sales_order".into(),
                event: EventType::Update,
                condition: Some("doc.status == 'Done'".into()),
            }),
        );
        let registry = TriggerRegistry::with_builtin_kinds();
        let open = index.matching(
            &document_event(
                EventType::Update,
                "sales_order",
                serde_json::json!({"status": "Open"}),
            ),
            &registry,
        );
        assert!(open.matches.is_empty());
        let done = index.matching(
            &document_event(
                EventType::Update,
                "sales_order",
                serde_json::json!({"status": "Done"}),
            ),
            &registry,
        );
        assert_eq!(done.matches.len(), 1);
    }

    #[test]
    fn a_broken_condition_is_a_rejection_not_a_match() {
        let mut index = TriggerIndex::new();
        index.add_spec(
            "order_to_cash",
            "erp_selling",
            1,
            NodeId::new("start").unwrap(),
            TriggerSpec::DocumentEvent(DocumentEventSpec {
                doctype: "sales_order".into(),
                event: EventType::Update,
                condition: Some("doc.status ==".into()),
            }),
        );
        let registry = TriggerRegistry::with_builtin_kinds();
        let report = index.matching(
            &document_event(
                EventType::Update,
                "sales_order",
                serde_json::json!({"status": "Open"}),
            ),
            &registry,
        );
        assert!(report.matches.is_empty());
        assert_eq!(report.rejected.len(), 1);
    }

    #[test]
    fn an_unregistered_kind_is_a_rejection() {
        let mut index = TriggerIndex::new();
        index.add_spec(
            "conversation",
            "app",
            1,
            NodeId::new("start").unwrap(),
            TriggerSpec::Custom(crate::spec::CustomSpec {
                kind: "message_arrived".into(),
                config: serde_json::json!({}),
            }),
        );
        let registry = TriggerRegistry::with_builtin_kinds();
        let report = index.matching(
            &SourceEvent::system(Source::Custom(crate::source::CustomEvent {
                kind: "message_arrived".into(),
                source_event_id: "m1".into(),
                payload: serde_json::json!({}),
            })),
            &registry,
        );
        assert!(report.matches.is_empty());
        assert_eq!(report.rejected.len(), 1);
    }
}
