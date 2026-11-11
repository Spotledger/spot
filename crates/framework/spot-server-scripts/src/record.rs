//! The `Server Script` record and the event vocabulary.

use spot_app_api::error::FieldPath;
use spot_app_api::lifecycle::{HookPoint, Phase, Target};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{AppError, AppResult, Document, ErrorClass, Operation};

use crate::codes;

/// The storage field holding the script's name.
pub const FIELD_NAME: &str = "name";
/// The storage field holding whether the script runs.
pub const FIELD_ENABLED: &str = "enabled";
/// The storage field holding the DocType the script listens to.
pub const FIELD_DOCTYPE: &str = "doctype";
/// The storage field holding the event name.
pub const FIELD_EVENT: &str = "event";
/// The storage field holding the TypeScript source.
pub const FIELD_SCRIPT: &str = "script";
/// The storage field holding the compiled source hash (read-only).
pub const FIELD_COMPILED_HASH: &str = "compiled_hash";
/// The storage field holding the last failure, for the admin listing (read-only).
pub const FIELD_LAST_ERROR: &str = "last_error";
/// The storage field holding the per-script timeout in milliseconds.
pub const FIELD_TIMEOUT_MS: &str = "timeout_ms";
/// The storage field holding how many times the script has run (read-only).
pub const FIELD_LAST_RUN_COUNT: &str = "last_run_count";
/// The storage field holding the ordering priority within an event.
pub const FIELD_PRIORITY: &str = "priority";
/// The storage field holding why the script was disabled (read-only).
pub const FIELD_DISABLED_REASON: &str = "disabled_reason";

/// A document event a Server Script may listen to. The names are the DocType's `event` select
/// options and the Frappe-familiar names the brief fixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScriptEvent {
    /// Derive the document before it is validated (ADR-51 `before_save`, may change it).
    BeforeValidate,
    /// Validate the document (ADR-51 `validate_save`, read-only).
    Validate,
    /// Derive the document just before the write (ADR-51 `before_save`, may change it).
    BeforeSave,
    /// Act after the write, inside the transaction (ADR-51 `after_save`).
    AfterSave,
    /// Derive a document before it is posted (ADR-51 `before_post`, may change it).
    BeforeSubmit,
    /// Act after a document is posted (ADR-51 `after_post`).
    OnSubmit,
    /// Validate a document before it is cancelled (ADR-51 `validate_cancel`, read-only).
    BeforeCancel,
    /// Act after a document is cancelled (ADR-51 `after_cancel`).
    OnCancel,
    /// Notify after the save commits (ADR-51 `save_committed`).
    AfterCommit,
}

impl ScriptEvent {
    /// Every event, in the order the DocType lists them.
    pub const ALL: [ScriptEvent; 9] = [
        ScriptEvent::BeforeValidate,
        ScriptEvent::Validate,
        ScriptEvent::BeforeSave,
        ScriptEvent::AfterSave,
        ScriptEvent::BeforeSubmit,
        ScriptEvent::OnSubmit,
        ScriptEvent::BeforeCancel,
        ScriptEvent::OnCancel,
        ScriptEvent::AfterCommit,
    ];

