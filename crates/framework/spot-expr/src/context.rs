//! The host's side of an evaluation: field values and `now`.

use std::collections::BTreeMap;

use crate::ast::Root;
use crate::value::{DateTime, Value};

/// What an expression reads from the outside world. Nothing else is readable.
pub trait Context {
    /// The value at `root.path...`, or `None` when there is none (which reads as `null`).
    fn field(&self, root: Root, path: &[String]) -> Option<Value>;

    /// The instant `now()` returns.
    fn now(&self) -> DateTime;
}

/// A context over a map from dotted path (`doc.qty`, `parent.status`) to value. Useful for
/// tests, tools and hosts that have flat records.
#[derive(Clone, Debug)]
pub struct MapContext {
    values: BTreeMap<String, Value>,
    now: DateTime,
}

impl MapContext {
    /// An empty context at `now`.
    pub fn new(now: DateTime) -> Self {
        Self {
            values: BTreeMap::new(),
            now,
        }
    }

    /// Sets the value of a dotted path.
    pub fn with(mut self, path: &str, value: Value) -> Self {
        self.values.insert(path.to_owned(), value);
        self
    }
}

impl Context for MapContext {
    fn field(&self, root: Root, path: &[String]) -> Option<Value> {
        let key = format!("{}.{}", root.as_str(), path.join("."));
        self.values.get(&key).cloned()
    }

    fn now(&self) -> DateTime {
        self.now
    }
}
