//! The form-script record and the events that trigger it.
//!
//! A form script is registered in an app manifest like every other server script (WP-EXT-06):
//! one TypeScript module per (DocType, event) with a name, a priority and a timeout. The
//! difference is where it runs: form scripts answer *form events* (`on_load`, `on_change`,
//! `on_save`, `on_action`) and return declarative UI effects plus a document patch, never a
//! transaction.

use spot_app_api::error::FieldPath;
use spot_app_api::{AppError, AppResult, ErrorClass};

use crate::codes;

/// The DocType a form script record lives in, when a site stores them as documents.
pub const FORM_SCRIPT_DOCTYPE: &str = "form_script";

/// The most a script's priority may add, matching the lifecycle tier's range.
pub const MAX_PRIORITY: i64 = 9_999;

/// One form event: what the user did, or what a script listens for.
///
/// The same type serves both sides so a registration and an occurrence compare directly.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FormEvent {
    /// The form opened.
    Load,
    /// A field changed. `field` is a field path (`field` or `table[idx].field`).
    Change {
        /// The changed field.
        field: String,
    },
    /// The form is about to be saved; validation messages are welcome.
    Save,
    /// A form action was chosen.
    Action {
        /// The action's id.
        action: String,
    },
}

impl FormEvent {
    /// The event's wire name (`load`, `change`, `save`, `action`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            FormEvent::Load => "load",
            FormEvent::Change { .. } => "change",
            FormEvent::Save => "save",
            FormEvent::Action { .. } => "action",
        }
    }

    /// The exported function the engine calls (`on_load`, `on_change`, `on_save`, `on_action`).
    #[must_use]
    pub fn entry(&self) -> &'static str {
        match self {
            FormEvent::Load => "on_load",
            FormEvent::Change { .. } => "on_change",
            FormEvent::Save => "on_save",
            FormEvent::Action { .. } => "on_action",
        }
    }

    /// The field this event is about, when it is about one.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        match self {
            FormEvent::Change { field } => Some(field),
            _ => None,
        }
    }

    /// The action this event is about, when it is about one.
    #[must_use]
    pub fn action(&self) -> Option<&str> {
        match self {
            FormEvent::Action { action } => Some(action),
            _ => None,
        }
    }

    /// A canonical key for the idempotency hash: the event and its target.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            FormEvent::Load => "load".to_owned(),
            FormEvent::Change { field } => format!("change:{field}"),
            FormEvent::Save => "save".to_owned(),
            FormEvent::Action { action } => format!("action:{action}"),
        }
    }

    /// Whether a script registered for `self` runs for the occurrence `event`.
    ///
    /// A `change` registration names a field path, a table name or a plain field: `qty` runs for
    /// a change to `items[2].qty`, and `items` runs for any row of the table. The match is
    /// deliberate and small; anything else is an exact path.
    #[must_use]
    pub fn runs_for(&self, event: &FormEvent) -> bool {
        match (self, event) {
            (FormEvent::Load, FormEvent::Load) => true,
            (FormEvent::Save, FormEvent::Save) => true,
            (FormEvent::Change { field: registered }, FormEvent::Change { field: changed }) => {
                change_matches(registered, changed)
            }
            (FormEvent::Action { action: registered }, FormEvent::Action { action: changed }) => {
                registered == changed
            }
            _ => false,
        }
    }
}

/// Whether a registration for `registered` covers a change to `changed`.
fn change_matches(registered: &str, changed: &str) -> bool {
    if registered == changed {
        return true;
    }
    // `qty` also covers `items[2].qty`: the same column in any row.
    if !registered.contains('[')
        && !registered.contains('.')
        && let Some((table, rest)) = changed.split_once('[')
    {
        if table == registered {
            return true;
        }
        if let Some((_, column)) = rest.split_once("].") {
            return column == registered;
        }
    }
    false
}

