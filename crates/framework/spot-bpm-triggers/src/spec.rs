//! [`TriggerSpec`]: one trigger node's config, and how a model [`TriggerConfig`] becomes one.
//!
//! The model's trigger node carries the fields a definition states (`doctype`, `event`, `cron`,
//! `path`); a spec carries the same plus the parts the model does not yet express (a document
//! condition, a webhook secret reference, manual roles, an API input schema). Those parts default
//! to "none" when a spec is read from a version, and a host that needs them supplies them through
//! [`TriggerIndex::add_spec`](crate::TriggerIndex::add_spec).

use spot_app_api::StepName;
use spot_bpm_model::{NodeId, TriggerConfig, TriggerType};
use spot_manifest::{Cron, Queue, SCHEDULE_BUDGET_DEFAULT_MS};
use spot_scheduler::{MissedRunPolicy, Schedule, ScheduleBuilder, TimeZone};
use spot_webhooks::EventType;

use crate::error::{Error, Result};
use crate::{KIND_API, KIND_DOCUMENT_EVENT, KIND_MANUAL, KIND_SCHEDULE, KIND_WEBHOOK};

/// The IANA zone a schedule trigger fires in until the model can name one.
pub const DEFAULT_SCHEDULE_ZONE: &str = "UTC";

/// The lifecycle event a document-event trigger listens for, plus its condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentEventSpec {
    /// The subject DocType (its storage name).
    pub doctype: String,
    /// The lifecycle operation, e.g. `create`, `post`, `cancel`.
    pub event: EventType,
    /// A boolean condition in the shared expression language, over the after-image `doc` root.
    pub condition: Option<String>,
}

/// A schedule trigger: the scheduler's own [`Schedule`] and missed-run policy.
#[derive(Debug, Clone)]
pub struct ScheduleSpec {
    /// The app the schedule belongs to (the slot's deduplication-key prefix).
    pub app: String,
    /// When it fires and what it runs.
    pub schedule: Schedule,
    /// What to do with the runs missed while nobody ticked.
    pub policy: MissedRunPolicy,
}

/// An incoming-webhook trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookSpec {
    /// The path the webhook arrives on.
    pub path: String,
    /// A reference to the sealed secret used to verify the signature, never the secret itself.
    pub secret_ref: Option<String>,
}

/// A manual start from the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualSpec {
    /// The roles a person must hold to start it; empty means any signed-in person.
    pub roles: Vec<String>,
    /// The JSON schema of the start input, when the definition states one.
    pub input_schema: Option<serde_json::Value>,
}

/// An API start.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiSpec {
    /// The API name that starts the process.
    pub name: String,
    /// The roles the caller must hold; empty means any signed-in caller.
    pub roles: Vec<String>,
    /// The JSON schema of the request body, when the definition states one.
    pub input_schema: Option<serde_json::Value>,
}

/// A trigger kind a later work package registered (ADR-080), e.g. `message_arrived`.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomSpec {
    /// The kind token the registry knows it by.
    pub kind: String,
    /// The kind's own config, opaque to this crate.
    pub config: serde_json::Value,
}

/// One trigger node's spec.
#[derive(Debug, Clone)]
pub enum TriggerSpec {
    /// A document lifecycle event from the outbox.
    DocumentEvent(DocumentEventSpec),
    /// A schedule.
    Schedule(ScheduleSpec),
    /// An incoming webhook.
    Webhook(WebhookSpec),
    /// A person starts it from the app.
    Manual(ManualSpec),
    /// An API call starts it.
    Api(ApiSpec),
    /// A kind registered by a later work package.
    Custom(CustomSpec),
}

