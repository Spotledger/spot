//! The open registry of trigger kinds (ADR-080): a kind's matcher and its starter.
//!
//! The five built-in kinds are registered through the same [`TriggerRegistry::register_kind`] a
//! later work package calls to add a kind such as `message_arrived`. A matcher is a pure
//! predicate over a spec and a source event; a starter builds the [`StartInput`] for a match.
//! Neither reads a clock, a store or a secret.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use spot_scheduler::slot::rfc3339;
use spot_webhooks::evaluate_condition;

use crate::error::{Error, Result};
use crate::index::TriggerMatch;
use crate::source::{Source, SourceEvent};
use crate::starter::StartInput;
use crate::{KIND_API, KIND_DOCUMENT_EVENT, KIND_MANUAL, KIND_SCHEDULE, KIND_WEBHOOK};

/// A trigger kind's matcher: whether a spec matches a source event.
pub trait TriggerMatcher: Send + Sync {
    /// Whether `matched`'s spec matches `event`.
    ///
    /// # Errors
    ///
    /// Only an evaluation error (a broken condition); "no match" is `Ok(false)`.
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool>;
}

/// A trigger kind's starter: the start input for a matched trigger.
pub trait TriggerStarter: Send + Sync {
    /// The [`StartInput`] for a match of `matched` against `event`.
    ///
    /// # Errors
    ///
    /// When the source event is not the shape the kind expects.
    fn start_input(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<StartInput>;
}

/// One registered kind.
#[derive(Clone)]
struct KindEntry {
    matcher: Arc<dyn TriggerMatcher>,
    starter: Arc<dyn TriggerStarter>,
}

/// The registry of trigger kinds.
///
/// Cheap to clone (the handlers are `Arc`'d) and safe to share between the index and the intake.
#[derive(Clone, Default)]
pub struct TriggerRegistry {
    kinds: BTreeMap<String, KindEntry>,
}

impl TriggerRegistry {
    /// An empty registry: no kind matches anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the five built-in kinds registered.
    #[must_use]
    pub fn with_builtin_kinds() -> Self {
        let mut registry = Self::new();
        registry.register_kind(
            KIND_DOCUMENT_EVENT,
            Arc::new(DocumentEventKind),
            Arc::new(DocumentEventKind),
        );
        registry.register_kind(
            KIND_SCHEDULE,
            Arc::new(ScheduleKind),
            Arc::new(ScheduleKind),
        );
        registry.register_kind(KIND_WEBHOOK, Arc::new(WebhookKind), Arc::new(WebhookKind));
        registry.register_kind(KIND_MANUAL, Arc::new(ManualKind), Arc::new(ManualKind));
        registry.register_kind(KIND_API, Arc::new(ApiKind), Arc::new(ApiKind));
        registry
    }

    /// Registers (or replaces) a kind's matcher and starter.
    pub fn register_kind(
        &mut self,
        kind: impl Into<String>,
        matcher: Arc<dyn TriggerMatcher>,
        starter: Arc<dyn TriggerStarter>,
    ) -> &mut Self {
        self.kinds
            .insert(kind.into(), KindEntry { matcher, starter });
        self
    }

    /// Whether a kind is registered.
    #[must_use]
    pub fn knows(&self, kind: &str) -> bool {
        self.kinds.contains_key(kind)
    }

    /// The kind's matcher, when it is registered.
    #[must_use]
    pub fn matcher(&self, kind: &str) -> Option<&Arc<dyn TriggerMatcher>> {
        self.kinds.get(kind).map(|entry| &entry.matcher)
    }

