//! The DAP variable view of a paused script: locals as typed values with expandable objects.
//!
//! A document handle returned by a host function is a JSON object of field values; the debugger
//! shows it as a typed object (`type: "object"`) whose fields are one `variables` request away,
//! exactly as a native debugger shows a struct. The store is built from one [`PausedSnapshot`];
//! every pause has a new generation, so a stale handle from the previous pause is refused rather
//! than answered with another frame's values.
//!
//! `evaluate` is **read-only**: it resolves a dotted path (`customer.credit`,
//! `items[0].qty`) against the paused frame's locals snapshot. It does not compile or run
//! JavaScript — the engine's `eval` is disabled for scripts, and a debugger must not reopen it.

use std::collections::BTreeMap;

use serde_json::Value;

use super::runtime::{Frame, PausedSnapshot};

/// The first reference of a frame's locals.
const FRAME_BASE: u64 = 1_000_000;
/// The first reference of a nested container.
const CHILD_BASE: u64 = 2_000_000;
/// The most children one container shows.
const MAX_CHILDREN: usize = 500;
/// The deepest value the store expands.
const MAX_DEPTH: usize = 8;

/// One DAP variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    /// The field or index name.
    pub name: String,
    /// The value rendered for the client.
    pub value: String,
    /// The JSON type name (`string`, `number`, `object`, `array`, `boolean`, `null`).
    pub type_name: String,
    /// Where to ask for this value's children, or 0.
    pub variables_reference: u64,
    /// The number of named children, when it has any.
    pub named_variables: Option<usize>,
    /// The number of indexed children, when it has any.
    pub indexed_variables: Option<usize>,
}

impl Variable {
    /// The variable as DAP JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut value = serde_json::json!({
            "name": self.name,
            "value": self.value,
            "type": self.type_name,
            "variablesReference": self.variables_reference,
        });
        if let Some(named) = self.named_variables {
            value["namedVariables"] = Value::from(named);
        }
        if let Some(indexed) = self.indexed_variables {
            value["indexedVariables"] = Value::from(indexed);
        }
        value
    }
}

/// The variables of one paused script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariableStore {
    generation: u64,
    frames: Vec<Frame>,
    references: BTreeMap<u64, Vec<Variable>>,
    next_reference: u64,
}

impl VariableStore {
    /// Builds the store for `snapshot`: one root reference per frame.
    #[must_use]
    pub fn build(snapshot: &PausedSnapshot) -> Self {
        let frames = snapshot.frames.clone();
        let mut store = Self {
            generation: snapshot.generation,
            frames: frames.clone(),
            references: BTreeMap::new(),
            next_reference: CHILD_BASE,
        };
        for frame in &frames {
            let reference = Self::frame_reference(frame.id);
            let children = store.children_of(&frame.locals, 0);
            store.references.insert(reference, children);
        }
        store
    }

    /// The pause generation this store belongs to.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The reference that answers with frame `frame_id`'s locals.
    #[must_use]
    pub fn frame_reference(frame_id: u32) -> u64 {
        FRAME_BASE + u64::from(frame_id)
    }

    /// The variables behind `reference`, if it belongs to this pause.
    #[must_use]
    pub fn variables(&self, reference: u64) -> Option<Vec<Variable>> {
        self.references.get(&reference).cloned()
    }

    /// Read-only `evaluate`: resolve `expression` against frame `frame_id`'s locals.
    ///
    /// Returns `None` for an expression that is not a plain path (a call, an assignment, a
    /// literal), which the client should show as unsupported rather than guess an answer.
    pub fn evaluate(&mut self, frame_id: u32, expression: &str) -> Option<Variable> {
        let locals = self
            .frames
            .iter()
            .find(|frame| frame.id == frame_id)?
            .locals
            .clone();
        let (name, value) = resolve(&locals, expression)?;
        let value = value.clone();
        Some(self.variable_for(&name, &value, 1))
    }

    fn variable_for(&mut self, name: &str, value: &Value, depth: usize) -> Variable {
        let type_name = type_name(value).to_owned();
        let rendered = render(value);
        let expandable = is_container(value) && depth < MAX_DEPTH && !is_empty_container(value);
        let variables_reference = if expandable {
            let reference = self.next_reference;
            self.next_reference = self.next_reference.saturating_add(1);
            let children = self.children_of(value, depth);
            self.references.insert(reference, children);
            reference
        } else {
            0
        };
        Variable {
            name: name.to_owned(),
            value: rendered,
            type_name,
            variables_reference,
            named_variables: match value {
                Value::Object(fields) if expandable => Some(fields.len()),
                _ => None,
            },
            indexed_variables: match value {
                Value::Array(items) if expandable => Some(items.len()),
                _ => None,
            },
        }
    }

