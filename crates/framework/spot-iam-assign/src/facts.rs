//! Facts: the plain per-user map a rule condition reads.
//!
//! The caller builds it from the user record and the business data (department, position,
//! company, attributes). This crate never queries anything. A multi-valued fact is a list.
//! In a condition a fact is written `facts.company` (or, equivalently, `doc.company`: the
//! shared language's one record root).

use std::collections::BTreeMap;

use serde::Deserialize;
use spot_expr::{Context, DateTime, Root, Value};
use time::OffsetDateTime;

/// A user, as the permission engine names them.
pub type UserId = String;

/// One fact value. `serde` form for fixtures; code can also use [`Facts::set_value`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FactValue {
    /// A yes or no.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// Text (a department, a company id).
    Text(String),
    /// A multi-valued fact.
    List(Vec<FactValue>),
}

impl From<&FactValue> for Value {
    fn from(v: &FactValue) -> Self {
        match v {
            FactValue::Bool(b) => Value::Bool(*b),
            FactValue::Int(n) => Value::Int(*n),
            FactValue::Text(t) => Value::Text(t.clone()),
            FactValue::List(items) => Value::List(items.iter().map(Value::from).collect()),
        }
    }
}

/// The facts of one user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Facts(BTreeMap<String, FactValue>);

impl Facts {
    /// No facts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a text fact.
    #[must_use]
    pub fn text(mut self, name: &str, value: &str) -> Self {
        self.0
            .insert(name.to_owned(), FactValue::Text(value.to_owned()));
        self
    }

    /// Adds a list fact of texts.
    #[must_use]
    pub fn texts(mut self, name: &str, values: &[&str]) -> Self {
        self.0.insert(
            name.to_owned(),
            FactValue::List(
                values
                    .iter()
                    .map(|v| FactValue::Text((*v).to_owned()))
                    .collect(),
            ),
        );
        self
    }

    /// Adds a fact of any shape.
    #[must_use]
    pub fn set_value(mut self, name: &str, value: FactValue) -> Self {
        self.0.insert(name.to_owned(), value);
        self
    }

    /// The fact called `name`.
    pub fn get(&self, name: &str) -> Option<&FactValue> {
        self.0.get(name)
    }
}

/// The evaluation context over one user's facts at one instant.
pub(crate) struct FactsContext<'a> {
    pub(crate) facts: &'a Facts,
    pub(crate) now: DateTime,
}

impl<'a> FactsContext<'a> {
    /// `None` when `now` is outside the language's years 0001 to 9999.
    pub(crate) fn new(facts: &'a Facts, now: OffsetDateTime) -> Option<Self> {
        let micros = now.unix_timestamp_nanos().div_euclid(1000);
        let now = DateTime::from_micros(i64::try_from(micros).ok()?)?;
        Some(Self { facts, now })
    }
}

impl Context for FactsContext<'_> {
    fn field(&self, root: Root, path: &[String]) -> Option<Value> {
        if root != Root::Doc {
            return None;
        }
        match path {
            [name] => self.facts.get(name).map(Value::from),
            _ => None,
        }
    }

    fn now(&self) -> DateTime {
        self.now
    }
}