    /// The kind's starter, when it is registered.
    #[must_use]
    pub fn starter(&self, kind: &str) -> Option<&Arc<dyn TriggerStarter>> {
        self.kinds.get(kind).map(|entry| &entry.starter)
    }
}

/// The built-in `document_event` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct DocumentEventKind;

impl TriggerMatcher for DocumentEventKind {
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool> {
        let (Some(spec), Some(document)) =
            (matched.spec.as_document_event(), event.document_event())
        else {
            return Ok(false);
        };
        if spec.doctype != document.doctype || spec.event != document.event_type {
            return Ok(false);
        }
        match &spec.condition {
            None => Ok(true),
            Some(condition) => {
                match evaluate_condition(condition, &document.after_image, document.occurred_at) {
                    Ok(result) => Ok(result),
                    Err(e) => Err(Error::ConditionEvaluationFailed {
                        condition: condition.clone(),
                        reason: e.to_string(),
                    }),
                }
            }
        }
    }
}

impl TriggerStarter for DocumentEventKind {
    fn start_input(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<StartInput> {
        let document = event.document_event().ok_or_else(|| Error::StartFailed {
            process: String::new(),
            node: String::new(),
            reason: "a document-event start needs a document event".to_owned(),
        })?;
        Ok(StartInput {
            version: matched.version,
            variables: json!({
                "doctype": document.doctype,
                "document_id": document.document_id,
                "document_version": document.document_version,
                "event": document.event_type.as_str(),
                "document": document.after_image,
                "before": document.before_image,
            }),
            initiator: initiator(event),
        })
    }
}

/// The built-in `schedule` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScheduleKind;

impl TriggerMatcher for ScheduleKind {
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool> {
        let (Some(spec), Source::Schedule(slot)) = (matched.spec.as_schedule(), &event.source)
        else {
            return Ok(false);
        };
        Ok(slot.slot_ref.app == spec.app && slot.slot_ref.schedule == spec.schedule.name)
    }
}

impl TriggerStarter for ScheduleKind {
    fn start_input(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<StartInput> {
        let Source::Schedule(slot) = &event.source else {
            return Err(Error::StartFailed {
                process: String::new(),
                node: String::new(),
                reason: "a schedule start needs a schedule slot".to_owned(),
            });
        };
        Ok(StartInput {
            version: matched.version,
            variables: json!({
                "app": slot.slot_ref.app,
                "schedule": slot.slot_ref.schedule,
                "scheduled_for": rfc3339(slot.scheduled_for),
                "deduplication_key": slot.deduplication_key,
            }),
            initiator: initiator(event),
        })
    }
}

/// The built-in `webhook` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct WebhookKind;

impl TriggerMatcher for WebhookKind {
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool> {
        let (Some(spec), Source::Webhook(call)) = (matched.spec.as_webhook(), &event.source) else {
            return Ok(false);
        };
        Ok(spec.path == call.path)
    }
}

impl TriggerStarter for WebhookKind {
    fn start_input(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<StartInput> {
        let Source::Webhook(call) = &event.source else {
            return Err(Error::StartFailed {
                process: String::new(),
                node: String::new(),
                reason: "a webhook start needs a webhook event".to_owned(),
            });
        };
        Ok(StartInput {
            version: matched.version,
            variables: call.payload.clone(),
            initiator: initiator(event),
        })
    }
}

/// The built-in `manual` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManualKind;

impl TriggerMatcher for ManualKind {
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool> {
        let (Some(_spec), Source::Manual(start)) = (matched.spec.as_manual(), &event.source) else {
            return Ok(false);
        };
        if start.process != matched.process {
            return Ok(false);
        }
        Ok(start.node.as_ref().is_none_or(|node| node == &matched.node))
    }
}

impl TriggerStarter for ManualKind {
    fn start_input(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<StartInput> {
        let Source::Manual(start) = &event.source else {
            return Err(Error::StartFailed {
                process: String::new(),
                node: String::new(),
                reason: "a manual start needs a manual event".to_owned(),
            });
        };
        check_input(matched, &start.input)?;
        Ok(StartInput {
            version: matched.version,
            variables: start.input.clone(),
            initiator: initiator(event),
        })
    }
}

/// The built-in `api` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApiKind;

impl TriggerMatcher for ApiKind {
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool> {
        let (Some(spec), Source::Api(start)) = (matched.spec.as_api(), &event.source) else {
            return Ok(false);
        };
        if spec.name != start.name {
            return Ok(false);
        }
        Ok(start
            .process
            .as_deref()
            .is_none_or(|process| process == matched.process))
    }
}

impl TriggerStarter for ApiKind {
    fn start_input(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<StartInput> {
        let Source::Api(start) = &event.source else {
            return Err(Error::StartFailed {
                process: String::new(),
                node: String::new(),
                reason: "an API start needs an API event".to_owned(),
            });
        };
        check_input(matched, &start.input)?;
        Ok(StartInput {
            version: matched.version,
            variables: start.input.clone(),
            initiator: initiator(event),
        })
    }
}

/// Refuses a start whose input does not satisfy the trigger's input schema, listing every problem.
fn check_input(matched: &TriggerMatch, input: &serde_json::Value) -> Result<()> {
    let Some(schema) = matched.spec.input_schema() else {
        return Ok(());
    };
    let problems = crate::input::validate_input(schema, input);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::InputInvalid {
            process: matched.process.clone(),
            node: matched.node.to_string(),
            problems,
        })
    }
}

/// The initiator user of a source event, or `None` for the system.
fn initiator(event: &SourceEvent) -> Option<String> {
    if event.principal.user.is_empty() {
        None
    } else {
        Some(event.principal.user.clone())
    }
}
