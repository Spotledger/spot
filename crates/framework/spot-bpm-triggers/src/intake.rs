//! [`TriggerIntake`]: turning a batch of source events into exactly one Case each.
//!
//! Modelled on `spot_webhooks::WebhookIntake`. For each source event the index returns the
//! triggers it matches (a pure function), the permission gate refuses starts the principal may
//! not make, the kind's starter builds the start input, and the derived start key
//! `(process, trigger node, source event id)` is handed to the [`CaseStarter`]. A repeated key
//! returns the existing Case; nothing is started twice.
//!
//! Rejections (a broken condition, an unregistered kind, a denied start) are collected in the
//! report, never dropped silently. A `CaseStarter` failure is an infrastructure error and is
//! returned, so the caller retries the whole event; the key makes the retry safe.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::index::TriggerIndex;
use crate::permission::{StartGate, StartPermission};
use crate::registry::TriggerRegistry;
use crate::source::{DocFactsSource, EventImageFacts, SourceEvent, check_complete};
use crate::starter::{CaseStarter, StartKey, StartOutcome};
use spot_permissions::DocFacts;
use spot_webhooks::CommittedEvent;

/// A trigger that could not start for a source event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedEvent {
    /// The process.
    pub process: String,
    /// The trigger node.
    pub node: String,
    /// Why: a matcher error, `permission_denied`, or a start-input shape error.
    pub reason: String,
}

/// What one intake pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntakeReport {
    /// Cases newly started.
    pub started: u32,
    /// Starts that returned an existing Case (a redelivery).
    pub existing: u32,
    /// Triggers that were rejected.
    pub rejected: Vec<RejectedEvent>,
}

impl IntakeReport {
    /// How many Cases the pass touched (started or existing).
    #[must_use]
    pub fn cases(&self) -> u32 {
        self.started + self.existing
    }
}

/// Consumes source events and starts Cases.
pub struct TriggerIntake {
    index: Arc<TriggerIndex>,
    registry: Arc<TriggerRegistry>,
    starter: Arc<dyn CaseStarter>,
    gate: Arc<dyn StartGate>,
    facts: Arc<dyn DocFactsSource>,
    loader: Option<Arc<dyn DocFactsSource>>,
    stateful: BTreeSet<String>,
}

impl TriggerIntake {
    /// An intake over `index` and `registry`, starting through `starter` and gating with `gate`.
    #[must_use]
    pub fn new(
        index: Arc<TriggerIndex>,
        registry: Arc<TriggerRegistry>,
        starter: Arc<dyn CaseStarter>,
        gate: Arc<dyn StartGate>,
    ) -> Self {
        Self {
            index,
            registry,
            starter,
            gate,
            facts: Arc::new(EventImageFacts),
            loader: None,
            stateful: BTreeSet::new(),
        }
    }

    /// Takes the facts of a document event's document from `facts` instead of the event's own
    /// image. A source that cannot load them denies the start.
    #[must_use]
    pub fn with_facts_source(mut self, facts: Arc<dyn DocFactsSource>) -> Self {
        self.facts = facts;
        self
    }

    /// A port that loads the document when the event's own facts are incomplete (no scalar
    /// values, or no state for a stateful doctype). Only when it also fails, or its facts are
    /// also incomplete, is the start denied.
    #[must_use]
    pub fn with_document_loader(mut self, loader: Arc<dyn DocFactsSource>) -> Self {
        self.loader = Some(loader);
        self
    }

    /// The doctypes the metadata marks stateful: an event for one of them whose facts carry no
    /// state is incomplete.
    #[must_use]
    pub fn with_stateful_doctypes<I, S>(mut self, doctypes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.stateful = doctypes.into_iter().map(Into::into).collect();
        self
    }

    /// The complete facts of the event's document: from the facts source, else from the loader.
    fn resolve_facts(&self, event: &CommittedEvent) -> std::result::Result<DocFacts, Error> {
        let stateful = self.stateful.contains(&event.doctype);
        let incomplete = |reason: &str| Error::FactsIncomplete {
            doctype: event.doctype.clone(),
            reason: reason.to_owned(),
        };
        let judge = |facts: Option<DocFacts>| match facts {
            None => Err(incomplete("the facts are unavailable")),
            Some(facts) => check_complete(&facts, stateful)
                .map(|()| facts)
                .map_err(incomplete),
        };
        match judge(self.facts.facts(event)) {
            Ok(facts) => Ok(facts),
            Err(first) => match &self.loader {
                Some(loader) => judge(loader.facts(event)),
                None => Err(first),
            },
        }
    }