impl TriggerSpec {
    /// The kind token the registry knows this spec by.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::DocumentEvent(_) => KIND_DOCUMENT_EVENT,
            Self::Schedule(_) => KIND_SCHEDULE,
            Self::Webhook(_) => KIND_WEBHOOK,
            Self::Manual(_) => KIND_MANUAL,
            Self::Api(_) => KIND_API,
            Self::Custom(custom) => &custom.kind,
        }
    }

    /// The document-event spec, when this is one.
    #[must_use]
    pub fn as_document_event(&self) -> Option<&DocumentEventSpec> {
        match self {
            Self::DocumentEvent(spec) => Some(spec),
            _ => None,
        }
    }

    /// The schedule spec, when this is one.
    #[must_use]
    pub fn as_schedule(&self) -> Option<&ScheduleSpec> {
        match self {
            Self::Schedule(spec) => Some(spec),
            _ => None,
        }
    }

    /// The webhook spec, when this is one.
    #[must_use]
    pub fn as_webhook(&self) -> Option<&WebhookSpec> {
        match self {
            Self::Webhook(spec) => Some(spec),
            _ => None,
        }
    }

    /// The manual spec, when this is one.
    #[must_use]
    pub fn as_manual(&self) -> Option<&ManualSpec> {
        match self {
            Self::Manual(spec) => Some(spec),
            _ => None,
        }
    }

    /// The API spec, when this is one.
    #[must_use]
    pub fn as_api(&self) -> Option<&ApiSpec> {
        match self {
            Self::Api(spec) => Some(spec),
            _ => None,
        }
    }

    /// The roles a manual or API start requires; empty for every other kind.
    #[must_use]
    pub fn roles(&self) -> &[String] {
        match self {
            Self::Manual(spec) => &spec.roles,
            Self::Api(spec) => &spec.roles,
            _ => &[],
        }
    }

    /// The input schema a manual or API start must satisfy, when the spec declares one.
    #[must_use]
    pub fn input_schema(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Manual(spec) => spec.input_schema.as_ref(),
            Self::Api(spec) => spec.input_schema.as_ref(),
            _ => None,
        }
    }

    /// Builds the spec of a trigger node from the model's config.
    ///
    /// The parts the model does not carry default to "none": a document condition, a webhook
    /// secret reference, manual roles and an API input schema.
    ///
    /// # Errors
    ///
    /// [`Error::TriggerIncomplete`] when a required field is missing, [`Error::UnknownOperation`]
    /// for a document-event operation the lifecycle does not have, and the schedule errors from
    /// [`Error::InvalidCron`] / [`Error::ScheduleInvalid`].
    pub fn from_config(
        process: &str,
        app: &str,
        node: &NodeId,
        config: &TriggerConfig,
    ) -> Result<Self> {
        match config.trigger {
            TriggerType::DocumentEvent => {
                let doctype = config.doctype.clone().ok_or_else(|| {
                    incomplete(process, node, "a document-event trigger names no DocType")
                })?;
                let event_text = config.event.as_deref().ok_or_else(|| {
                    incomplete(process, node, "a document-event trigger names no operation")
                })?;
                let event = parse_operation(event_text).ok_or_else(|| Error::UnknownOperation {
                    node: node.to_string(),
                    operation: event_text.to_owned(),
                })?;
                Ok(Self::DocumentEvent(DocumentEventSpec {
                    doctype,
                    event,
                    condition: None,
                }))
            }
            TriggerType::Schedule => {
                let cron_text = config.cron.as_deref().ok_or_else(|| {
                    incomplete(process, node, "a schedule trigger names no cron expression")
                })?;
                let schedule = build_schedule(app, node, cron_text)?;
                Ok(Self::Schedule(ScheduleSpec {
                    app: app.to_owned(),
                    schedule,
                    policy: MissedRunPolicy::RunOnce,
                }))
            }
            TriggerType::Webhook => {
                let path = config
                    .path
                    .clone()
                    .ok_or_else(|| incomplete(process, node, "a webhook trigger names no path"))?;
                Ok(Self::Webhook(WebhookSpec {
                    path,
                    secret_ref: None,
                }))
            }
            TriggerType::Manual => Ok(Self::Manual(ManualSpec {
                roles: Vec::new(),
                input_schema: None,
            })),
            TriggerType::Api => {
                let name = config
                    .path
                    .clone()
                    .ok_or_else(|| incomplete(process, node, "an API trigger names no name"))?;
                Ok(Self::Api(ApiSpec {
                    name,
                    roles: Vec::new(),
                    input_schema: None,
                }))
            }
        }
    }
}

/// Builds the scheduler [`Schedule`] a schedule trigger node names.
///
/// The schedule's name is the node id, its step is `<app>:<node>` (the app's step that runs),
/// its zone is [`DEFAULT_SCHEDULE_ZONE`] and its budget is the manifest default.
///
/// # Errors
///
/// [`Error::InvalidCron`] when the cron text does not parse, [`Error::ScheduleInvalid`] when the
/// step name or the schedule itself does not build.
fn build_schedule(app: &str, node: &NodeId, cron_text: &str) -> Result<Schedule> {
    let cron = Cron::parse(cron_text).map_err(|e| Error::InvalidCron {
        node: node.to_string(),
        detail: e.to_string(),
    })?;
    let step = StepName::new(&format!("{app}:{node}")).map_err(|e| Error::ScheduleInvalid {
        node: node.to_string(),
        detail: e.to_string(),
    })?;
    let zone = TimeZone::parse(DEFAULT_SCHEDULE_ZONE).map_err(|e| Error::ScheduleInvalid {
        node: node.to_string(),
        detail: e.to_string(),
    })?;
    ScheduleBuilder::new(node.as_str())
        .cron(cron)
        .step(step)
        .queue(Queue::Default)
        .budget(std::time::Duration::from_millis(SCHEDULE_BUDGET_DEFAULT_MS))
        .zone(zone)
        .build()
        .map_err(|e| Error::ScheduleInvalid {
            node: node.to_string(),
            detail: e.to_string(),
        })
}