    /// The event's name, as stored and as the DocType's select option.
    pub fn as_str(self) -> &'static str {
        match self {
            ScriptEvent::BeforeValidate => "before_validate",
            ScriptEvent::Validate => "validate",
            ScriptEvent::BeforeSave => "before_save",
            ScriptEvent::AfterSave => "after_save",
            ScriptEvent::BeforeSubmit => "before_submit",
            ScriptEvent::OnSubmit => "on_submit",
            ScriptEvent::BeforeCancel => "before_cancel",
            ScriptEvent::OnCancel => "on_cancel",
            ScriptEvent::AfterCommit => "after_commit",
        }
    }

    /// Reads an event from its stored name.
    pub fn parse(text: &str) -> Option<ScriptEvent> {
        ScriptEvent::ALL.into_iter().find(|e| e.as_str() == text)
    }

    /// The exported function the engine calls: `on_<event>`. The engine passes one JSON
    /// argument, so the entry receives `{ doc, ctx }`.
    pub fn entry(self) -> &'static str {
        match self {
            ScriptEvent::BeforeValidate => "on_before_validate",
            ScriptEvent::Validate => "on_validate",
            ScriptEvent::BeforeSave => "on_before_save",
            ScriptEvent::AfterSave => "on_after_save",
            ScriptEvent::BeforeSubmit => "on_before_submit",
            ScriptEvent::OnSubmit => "on_submit",
            ScriptEvent::BeforeCancel => "on_before_cancel",
            ScriptEvent::OnCancel => "on_cancel",
            ScriptEvent::AfterCommit => "on_after_commit",
        }
    }

    /// The ADR-51 hook point the event runs at. `before_validate` and `before_save` share the
    /// derive phase but are ordered apart by their base priority; `before_cancel` is the cancel
    /// phase's check, per the blueprint's Frappe mapping.
    pub fn hook_point(self) -> HookPoint {
        match self {
            ScriptEvent::BeforeValidate | ScriptEvent::BeforeSave => {
                HookPoint::of(Phase::Before, Target::Save)
            }
            ScriptEvent::Validate => HookPoint::of(Phase::Validate, Target::Save),
            ScriptEvent::AfterSave => HookPoint::of(Phase::After, Target::Save),
            ScriptEvent::BeforeSubmit => HookPoint::of(Phase::Before, Target::Op(Operation::Post)),
            ScriptEvent::OnSubmit => HookPoint::of(Phase::After, Target::Op(Operation::Post)),
            ScriptEvent::BeforeCancel => {
                HookPoint::of(Phase::Validate, Target::Op(Operation::Cancel))
            }
            ScriptEvent::OnCancel => HookPoint::of(Phase::After, Target::Op(Operation::Cancel)),
            ScriptEvent::AfterCommit => HookPoint::of(Phase::Committed, Target::Save),
        }
    }

    /// The phase the event runs in.
    pub fn phase(self) -> Phase {
        self.hook_point().phase()
    }

    /// The base step priority: events that share a hook point are ordered apart by it, before
    /// the script's own priority. A multiple of 10,000 so a script priority (0 to 9,999) can
    /// never jump the event order.
    pub fn base_priority(self) -> i32 {
        let rank = match self {
            ScriptEvent::BeforeValidate => 0,
            ScriptEvent::Validate => 1,
            ScriptEvent::BeforeSave => 2,
            ScriptEvent::AfterSave => 3,
            ScriptEvent::BeforeSubmit => 4,
            ScriptEvent::OnSubmit => 5,
            ScriptEvent::BeforeCancel => 6,
            ScriptEvent::OnCancel => 7,
            ScriptEvent::AfterCommit => 8,
        };
        rank * 10_000
    }

    /// Whether a script of this event may change its own document (ADR-51: only `before_*`).
    pub fn may_modify_own(self) -> bool {
        self.phase().rules().modify_own
    }
}

/// The most a script priority may add to an event's base priority.
pub const MAX_PRIORITY: i64 = 9_999;

/// One enabled Server Script, read from its document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerScript {
    /// The record id, when the script came from a stored document.
    pub record_id: Option<RecordId>,
    /// The script's name (the document's `name`).
    pub name: String,
    /// The DocType the script listens to (its storage name).
    pub doctype: String,
    /// The event the script listens to.
    pub event: ScriptEvent,
    /// The TypeScript source.
    pub source: String,
    /// The compiled source hash, when the script has been saved once.
    pub compiled_hash: Option<String>,
    /// The per-script timeout in milliseconds.
    pub timeout_ms: u64,
    /// The ordering priority within the event (0 to [`MAX_PRIORITY`]).
    pub priority: i64,
    /// Whether the script runs.
    pub enabled: bool,
    /// Why the script was disabled, when it was.
    pub disabled_reason: Option<String>,
}