    fn children_of(&mut self, value: &Value, depth: usize) -> Vec<Variable> {
        if depth >= MAX_DEPTH {
            return Vec::new();
        }
        match value {
            Value::Object(fields) => fields
                .iter()
                .take(MAX_CHILDREN)
                .map(|(name, value)| self.variable_for(name, value, depth + 1))
                .collect(),
            Value::Array(items) => items
                .iter()
                .take(MAX_CHILDREN)
                .enumerate()
                .map(|(index, value)| self.variable_for(&format!("[{index}]"), value, depth + 1))
                .collect(),
            _ => Vec::new(),
        }
    }
}

fn is_container(value: &Value) -> bool {
    matches!(value, Value::Object(_) | Value::Array(_))
}

fn is_empty_container(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "number",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => format!("{text:?}"),
        Value::Array(items) => format!("Array({})", items.len()),
        Value::Object(fields) => format!("Object({} field(s))", fields.len()),
    }
}

/// Resolves `expression` as a path into `root`, returning the last name and the value.
fn resolve<'a>(root: &'a Value, expression: &str) -> Option<(String, &'a Value)> {
    let expression = expression.trim();
    if expression.is_empty() || expression.contains(['(', ')', '=', ';', '+', '{', '}', '`']) {
        return None;
    }
    let mut rest = expression;
    let name = read_identifier(&mut rest)?;
    let mut value = root.get(&name)?;
    let mut last = name;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            rest = after;
            let name = read_identifier(&mut rest)?;
            value = value.get(&name)?;
            last = name;
        } else {
            let after = rest.strip_prefix('[')?;
            let close = after.find(']')?;
            let key = after[..close].trim();
            let key = key.trim_matches(['"', '\'']);
            rest = &after[close + 1..];
            value = match value {
                Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
                other => other.get(key)?,
            };
            last = key.to_owned();
        }
    }
    Some((last, value))
}

fn read_identifier(text: &mut &str) -> Option<String> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return None;
    }
    let mut end = first.len_utf8();
    for (index, c) in chars {
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            end = index + c.len_utf8();
        } else {
            break;
        }
    }
    let name = text[..end].to_owned();
    *text = &text[end..];
    Some(name)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::VariableStore;
    use crate::debug::runtime::{Frame, PausedSnapshot, StopReason};

    fn snapshot() -> PausedSnapshot {
        PausedSnapshot {
            reason: StopReason::Breakpoint,
            generation: 7,
            frames: vec![Frame {
                id: 0,
                name: "on_validate".into(),
                source: "scripts/on_validate.ts".into(),
                line: 3,
                column: 5,
                locals: json!({
                    "doc": {"total": 120, "customer": "c-1"},
                    "customer": {"name": "Acme", "credit": 100, "tags": ["vip"]},
                    "items": [{"qty": 2}, {"qty": 3}],
                }),
            }],
        }
    }

    #[test]
    fn a_document_handle_is_a_typed_object_with_fields() {
        let store = VariableStore::build(&snapshot());
        let root = VariableStore::frame_reference(0);
        let variables = store.variables(root).expect("frame locals");
        let customer = variables
            .iter()
            .find(|variable| variable.name == "customer")
            .expect("customer");
        assert_eq!(customer.type_name, "object");
        assert!(customer.variables_reference > 0);
        let fields = store
            .variables(customer.variables_reference)
            .expect("fields");
        assert!(fields.iter().any(|field| field.name == "name"));
        let credit = fields
            .iter()
            .find(|field| field.name == "credit")
            .expect("credit");
        assert_eq!(credit.value, "100");
        assert_eq!(credit.type_name, "number");
    }

    #[test]
    fn arrays_expand_by_index_and_are_bounded() {
        let store = VariableStore::build(&snapshot());
        let root = store
            .variables(VariableStore::frame_reference(0))
            .expect("locals");
        let items = root
            .iter()
            .find(|variable| variable.name == "items")
            .expect("items");
        assert_eq!(items.type_name, "array");
        assert_eq!(items.indexed_variables, Some(2));
        let children = store.variables(items.variables_reference).expect("items");
        assert_eq!(children[0].name, "[0]");
    }

    #[test]
    fn evaluate_resolves_a_read_only_path() {
        let mut store = VariableStore::build(&snapshot());
        let customer = store.evaluate(0, "customer").expect("customer");
        assert_eq!(customer.type_name, "object");
        assert_eq!(
            store.evaluate(0, "customer.name").expect("name").value,
            "\"Acme\""
        );
        assert_eq!(store.evaluate(0, "items[1].qty").expect("qty").value, "3");
        assert_eq!(
            store.evaluate(0, "doc[\"total\"]").expect("total").value,
            "120"
        );
        assert!(store.evaluate(0, "customer.name = 1").is_none());
        assert!(store.evaluate(0, "host.get_doc()").is_none());
        assert!(store.evaluate(0, "missing").is_none());
        assert!(store.evaluate(9, "customer").is_none());
    }

    #[test]
    fn a_stale_reference_is_not_answered() {
        let store = VariableStore::build(&snapshot());
        assert!(store.variables(999_999).is_none());
        assert_eq!(store.generation(), 7);
    }
}
