//! [`SourceEvent`]: one thing that may start a Case, with the principal it acts as.
//!
//! Every kind of trigger has a source: a committed document event, a schedule slot, an incoming
//! webhook, a manual start, an API start, or a custom kind's own event (ADR-080). A source event
//! carries an **id** — the key that makes a second delivery start nothing — and the principal the
//! start runs as, so the permission gate can refuse it.

use serde_json::Value as Json;
use spot_app_api::prelude::Value as StorageValue;
use spot_bpm_model::NodeId;
use spot_permissions::{DocFacts, Principal};
use spot_scheduler::RunSlot;
use spot_webhooks::{CommittedEvent, EventType};
use time::OffsetDateTime;

/// An incoming webhook that verified, ready to match.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookEvent {
    /// The path it arrived on.
    pub path: String,
    /// The idempotency id: a caller-supplied header, or the body hash when there is none.
    pub idempotency_id: String,
    /// The verified request body, a JSON object.
    pub payload: Json,
    /// When the request arrived (an argument, never a wall-clock read).
    pub occurred_at: OffsetDateTime,
}

/// A person starting a process from the app.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualStart {
    /// The process being started.
    pub process: String,
    /// The trigger node to start through, when the caller names one; otherwise every manual
    /// trigger of the process matches.
    pub node: Option<NodeId>,
    /// The caller-supplied idempotency id.
    pub idempotency_id: String,
    /// The start variables.
    pub input: Json,
}

/// An API call starting a process.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiStart {
    /// The API name the call names.
    pub name: String,
    /// The process being started, when the caller names one.
    pub process: Option<String>,
    /// The caller-supplied idempotency id.
    pub idempotency_id: String,
    /// The start variables.
    pub input: Json,
}

/// A source event of a kind a later work package registered (ADR-080).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomEvent {
    /// The kind token the registry knows it by.
    pub kind: String,
    /// The source event id, the exactly-once key.
    pub source_event_id: String,
    /// The kind's own payload.
    pub payload: Json,
}

/// The source of a start, without its principal.
#[derive(Debug, Clone)]
pub enum Source {
    /// A committed document lifecycle event from the outbox.
    Document(CommittedEvent),
    /// A schedule slot the scheduler decided must run.
    Schedule(RunSlot),
    /// An incoming webhook that verified.
    Webhook(WebhookEvent),
    /// A person starting from the app.
    Manual(ManualStart),
    /// An API call.
    Api(ApiStart),
    /// A custom kind's event.
    Custom(CustomEvent),
}

/// One thing that may start a Case: its source and the principal it acts as.
#[derive(Debug, Clone)]
pub struct SourceEvent {
    /// What happened.
    pub source: Source,
    /// Who it acts as (the document event's resolved actor, the caller, or the system).
    pub principal: Principal,
}

impl SourceEvent {
    /// A source event acting as `principal`.
    #[must_use]
    pub fn new(source: Source, principal: Principal) -> Self {
        Self { source, principal }
    }

    /// A source event acting as the platform itself.
    #[must_use]
    pub fn system(source: Source) -> Self {
        Self {
            source,
            principal: Principal::system(),
        }
    }

    /// A committed document event acting as `principal`.
    #[must_use]
    pub fn document(event: CommittedEvent, principal: Principal) -> Self {
        Self::new(Source::Document(event), principal)
    }

    /// The source event id: the exactly-once key within a `(process, trigger node)`.
    #[must_use]
    pub fn id(&self) -> String {
        match &self.source {
            Source::Document(event) => event.event_id.clone(),
            Source::Schedule(slot) => slot.deduplication_key.clone(),
            Source::Webhook(event) => event.idempotency_id.clone(),
            Source::Manual(start) => start.idempotency_id.clone(),
            Source::Api(start) => start.idempotency_id.clone(),
            Source::Custom(event) => event.source_event_id.clone(),
        }
    }

    /// The committed document event, when this source is one.
    #[must_use]
    pub fn document_event(&self) -> Option<&CommittedEvent> {
        match &self.source {
            Source::Document(event) => Some(event),
            _ => None,
        }
    }

    /// The document facts the read check runs against, for a document source, built from the
    /// event's own image ([`EventImageFacts`]); `None` for any other source, and for a document
    /// event whose images are not JSON objects.
    #[must_use]
    pub fn doc_facts(&self) -> Option<DocFacts> {
        EventImageFacts.facts(self.document_event()?)
    }
}

/// Where the permission check gets the real state and values of a document event's document.
///
/// A host with the authoritative document store supplies its own source; the default is
/// [`EventImageFacts`]. `None` means the facts cannot be loaded, and a start that needs them is
/// denied (fail closed): an empty `DocFacts` would let a scope restriction pass silently.
pub trait DocFactsSource: Send + Sync {
    /// The facts of the document `event` is about, or `None` when they are unavailable.
    fn facts(&self, event: &CommittedEvent) -> Option<DocFacts>;
}

/// The default [`DocFactsSource`]: the facts the committed event itself carries.
///
/// The values come from the event's after-image (the before-image for a delete), so the
/// permission engine's scope restrictions (user permissions on link fields) see the real field
/// values. `created_by` and `state` are taken from the image when it carries them as text. An
/// event whose image is not a JSON object has no facts.
///
/// Whether the facts are *complete* enough to judge is a separate question,
/// [`check_complete`], which the intake asks of every source.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventImageFacts;

