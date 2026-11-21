//! Which schema family a topic belongs to, and how a payload is validated against it.

use spot_events_contract::lifecycle::Operation;
use spot_events_contract::{journal, state, system, system_platform};

/// The schema family of a topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// `doc_created` and its siblings.
    Lifecycle,
    /// The journal's `doc_state` image.
    State,
    /// A `sys_*` topic of `system.rs` (which also validates the platform and journal ones).
    System,
    /// A topic only `system_platform.rs` has a schema for.
    SystemPlatform,
    /// A topic only `journal.rs` has a schema for.
    Journal,
    /// No schema anywhere.
    None,
}

impl Family {
    /// The name used in the table.
    pub fn name(self) -> &'static str {
        match self {
            Family::Lifecycle => "lifecycle",
            Family::State => "state",
            Family::System => "system",
            Family::SystemPlatform => "system_platform",
            Family::Journal => "journal",
            Family::None => "none",
        }
    }
}

/// The family that holds the schema of `topic`.
pub fn family_of(topic: &str) -> Family {
    if Operation::ALL.iter().any(|o| o.topic_name() == topic) {
        Family::Lifecycle
    } else if topic == state::DOC_STATE_TOPIC {
        Family::State
    } else if system::schema_of(topic).is_some() {
        Family::System
    } else if system_platform::schema_of(topic).is_some() {
        Family::SystemPlatform
    } else if journal::schema_of(topic).is_some() {
        Family::Journal
    } else {
        Family::None
    }
}

/// Validates `data` against the schema of `topic`: `Ok(true)` valid, `Ok(false)` there is no
/// schema to validate against, `Err(paths)` the JSON paths that do not conform (never values).
///
/// # Errors
///
/// The paths of the fields that fail the schema.
pub fn validate(topic: &str, data: &serde_json::Value) -> Result<bool, Vec<String>> {
    let paths = |errors: Vec<spot_events_contract::SchemaError>| {
        errors.into_iter().map(|e| e.path).collect::<Vec<_>>()
    };
    match family_of(topic) {
        Family::Lifecycle => {
            let Some(op) = Operation::ALL.iter().find(|o| o.topic_name() == topic) else {
                return Ok(false);
            };
            op.schema().validate(data).map(|()| true).map_err(paths)
        }
        Family::State => state::validate(data).map(|()| true).map_err(paths),
        Family::System => system::validate(topic, data).map(|()| true).map_err(paths),
        Family::SystemPlatform => match system_platform::schema_of(topic) {
            Some(schema) => schema.validate(data).map(|()| true).map_err(paths),
            None => Ok(false),
        },
        Family::Journal => match journal::schema_of(topic) {
            Some(schema) => schema.validate(data).map(|()| true).map_err(paths),
            None => Ok(false),
        },
        Family::None => Ok(false),
    }
}

/// The registry owners whose emitted topics must have a schema.
pub const MUST_HAVE_SCHEMA_OWNERS: [&str; 3] = ["spot-document", "spot-jobs", "spot-doc-audit"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_state_topics_have_a_family() {
        assert_eq!(family_of("doc_created"), Family::Lifecycle);
        assert_eq!(family_of("doc_state"), Family::State);
        assert_eq!(family_of("not_a_topic_at_all"), Family::None);
    }

    #[test]
    fn a_lifecycle_payload_validates_and_a_wrong_one_names_the_path() {
        let ok = serde_json::json!({"name": "a"});
        assert_eq!(validate("doc_created", &ok), Ok(true));
        let wrong = serde_json::json!({"name": "a", "extra": 1});
        assert!(validate("doc_created", &wrong).is_err());
        assert_eq!(validate("nope", &ok), Ok(false));
    }
}
