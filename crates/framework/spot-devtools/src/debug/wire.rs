//! What reaches the debugger client is redacted first.
//!
//! Script output, the script's result or error, warnings, and variable and `evaluate` values all
//! pass through [`spot_redact::Redactor`] before they are framed. A hidden field is redacted by
//! its name (the redactor's field names), a sealed or otherwise registered secret by its known
//! forms, and anything shaped like a credential by the pattern rules. With no redactor the text is
//! withheld, never sent raw.

use std::sync::Arc;

use serde_json::{Map, Value};
use spot_redact::Redactor;

/// The text a client sees when there is no redactor to vouch for the real text.
pub const WITHHELD: &str = "[withheld: no redactor]";

/// The redactor in front of the wire.
#[derive(Clone)]
pub struct WireRedactor(Option<Arc<Redactor>>);

impl WireRedactor {
    /// Redacts through `redactor`; `None` withholds everything.
    #[must_use]
    pub fn new(redactor: Option<Arc<Redactor>>) -> Self {
        Self(redactor)
    }

    /// One piece of free text.
    #[must_use]
    pub fn text(&self, text: &str) -> String {
        match &self.0 {
            Some(redactor) => redactor.redact_str(text).into_owned(),
            None => WITHHELD.to_owned(),
        }
    }

    /// The value of a variable called `name`: the value is redacted outright when the name is a
    /// hidden field name, else as text.
    #[must_use]
    pub fn named(&self, name: &str, value: &str) -> String {
        let Some(redactor) = &self.0 else {
            return WITHHELD.to_owned();
        };
        let mut single = Map::new();
        single.insert(name.to_owned(), Value::String(value.to_owned()));
        let mut wrapped = Value::Object(single);
        redactor.redact_json(&mut wrapped);
        wrapped
            .get(name)
            .and_then(Value::as_str)
            .map_or_else(|| WITHHELD.to_owned(), str::to_owned)
    }

    /// A script's output: JSON text is redacted field by field, anything else as text.
    #[must_use]
    pub fn output(&self, output: &str) -> String {
        let Some(redactor) = &self.0 else {
            return WITHHELD.to_owned();
        };
        match serde_json::from_str::<Value>(output) {
            Ok(mut json) if json.is_object() || json.is_array() => {
                redactor.redact_json(&mut json);
                json.to_string()
            }
            _ => redactor.redact_str(output).into_owned(),
        }
    }
}
