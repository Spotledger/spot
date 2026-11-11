//! The host bridge: the only way a script touches the world, and the closed allow-list in front
//! of it.

use serde_json::{Map, Value};

use crate::error::HostError;

/// The shape a host function's argument must have. A deliberately small structural schema: enough
/// to refuse nonsense before the bridge sees it, not a general JSON Schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Anything.
    Any,
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool,
    /// An integer (never a fraction).
    Int,
    /// A string.
    Text,
    /// An array whose elements all have this shape.
    Array(Box<Shape>),
    /// An object with these required fields; `open` decides whether other fields are allowed.
    Object {
        /// The required fields and their shapes.
        required: Vec<(String, Shape)>,
        /// Whether fields not listed are allowed.
        open: bool,
    },
}

impl Shape {
    /// Why `value` does not have this shape, or `None` if it does. `path` names the argument.
    pub fn violation(&self, value: &Value, path: &str) -> Option<String> {
        match (self, value) {
            (Shape::Any, _)
            | (Shape::Null, Value::Null)
            | (Shape::Bool, Value::Bool(_))
            | (Shape::Text, Value::String(_)) => None,
            (Shape::Int, Value::Number(n)) if n.is_i64() || n.is_u64() => None,
            (Shape::Array(element), Value::Array(items)) => items
                .iter()
                .enumerate()
                .find_map(|(i, v)| element.violation(v, &format!("{path}[{i}]"))),
            (Shape::Object { required, open }, Value::Object(object)) => {
                violation_in_object(required, *open, object, path)
            }
            (expected, _) => Some(format!("{path}: expected {}", expected.describe())),
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Shape::Any => "anything",
            Shape::Null => "null",
            Shape::Bool => "true or false",
            Shape::Int => "an integer",
            Shape::Text => "a string",
            Shape::Array(_) => "an array",
            Shape::Object { .. } => "an object",
        }
    }
}

fn violation_in_object(
    required: &[(String, Shape)],
    open: bool,
    object: &Map<String, Value>,
    path: &str,
) -> Option<String> {
    for (name, shape) in required {
        match object.get(name) {
            None => return Some(format!("{path}: missing `{name}`")),
            Some(value) => {
                if let Some(why) = shape.violation(value, &format!("{path}.{name}")) {
                    return Some(why);
                }
            }
        }
    }
    if !open
        && let Some(extra) = object
            .keys()
            .find(|k| !required.iter().any(|(name, _)| name == *k))
    {
        return Some(format!("{path}: unexpected `{extra}`"));
    }
    None
}

/// One function a script may call as `host.<name>(argument)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFn {
    /// The name, a JavaScript identifier (`get_doc`).
    pub name: String,
    /// The shape of its one argument.
    pub args_schema: Shape,
}

impl HostFn {
    /// A host function taking one argument of `args_schema`.
    pub fn new(name: &str, args_schema: Shape) -> Self {
        Self {
            name: name.to_owned(),
            args_schema,
        }
    }
}

/// The closed allow-list of host functions an execution may call. Nothing else is reachable:
/// `host` has these, plus `now` and `random` (deterministic, from the invocation).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostSurface {
    /// The functions, by name.
    pub functions: Vec<HostFn>,
}

impl HostSurface {
    /// A surface of these functions.
    pub fn new(functions: Vec<HostFn>) -> Self {
        Self { functions }
    }
}

/// What the host provides to one execution. The bridge sees only names and JSON; it decides what
/// each call does, subject to the engine's allow-list and argument schemas.
pub trait HostBridge: Send + Sync {
    /// Performs the host function `name` (already checked against the allow-list and schema).
    ///
    /// # Errors
    ///
    /// A [`HostError`], which the script sees as a thrown `HostError` it may catch.
    fn call(&self, name: &str, args: Value) -> Result<Value, HostError>;

    /// The time of the invocation, in milliseconds since the Unix epoch: the only clock a script
    /// sees (`host.now()`, `Date.now()`, `new Date()`). Not the machine's clock.
    fn now_millis(&self) -> i64;

    /// The seed of this execution's pseudo-random numbers (`host.random()`, `Math.random()`): the
    /// same seed gives the same sequence.
    fn random_seed(&self) -> u64;
}