/// One registered form script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormScript {
    /// The script's name, unique within its DocType.
    pub name: String,
    /// The DocType whose form it serves (a storage name).
    pub doctype: String,
    /// The event it listens for.
    pub event: FormEvent,
    /// The TypeScript module source.
    pub source: String,
    /// Its order among the scripts of the same event: lower first, then by name.
    pub priority: i64,
    /// Its own timeout in milliseconds; 0 means the runner's default.
    pub timeout_ms: u64,
    /// Whether it runs.
    pub enabled: bool,
}

impl FormScript {
    /// A script with the runner's default timeout and priority 0.
    #[must_use]
    pub fn new(name: &str, doctype: &str, event: FormEvent, source: &str) -> FormScript {
        FormScript {
            name: name.to_owned(),
            doctype: doctype.to_owned(),
            event,
            source: source.to_owned(),
            priority: 0,
            timeout_ms: 0,
            enabled: true,
        }
    }

    /// The same script with this priority.
    #[must_use]
    pub fn with_priority(mut self, priority: i64) -> FormScript {
        self.priority = priority;
        self
    }

    /// The same script with this timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout_ms: u64) -> FormScript {
        self.timeout_ms = timeout_ms;
        self
    }

    /// The same script enabled or disabled.
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> FormScript {
        self.enabled = enabled;
        self
    }

    /// The entry point the engine calls.
    #[must_use]
    pub fn entry(&self) -> &'static str {
        self.event.entry()
    }

    /// Checks that the record can be registered.
    ///
    /// # Errors
    ///
    /// `form_effects.script_record_invalid`, one field error per problem: an empty name, a
    /// DocType that is not a storage name, an empty source, a priority out of range, or a
    /// `change`/`action` event with a malformed target.
    pub fn validate(&self) -> AppResult<()> {
        let mut error: Option<AppError> = None;
        let mut add = |field: &str, message: String| {
            error = Some(match error.take() {
                None => AppError::new(
                    ErrorClass::Validation,
                    codes::SCRIPT_RECORD_INVALID,
                    message.clone(),
                )
                .with_field(
                    FieldPath::field(field),
                    codes::SCRIPT_RECORD_INVALID,
                    message,
                ),
                Some(existing) => existing.with_field(
                    FieldPath::field(field),
                    codes::SCRIPT_RECORD_INVALID,
                    message,
                ),
            });
        };
        if self.name.trim().is_empty() {
            add("name", "A form script needs a name.".to_owned());
        }
        if !is_name(&self.doctype) {
            add(
                "doctype",
                format!("`{}` is not a DocType storage name.", self.doctype),
            );
        }
        if self.source.trim().is_empty() {
            add(
                "script",
                "A form script needs TypeScript source.".to_owned(),
            );
        }
        if !(0..=MAX_PRIORITY).contains(&self.priority) {
            add(
                "priority",
                format!("A priority is between 0 and {MAX_PRIORITY}."),
            );
        }
        match &self.event {
            FormEvent::Change { field } if !is_target(field) => add(
                "event",
                format!("`{field}` is not a field path a change can name."),
            ),
            FormEvent::Action { action } if !is_id(action) => {
                add("event", format!("`{action}` is not an action id."))
            }
            _ => {}
        }
        match error {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }
}

/// A set of form scripts, grouped and ordered for one DocType.
#[derive(Clone, Debug, Default)]
pub struct FormScriptSet {
    scripts: Vec<FormScript>,
}

impl FormScriptSet {
    /// Registers `scripts`, validating every record.
    ///
    /// # Errors
    ///
    /// The first record's `form_effects.script_record_invalid` error.
    pub fn new(scripts: impl IntoIterator<Item = FormScript>) -> AppResult<FormScriptSet> {
        let mut set = FormScriptSet::default();
        for script in scripts {
            set.insert(script)?;
        }
        Ok(set)
    }