impl DocFactsSource for EventImageFacts {
    fn facts(&self, event: &CommittedEvent) -> Option<DocFacts> {
        // A delete has no after-image by definition: it uses the before-image. Any other event
        // uses its after-image only; a stale before-image would be the wrong document state.
        let image = if event.event_type == EventType::Delete {
            event.before_image.as_ref()?.as_object()?
        } else {
            event.after_image.as_object()?
        };
        let text = |key: &str| image.get(key).and_then(Json::as_str).map(str::to_owned);
        let values = image
            .iter()
            .filter_map(|(name, value)| Some((name.clone(), scalar(value)?)))
            .collect();
        Some(DocFacts {
            doctype: event.doctype.clone(),
            id: Some(event.document_id.clone()),
            created_by: text("created_by"),
            state: text("state"),
            values,
        })
    }
}

/// Whether `facts` are complete enough for the permission engine to judge: at least one scalar
/// value, and a state when the doctype is stateful (`stateful`, from the metadata). Facts with
/// nothing to compare would let a state or field condition pass against nothing.
///
/// # Errors
///
/// What is missing, as a short phrase (never a field value).
pub fn check_complete(facts: &DocFacts, stateful: bool) -> std::result::Result<(), &'static str> {
    if facts.values.is_empty() {
        return Err("the image carries no scalar field values");
    }
    if stateful && facts.state.is_none() {
        return Err("the image carries no state for a stateful doctype");
    }
    Ok(())
}

/// A scalar image field as the storage value the permission engine compares by text; nested
/// objects and arrays are not link or select values and are left out.
fn scalar(value: &Json) -> Option<StorageValue> {
    match value {
        Json::Null => Some(StorageValue::Null),
        Json::Bool(flag) => Some(StorageValue::Bool(*flag)),
        Json::String(text) => Some(StorageValue::Text(text.clone())),
        Json::Number(number) => Some(
            number
                .as_i64()
                .map_or_else(|| StorageValue::Text(number.to_string()), StorageValue::Int),
        ),
        Json::Array(_) | Json::Object(_) => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_events_contract::Topic;
    use spot_webhooks::EventType;

    fn document_event() -> CommittedEvent {
        CommittedEvent {
            event_id: "evt-7".into(),
            topic: Topic::any(EventType::Create.topic_name()).unwrap(),
            doctype: "sales_order".into(),
            document_id: "doc-9".into(),
            document_version: 1,
            event_type: EventType::Create,
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            after_image: serde_json::json!({}),
            before_image: None,
        }
    }

    #[test]
    fn a_document_source_ids_and_facts_from_the_event() {
        let event = SourceEvent::document(document_event(), Principal::system());
        assert_eq!(event.id(), "evt-7");
        let facts = event.doc_facts().unwrap();
        assert_eq!(facts.doctype, "sales_order");
        assert_eq!(facts.id.as_deref(), Some("doc-9"));
    }

    #[test]
    fn the_facts_carry_the_after_image_values_for_scope_checks() {
        let mut committed = document_event();
        committed.after_image = serde_json::json!({
            "company": "acme",
            "created_by": "u1",
            "state": "posted"
        });
        let facts = SourceEvent::document(committed, Principal::system())
            .doc_facts()
            .unwrap();
        assert_eq!(
            facts.values.get("company"),
            Some(&StorageValue::Text("acme".into()))
        );
        assert_eq!(facts.created_by.as_deref(), Some("u1"));
        assert_eq!(facts.state.as_deref(), Some("posted"));
    }

    #[test]
    fn an_event_without_an_object_image_has_no_facts() {
        let mut committed = document_event();
        committed.after_image = Json::Null;
        let event = SourceEvent::document(committed.clone(), Principal::system());
        assert!(event.doc_facts().is_none());
        // A stale before-image never stands in for a non-delete event's after-image.
        committed.before_image = Some(serde_json::json!({"company": "acme"}));
        assert!(
            SourceEvent::document(committed.clone(), Principal::system())
                .doc_facts()
                .is_none()
        );
        // A delete has no after-image by definition: it uses the before-image.
        committed.event_type = EventType::Delete;
        let facts = SourceEvent::document(committed, Principal::system())
            .doc_facts()
            .unwrap();
        assert_eq!(
            facts.values.get("company"),
            Some(&StorageValue::Text("acme".into()))
        );
    }

    #[test]
    fn completeness_needs_scalar_values_and_a_state_for_a_stateful_doctype() {
        let mut facts = DocFacts {
            doctype: "sales_order".into(),
            ..DocFacts::default()
        };
        assert!(check_complete(&facts, false).is_err());
        facts
            .values
            .insert("company".into(), StorageValue::Text("acme".into()));
        assert!(check_complete(&facts, false).is_ok());
        assert!(check_complete(&facts, true).is_err());
        facts.state = Some("posted".into());
        assert!(check_complete(&facts, true).is_ok());
    }

    #[test]
    fn a_schedule_source_ids_from_the_slot_key() {
        let slot = RunSlot::new(
            "app",
            "daily",
            jiff::Timestamp::from_second(1_700_000_000).unwrap(),
            spot_scheduler::SlotOrigin::Due,
            spot_scheduler::SlotRef::new("app", "daily"),
        );
        let event = SourceEvent::system(Source::Schedule(slot.clone()));
        assert_eq!(event.id(), slot.deduplication_key);
        assert!(event.doc_facts().is_none());
    }
}