/// The lifecycle [`EventType`] an operation name names, or `None` for an unknown operation.
///
/// The names are the model's `KNOWN_OPERATIONS` (`create`, `update`, `post`, `cancel`, `delete`,
/// `rename`, `update_posted`), the same seven the lifecycle topics carry.
#[must_use]
pub fn parse_operation(text: &str) -> Option<EventType> {
    match text {
        "create" => Some(EventType::Create),
        "update" => Some(EventType::Update),
        "delete" => Some(EventType::Delete),
        "post" => Some(EventType::Post),
        "cancel" => Some(EventType::Cancel),
        "rename" => Some(EventType::Rename),
        "update_posted" => Some(EventType::UpdatedPosted),
        _ => None,
    }
}

/// An incomplete-trigger error.
fn incomplete(process: &str, node: &NodeId, detail: &str) -> Error {
    Error::TriggerIncomplete {
        process: process.to_owned(),
        node: node.to_string(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn node(id: &str) -> NodeId {
        NodeId::new(id).unwrap()
    }

    #[test]
    fn document_event_spec_reads_doctype_and_operation() {
        let config = TriggerConfig {
            trigger: TriggerType::DocumentEvent,
            doctype: Some("sales_order".into()),
            event: Some("post".into()),
            ..TriggerConfig::default()
        };
        let spec =
            TriggerSpec::from_config("order_to_cash", "erp_selling", &node("start"), &config)
                .unwrap();
        let document = spec.as_document_event().unwrap();
        assert_eq!(document.doctype, "sales_order");
        assert_eq!(document.event, EventType::Post);
        assert_eq!(document.condition, None);
        assert_eq!(spec.kind(), KIND_DOCUMENT_EVENT);
    }

    #[test]
    fn an_unknown_operation_is_rejected() {
        let config = TriggerConfig {
            trigger: TriggerType::DocumentEvent,
            doctype: Some("sales_order".into()),
            event: Some("submit".into()),
            ..TriggerConfig::default()
        };
        let err = TriggerSpec::from_config("p", "app", &node("start"), &config).unwrap_err();
        assert_eq!(err.code(), crate::error::TriggerErrorCode::UnknownOperation);
    }

    #[test]
    fn a_missing_doctype_is_incomplete() {
        let config = TriggerConfig {
            trigger: TriggerType::DocumentEvent,
            event: Some("create".into()),
            ..TriggerConfig::default()
        };
        let err = TriggerSpec::from_config("p", "app", &node("start"), &config).unwrap_err();
        assert_eq!(
            err.code(),
            crate::error::TriggerErrorCode::TriggerIncomplete
        );
    }

    #[test]
    fn a_schedule_builds_a_scheduler_schedule() {
        let config = TriggerConfig {
            trigger: TriggerType::Schedule,
            cron: Some("0 0 9 * * *".into()),
            ..TriggerConfig::default()
        };
        let spec = TriggerSpec::from_config("p", "app", &node("daily"), &config).unwrap();
        let schedule = spec.as_schedule().unwrap();
        assert_eq!(schedule.app, "app");
        assert_eq!(schedule.schedule.name, "daily");
        assert_eq!(schedule.policy, MissedRunPolicy::RunOnce);
    }

    #[test]
    fn an_invalid_cron_is_rejected() {
        let config = TriggerConfig {
            trigger: TriggerType::Schedule,
            cron: Some("not a cron".into()),
            ..TriggerConfig::default()
        };
        let err = TriggerSpec::from_config("p", "app", &node("daily"), &config).unwrap_err();
        assert_eq!(err.code(), crate::error::TriggerErrorCode::InvalidCron);
    }

    #[test]
    fn webhook_manual_and_api_specs_read_their_fields() {
        let webhook = TriggerSpec::from_config(
            "p",
            "app",
            &node("hook"),
            &TriggerConfig {
                trigger: TriggerType::Webhook,
                path: Some("/hooks/intake".into()),
                ..TriggerConfig::default()
            },
        )
        .unwrap();
        assert_eq!(webhook.as_webhook().unwrap().path, "/hooks/intake");
        assert_eq!(webhook.as_webhook().unwrap().secret_ref, None);

        let manual = TriggerSpec::from_config(
            "p",
            "app",
            &node("start"),
            &TriggerConfig {
                trigger: TriggerType::Manual,
                ..TriggerConfig::default()
            },
        )
        .unwrap();
        assert!(manual.as_manual().unwrap().roles.is_empty());
        assert!(manual.roles().is_empty());

        let api = TriggerSpec::from_config(
            "p",
            "app",
            &node("api"),
            &TriggerConfig {
                trigger: TriggerType::Api,
                path: Some("start_order".into()),
                ..TriggerConfig::default()
            },
        )
        .unwrap();
        assert_eq!(api.as_api().unwrap().name, "start_order");
    }

    #[test]
    fn parse_operation_covers_every_lifecycle_operation() {
        for (text, event) in [
            ("create", EventType::Create),
            ("update", EventType::Update),
            ("delete", EventType::Delete),
            ("post", EventType::Post),
            ("cancel", EventType::Cancel),
            ("rename", EventType::Rename),
            ("update_posted", EventType::UpdatedPosted),
        ] {
            assert_eq!(parse_operation(text), Some(event), "{text}");
        }
        assert_eq!(parse_operation("submit"), None);
    }
}