    /// Adds one script.
    ///
    /// # Errors
    ///
    /// `form_effects.script_record_invalid` when the record is malformed.
    pub fn insert(&mut self, script: FormScript) -> AppResult<()> {
        script.validate()?;
        if self
            .scripts
            .iter()
            .any(|existing| existing.doctype == script.doctype && existing.name == script.name)
        {
            return Err(AppError::new(
                ErrorClass::Validation,
                codes::SCRIPT_RECORD_INVALID,
                format!(
                    "`{}` is already registered for `{}`.",
                    script.name, script.doctype
                ),
            )
            .with_field(
                FieldPath::field("name"),
                codes::SCRIPT_RECORD_INVALID,
                "The name is already registered for this DocType.",
            ));
        }
        self.scripts.push(script);
        Ok(())
    }

    /// Every registered script, in registration order.
    #[must_use]
    pub fn scripts(&self) -> &[FormScript] {
        &self.scripts
    }

    /// The enabled scripts that run for `event` on `doctype`, in deterministic order: priority,
    /// then name.
    #[must_use]
    pub fn matching(&self, doctype: &str, event: &FormEvent) -> Vec<&FormScript> {
        let mut matching: Vec<&FormScript> = self
            .scripts
            .iter()
            .filter(|script| script.enabled && script.doctype == doctype)
            .filter(|script| script.event.runs_for(event))
            .collect();
        matching.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.name.cmp(&b.name))
        });
        matching
    }

    /// How many scripts are registered (enabled or not).
    #[must_use]
    pub fn len(&self) -> usize {
        self.scripts.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }
}

/// A storage-name-like text: lowercase letters, digits and underscores, starting with a letter.
#[must_use]
pub fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// An id an action or a dialog may carry: lowercase letters, digits, and `_ - . :`, starting
/// with a letter, at most 64 characters.
#[must_use]
pub fn is_id(text: &str) -> bool {
    if text.len() > 64 {
        return false;
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.' | ':'))
}

/// A change target: a field path or a bare table name.
fn is_target(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    crate::path::TargetPath::parse(text).is_ok() || is_name(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn script(name: &str, event: FormEvent, priority: i64) -> FormScript {
        FormScript::new(name, "order", event, "export function on_load() {}")
            .with_priority(priority)
    }

    #[test]
    fn a_change_registration_covers_the_column_and_the_table() {
        let changed = FormEvent::Change {
            field: "items[2].qty".into(),
        };
        assert!(
            FormEvent::Change {
                field: "items[2].qty".into()
            }
            .runs_for(&changed)
        );
        assert!(
            FormEvent::Change {
                field: "qty".into()
            }
            .runs_for(&changed)
        );
        assert!(
            FormEvent::Change {
                field: "items".into()
            }
            .runs_for(&changed)
        );
        assert!(
            !FormEvent::Change {
                field: "price".into()
            }
            .runs_for(&changed)
        );
    }

    #[test]
    fn matching_scripts_are_ordered_by_priority_then_name() {
        let set = FormScriptSet::new([
            script("b", FormEvent::Save, 10),
            script("a", FormEvent::Save, 10),
            script("first", FormEvent::Save, 1),
            script("load", FormEvent::Load, 0),
        ])
        .unwrap();
        let names: Vec<&str> = set
            .matching("order", &FormEvent::Save)
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["first", "a", "b"]);
    }

    #[test]
    fn a_malformed_record_reports_every_problem() {
        let bad = FormScript::new(
            "",
            "Sales Order",
            FormEvent::Action {
                action: "Not An Id".into(),
            },
            "  ",
        );
        let error = bad.validate().unwrap_err();
        assert_eq!(error.code, codes::SCRIPT_RECORD_INVALID);
        assert_eq!(error.fields.len(), 4);
    }

    #[test]
    fn ids_are_small_and_predictable() {
        assert!(is_id("pick_customer"));
        assert!(is_id("sales:pick"));
        assert!(!is_id("Pick"));
        assert!(!is_id(""));
        assert!(!is_id("a b"));
        assert!(is_name("form_script"));
        assert!(!is_name("Form Script"));
    }
}