    /// Processes one source event.
    ///
    /// # Errors
    ///
    /// Only an infrastructure error from the `CaseStarter`; matcher failures and denied starts
    /// are reported in the [`IntakeReport`].
    pub async fn process(&self, event: &SourceEvent) -> Result<IntakeReport> {
        let mut report = IntakeReport::default();
        let matched = self.index.matching(event, &self.registry);
        for rejection in matched.rejected {
            report.rejected.push(RejectedEvent {
                process: rejection.process,
                node: rejection.node,
                reason: rejection.reason,
            });
        }

        let resolved = event
            .document_event()
            .map(|committed| self.resolve_facts(committed));
        let (doc, facts_error) = match resolved {
            Some(Ok(facts)) => (Some(facts), None),
            Some(Err(error)) => (None, Some(error)),
            None => (None, None),
        };
        // A document event whose facts are incomplete starts nothing for a person: the read
        // check would otherwise judge an empty document. The system principal is not judged.
        let source_event_id = event.id();
        for trigger in matched.matches {
            if let Some(error) = &facts_error
                && !event.principal.is_system
            {
                report.rejected.push(RejectedEvent {
                    process: trigger.process,
                    node: trigger.node.to_string(),
                    reason: format!("{}: {error}", error.code().as_str()),
                });
                continue;
            }
            let permission = StartPermission {
                principal: &event.principal,
                spec: &trigger.spec,
                doc: doc.as_ref(),
            };
            if !self.gate.may_start(&permission) {
                report.rejected.push(RejectedEvent {
                    process: trigger.process,
                    node: trigger.node.to_string(),
                    reason: "permission_denied".to_owned(),
                });
                continue;
            }
            let Some(starter) = self.registry.starter(trigger.spec.kind()) else {
                report.rejected.push(RejectedEvent {
                    process: trigger.process,
                    node: trigger.node.to_string(),
                    reason: format!("no trigger kind `{}` is registered", trigger.spec.kind()),
                });
                continue;
            };
            let input = match starter.start_input(&trigger, event) {
                Ok(input) => input,
                Err(e) => {
                    report.rejected.push(RejectedEvent {
                        process: trigger.process,
                        node: trigger.node.to_string(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let key = StartKey::new(
                trigger.process.clone(),
                trigger.node.clone(),
                source_event_id.clone(),
            );
            match self.starter.start(key, input).await? {
                StartOutcome::Started { .. } => report.started += 1,
                StartOutcome::Existing { .. } => report.existing += 1,
            }
        }
        Ok(report)
    }

    /// Processes a batch of source events, in order.
    ///
    /// # Errors
    ///
    /// The same as [`TriggerIntake::process`]; the first infrastructure failure stops the batch.
    pub async fn process_batch(&self, events: &[SourceEvent]) -> Result<Vec<IntakeReport>> {
        let mut reports = Vec::with_capacity(events.len());
        for event in events {
            reports.push(self.process(event).await?);
        }
        Ok(reports)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::permission::AllowAll;
    use crate::registry::TriggerRegistry;
    use crate::spec::{DocumentEventSpec, TriggerSpec};
    use crate::starter::InMemoryCaseStarter;
    use spot_bpm_model::NodeId;
    use spot_events_contract::Topic;
    use spot_permissions::Principal;
    use spot_webhooks::{CommittedEvent, EventType};
    use time::OffsetDateTime;

    fn index() -> TriggerIndex {
        let mut index = TriggerIndex::new();
        index.add_spec(
            "order_to_cash",
            "erp_selling",
            1,
            NodeId::new("start").unwrap(),
            TriggerSpec::DocumentEvent(DocumentEventSpec {
                doctype: "sales_order".into(),
                event: EventType::Post,
                condition: None,
            }),
        );
        index
    }

    fn intake() -> (TriggerIntake, Arc<InMemoryCaseStarter>) {
        let starter = Arc::new(InMemoryCaseStarter::new());
        let intake = TriggerIntake::new(
            Arc::new(index()),
            Arc::new(TriggerRegistry::with_builtin_kinds()),
            starter.clone(),
            Arc::new(AllowAll),
        );
        (intake, starter)
    }

    fn posted(id: &str) -> SourceEvent {
        SourceEvent::document(
            CommittedEvent {
                event_id: id.into(),
                topic: Topic::any(EventType::Post.topic_name()).unwrap(),
                doctype: "sales_order".into(),
                document_id: "doc-1".into(),
                document_version: 1,
                event_type: EventType::Post,
                occurred_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                after_image: serde_json::json!({}),
                before_image: None,
            },
            Principal::system(),
        )
    }

    #[tokio::test]
    async fn a_matching_event_starts_one_case() {
        let (intake, starter) = intake();
        let report = intake.process(&posted("e1")).await.unwrap();
        assert_eq!(report.started, 1);
        assert_eq!(report.existing, 0);
        assert!(report.rejected.is_empty());
        assert_eq!(starter.len(), 1);
    }

    #[tokio::test]
    async fn a_redelivered_event_returns_the_existing_case() {
        let (intake, starter) = intake();
        assert_eq!(intake.process(&posted("e1")).await.unwrap().started, 1);
        let second = intake.process(&posted("e1")).await.unwrap();
        assert_eq!(second.started, 0);
        assert_eq!(second.existing, 1);
        assert_eq!(starter.len(), 1);
    }

    struct NoFacts;
    impl DocFactsSource for NoFacts {
        fn facts(&self, _event: &CommittedEvent) -> Option<spot_permissions::DocFacts> {
            None
        }
    }

    fn person_intake(
        facts: Option<Arc<dyn DocFactsSource>>,
    ) -> (TriggerIntake, Arc<InMemoryCaseStarter>) {
        let starter = Arc::new(InMemoryCaseStarter::new());
        let registry = spot_meta::Registry::new(Vec::new()).unwrap();
        let snapshot = spot_permissions::Snapshot::empty(&registry.effective()).unwrap();
        let gate = crate::permission::PermissionGate::with_read_engine(
            spot_permissions::PermissionEngine::new(Arc::new(snapshot)),
        );
        let mut intake = TriggerIntake::new(
            Arc::new(index()),
            Arc::new(TriggerRegistry::with_builtin_kinds()),
            starter.clone(),
            Arc::new(gate),
        );
        if let Some(facts) = facts {
            intake = intake.with_facts_source(facts);
        }
        (intake, starter)
    }

    #[tokio::test]
    async fn an_unavailable_facts_source_denies_a_person_but_not_the_system() {
        let (intake, starter) = person_intake(Some(Arc::new(NoFacts)));
        let mut event = posted("e1");
        event.principal = Principal::with_roles("u1", &["Manager"]);
        let report = intake.process(&event).await.unwrap();
        assert_eq!(report.cases(), 0);
        assert_eq!(report.rejected.len(), 1);
        assert!(report.rejected[0].reason.contains("facts are unavailable"));
        assert!(starter.is_empty());
        assert_eq!(intake.process(&posted("e2")).await.unwrap().started, 1);
    }

    fn person_event(id: &str, image: serde_json::Value) -> SourceEvent {
        let mut event = posted(id);
        event.principal = Principal::with_roles("u1", &["Manager"]);
        if let crate::source::Source::Document(document) = &mut event.source {
            document.after_image = image;
        }
        event
    }

    /// Loads a fixed, complete document, standing in for the document service.
    struct Loader(Option<DocFacts>);
    impl DocFactsSource for Loader {
        fn facts(&self, _event: &CommittedEvent) -> Option<DocFacts> {
            self.0.clone()
        }
    }

    fn full_facts(state: Option<&str>) -> DocFacts {
        let mut facts = DocFacts {
            doctype: "sales_order".into(),
            id: Some("doc-1".into()),
            state: state.map(str::to_owned),
            ..DocFacts::default()
        };
        facts.values.insert(
            "company".into(),
            spot_app_api::prelude::Value::Text("acme".into()),
        );
        facts
    }

    fn assert_incomplete(report: &IntakeReport, starter: &InMemoryCaseStarter) {
        assert_eq!(report.cases(), 0);
        assert_eq!(report.rejected.len(), 1);
        assert!(
            report.rejected[0]
                .reason
                .starts_with("bpm_triggers.facts_incomplete"),
            "{}",
            report.rejected[0].reason
        );
        assert!(starter.is_empty());
    }

    #[tokio::test]
    async fn a_person_with_no_read_grant_starts_nothing_on_the_real_values() {
        let (intake, starter) = person_intake(None);
        let report = intake
            .process(&person_event("e1", serde_json::json!({"company": "acme"})))
            .await
            .unwrap();
        assert_eq!(report.cases(), 0);
        assert_eq!(report.rejected[0].reason, "permission_denied");
        assert!(starter.is_empty());
    }

    #[tokio::test]
    async fn a_complete_image_reaches_the_gate_and_an_allowing_gate_starts() {
        let starter = Arc::new(InMemoryCaseStarter::new());
        let intake = TriggerIntake::new(
            Arc::new(index()),
            Arc::new(TriggerRegistry::with_builtin_kinds()),
            starter.clone(),
            Arc::new(AllowAll),
        );
        let report = intake
            .process(&person_event("e1", serde_json::json!({"company": "acme"})))
            .await
            .unwrap();
        assert_eq!(report.started, 1);
    }

    #[tokio::test]
    async fn an_image_without_state_for_a_stateful_doctype_denies() {
        let (intake, starter) = person_intake(None);
        let intake = intake.with_stateful_doctypes(["sales_order"]);
        let report = intake
            .process(&person_event("e1", serde_json::json!({"company": "acme"})))
            .await
            .unwrap();
        assert_incomplete(&report, &starter);
    }

    #[tokio::test]
    async fn an_image_with_only_nested_fields_denies() {
        let (intake, starter) = person_intake(None);
        let report = intake
            .process(&person_event(
                "e1",
                serde_json::json!({"lines": [{"a": 1}], "x": {"y": 2}}),
            ))
            .await
            .unwrap();
        assert_incomplete(&report, &starter);
    }

    #[tokio::test]
    async fn a_non_object_image_denies() {
        let (intake, starter) = person_intake(None);
        for image in [
            serde_json::Value::Null,
            serde_json::json!("text"),
            serde_json::json!([1]),
        ] {
            let report = intake.process(&person_event("e1", image)).await.unwrap();
            assert_incomplete(&report, &starter);
        }
    }

    #[tokio::test]
    async fn a_delete_event_uses_the_before_image_and_an_empty_one_denies() {
        let mut delete_index = TriggerIndex::new();
        delete_index.add_spec(
            "cleanup",
            "erp_selling",
            1,
            NodeId::new("start").unwrap(),
            TriggerSpec::DocumentEvent(DocumentEventSpec {
                doctype: "sales_order".into(),
                event: EventType::Delete,
                condition: None,
            }),
        );
        let (base, starter) = person_intake(None);
        let intake = TriggerIntake {
            index: Arc::new(delete_index),
            ..base
        };
        let mut event = person_event("e1", serde_json::Value::Null);
        if let crate::source::Source::Document(document) = &mut event.source {
            document.event_type = EventType::Delete;
            document.topic = Topic::any(EventType::Delete.topic_name()).unwrap();
            document.before_image = Some(serde_json::json!({"company": "acme"}));
        }
        // Complete from the before-image: it reaches the gate, which denies on the grant.
        let report = intake.process(&event).await.unwrap();
        assert_eq!(report.rejected[0].reason, "permission_denied");
        if let crate::source::Source::Document(document) = &mut event.source {
            document.before_image = None;
        }
        let report = intake.process(&event).await.unwrap();
        assert_incomplete(&report, &starter);
    }

    fn loader_intake(loader: Loader) -> (TriggerIntake, Arc<InMemoryCaseStarter>) {
        let starter = Arc::new(InMemoryCaseStarter::new());
        let intake = TriggerIntake::new(
            Arc::new(index()),
            Arc::new(TriggerRegistry::with_builtin_kinds()),
            starter.clone(),
            Arc::new(AllowAll),
        )
        .with_stateful_doctypes(["sales_order"])
        .with_document_loader(Arc::new(loader));
        (intake, starter)
    }

    #[tokio::test]
    async fn an_incomplete_image_is_completed_by_the_loader() {
        let (intake, starter) = loader_intake(Loader(Some(full_facts(Some("posted")))));
        let report = intake
            .process(&person_event("e1", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(report.started, 1);
        assert_eq!(starter.len(), 1);
    }

    #[tokio::test]
    async fn a_failing_or_incomplete_loader_denies() {
        let (intake, starter) = loader_intake(Loader(None));
        let report = intake
            .process(&person_event("e1", serde_json::json!({})))
            .await
            .unwrap();
        assert_incomplete(&report, &starter);
        // The loader's facts lack the state a stateful doctype needs.
        let (intake, starter) = loader_intake(Loader(Some(full_facts(None))));
        let report = intake
            .process(&person_event("e2", serde_json::json!({})))
            .await
            .unwrap();
        assert_incomplete(&report, &starter);
    }

    #[tokio::test]
    async fn a_non_matching_event_starts_nothing() {
        let (intake, starter) = intake();
        let mut event = posted("e1");
        if let crate::source::Source::Document(document) = &mut event.source {
            document.doctype = "purchase_order".into();
        }
        let report = intake.process(&event).await.unwrap();
        assert_eq!(report.cases(), 0);
        assert!(starter.is_empty());
    }
}