impl ServerScript {
    /// Reads a script from a `Server Script` document.
    ///
    /// # Errors
    ///
    /// `serverscript.record_invalid` (as field errors) when the record is missing its name,
    /// DocType, event or source, or when the DocType is not a storage name.
    pub fn from_document(doc: &Document) -> AppResult<ServerScript> {
        let name = text(doc, FIELD_NAME).unwrap_or_default().to_owned();
        let doctype = text(doc, FIELD_DOCTYPE).unwrap_or_default().to_owned();
        let event_text = text(doc, FIELD_EVENT).unwrap_or_default().to_owned();
        let source = text(doc, FIELD_SCRIPT).unwrap_or_default().to_owned();

        let mut problems: Vec<(&str, String)> = Vec::new();
        if name.is_empty() {
            problems.push((FIELD_NAME, "A Server Script needs a name.".to_owned()));
        }
        if Name::new(&doctype).is_err() {
            problems.push((
                FIELD_DOCTYPE,
                format!("`{doctype}` is not a DocType storage name."),
            ));
        }
        let event = match ScriptEvent::parse(&event_text) {
            Some(event) => Some(event),
            None => {
                problems.push((
                    FIELD_EVENT,
                    format!("`{event_text}` is not one of the document events."),
                ));
                None
            }
        };
        if source.trim().is_empty() {
            problems.push((
                FIELD_SCRIPT,
                "A Server Script needs TypeScript source.".to_owned(),
            ));
        }
        if let Some((field, message)) = problems.first() {
            let mut error = AppError::field(
                FieldPath::field(field),
                codes::RECORD_INVALID,
                message.clone(),
            );
            for (field, message) in problems.iter().skip(1) {
                error = error.with_field(
                    FieldPath::field(field),
                    codes::RECORD_INVALID,
                    message.clone(),
                );
            }
            return Err(error);
        }
        let event = event.ok_or_else(|| {
            AppError::new(
                ErrorClass::Internal,
                codes::RECORD_INVALID,
                "the event was checked above",
            )
        })?;

        Ok(ServerScript {
            record_id: doc.version().map(|_| doc.id()),
            name,
            doctype,
            event,
            source,
            compiled_hash: text(doc, FIELD_COMPILED_HASH).map(str::to_owned),
            timeout_ms: int(doc, FIELD_TIMEOUT_MS).unwrap_or(200).max(0) as u64,
            priority: int(doc, FIELD_PRIORITY).unwrap_or(0).clamp(0, MAX_PRIORITY),
            enabled: matches!(doc.get(FIELD_ENABLED), Value::Bool(true)),
            disabled_reason: text(doc, FIELD_DISABLED_REASON).map(str::to_owned),
        })
    }

    /// The step's priority: the event's base plus the script's own, so events sharing a hook
    /// point stay in event order and scripts within an event order by priority, then name.
    pub fn step_priority(&self) -> i32 {
        self.event
            .base_priority()
            .saturating_add(i32::try_from(self.priority).unwrap_or(0))
    }
}

fn text<'a>(doc: &'a Document, field: &str) -> Option<&'a str> {
    match doc.get(field) {
        Value::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

fn int(doc: &Document, field: &str) -> Option<i64> {
    match doc.get(field) {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_app_api::prelude::Version;
    use std::collections::BTreeMap;

    fn doc_with(fields: &[(&str, Value)]) -> Document {
        let map: BTreeMap<Name, Value> = fields
            .iter()
            .map(|(k, v)| (Name::new(k).expect("a name"), v.clone()))
            .collect();
        Document::loaded(
            Name::new("server_script").expect("a name"),
            RecordId::new_v7(),
            Version::FIRST,
            None,
            map,
            BTreeMap::new(),
        )
    }

    #[test]
    fn a_record_reads_back() {
        let doc = doc_with(&[
            (FIELD_NAME, Value::Text("check_qty".into())),
            (FIELD_DOCTYPE, Value::Text("sales_invoice".into())),
            (FIELD_EVENT, Value::Text("validate".into())),
            (
                FIELD_SCRIPT,
                Value::Text("export function on_validate() {}".into()),
            ),
            (FIELD_ENABLED, Value::Bool(true)),
            (FIELD_TIMEOUT_MS, Value::Int(500)),
            (FIELD_PRIORITY, Value::Int(20)),
        ]);
        let script = ServerScript::from_document(&doc).expect("a valid record");
        assert_eq!(script.event, ScriptEvent::Validate);
        assert_eq!(script.timeout_ms, 500);
        assert_eq!(script.priority, 20);
        assert!(script.enabled);
        assert_eq!(script.event.entry(), "on_validate");
        assert!(!script.event.may_modify_own());
    }

    #[test]
    fn a_broken_record_reports_every_problem() {
        let doc = doc_with(&[
            (FIELD_NAME, Value::Text(String::new())),
            (FIELD_DOCTYPE, Value::Text("Sales Invoice".into())),
            (FIELD_EVENT, Value::Text("nope".into())),
            (FIELD_SCRIPT, Value::Text("  ".into())),
        ]);
        let error = ServerScript::from_document(&doc).expect_err("invalid");
        assert_eq!(error.fields.len(), 4);
        assert_eq!(error.fields[0].code, codes::RECORD_INVALID);
    }

    #[test]
    fn events_are_ordered_by_base_priority() {
        let before_validate = ScriptEvent::BeforeValidate.base_priority();
        let before_save = ScriptEvent::BeforeSave.base_priority();
        assert!(before_validate < before_save);
        assert_eq!(ScriptEvent::Validate.hook_point().name(), "validate_save");
        assert_eq!(
            ScriptEvent::BeforeCancel.hook_point().name(),
            "validate_cancel"
        );
        assert_eq!(
            ScriptEvent::AfterCommit.hook_point().name(),
            "save_committed"
        );
    }
}
