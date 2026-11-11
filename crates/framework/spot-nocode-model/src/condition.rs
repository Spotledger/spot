//! The friendly condition builder: field/operator/value rows a business user fills in, compiled
//! to the shared expression language (`spot-expr`, ADR-28).
//!
//! A designer never types an expression. They pick a field, an operator and a value, and the
//! builder turns the rows into canonical expression text with [`Expr`]'s own `Display`, so the
//! text is exactly what `spot-expr` prints and parses. The compiled text is then parsed once
//! more, so a condition that reaches a definition is known to be well-formed before the
//! registry ever sees it.

use spot_expr::{BinaryOp, Expr, Root, parse};

/// How the rows combine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Join {
    /// Every row must hold.
    #[default]
    And,
    /// At least one row must hold.
    Or,
}

impl Join {
    /// The operator that combines the rows.
    fn op(self) -> BinaryOp {
        match self {
            Self::And => BinaryOp::And,
            Self::Or => BinaryOp::Or,
        }
    }
}

/// One row of the builder: a field, an operator and the value(s) it compares to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// The field, by storage name.
    pub field: String,
    /// What to test.
    pub operator: Operator,
    /// The value(s); a single value for every operator but [`Operator::In`].
    pub values: Vec<Value>,
}

impl Row {
    /// A row comparing `field` with `operator` to one value.
    #[must_use]
    pub fn new(field: &str, operator: Operator, value: Value) -> Self {
        Self {
            field: field.to_owned(),
            operator,
            values: vec![value],
        }
    }
}

/// What a row tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    /// The field holds a value.
    IsSet,
    /// The field holds no value.
    IsNotSet,
    /// Equals.
    Equals,
    /// Does not equal.
    NotEquals,
    /// Greater than.
    GreaterThan,
    /// At least.
    GreaterOrEqual,
    /// Less than.
    LessThan,
    /// At most.
    LessOrEqual,
    /// Is one of the listed values.
    In,
}

/// A literal a row compares to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    /// A text value.
    Text(String),
    /// A whole number.
    Int(i64),
    /// A true/false value.
    Bool(bool),
}

impl Value {
    /// The literal as an expression node.
    fn expr(&self) -> Expr {
        match self {
            Self::Text(s) => Expr::Text(s.clone()),
            Self::Int(n) => Expr::Int(*n),
            Self::Bool(b) => Expr::Bool(*b),
        }
    }
}

/// A whole condition: rows combined with [`Join`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// How the rows combine.
    #[serde(default)]
    pub join: Join,
    /// The rows, in order. At least one.
    pub rows: Vec<Row>,
}

/// Why a condition could not be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConditionError {
    /// The stable `expr.*` code when the compiled text did not parse, else `nocode_model.bad_condition`.
    pub code: String,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl Condition {
    /// A condition of one row.
    #[must_use]
    pub fn single(row: Row) -> Self {
        Self {
            join: Join::And,
            rows: vec![row],
        }
    }

    /// The expression text the condition compiles to: the rows joined with `and`/`or`, each a
    /// comparison of `doc.<field>` with the row's value(s). Pure and deterministic.
    ///
    /// # Errors
    ///
    /// [`ConditionError`] when there are no rows, a row's value count does not fit its
    /// operator, or the compiled text does not parse (which would be a bug in the builder, but
    /// is checked rather than trusted).
    pub fn compile(&self) -> Result<String, ConditionError> {
        if self.rows.is_empty() {
            return Err(ConditionError {
                code: crate::codes::BAD_CONDITION.to_owned(),
                message: "the condition has no rows".to_owned(),
            });
        }
        let mut nodes = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            nodes.push(row.expr()?);
        }
        let mut combined = nodes.remove(0);
        for node in nodes {
            combined = Expr::Binary(self.join.op(), Box::new(combined), Box::new(node));
        }
        let text = combined.to_string();
        parse(&text).map_err(|e| ConditionError {
            code: e.code.to_owned(),
            message: e.message.clone(),
        })?;
        Ok(text)
    }

    /// The field names the condition reads, in first-seen order.
    #[must_use]
    pub fn fields(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = Vec::new();
        for row in &self.rows {
            if !seen.contains(&row.field.as_str()) {
                seen.push(row.field.as_str());
            }
        }
        seen
    }
}

impl Row {
    /// The row's comparison as an expression node.
    fn expr(&self) -> Result<Expr, ConditionError> {
        let path = || Expr::Path(Root::Doc, vec![self.field.clone()]);
        let bad = |message: &str| ConditionError {
            code: crate::codes::BAD_CONDITION.to_owned(),
            message: message.to_owned(),
        };
        let one = |values: &[Value]| -> Result<Expr, ConditionError> {
            match values {
                [only] => Ok(only.expr()),
                _ => Err(bad("this operator needs exactly one value")),
            }
        };
        let node = match self.operator {
            Operator::IsSet => Expr::Binary(BinaryOp::Ne, Box::new(path()), Box::new(Expr::Null)),
            Operator::IsNotSet => {
                Expr::Binary(BinaryOp::Eq, Box::new(path()), Box::new(Expr::Null))
            }
            Operator::Equals => {
                Expr::Binary(BinaryOp::Eq, Box::new(path()), Box::new(one(&self.values)?))
            }
            Operator::NotEquals => {
                Expr::Binary(BinaryOp::Ne, Box::new(path()), Box::new(one(&self.values)?))
            }
            Operator::GreaterThan => {
                Expr::Binary(BinaryOp::Gt, Box::new(path()), Box::new(one(&self.values)?))
            }
            Operator::GreaterOrEqual => {
                Expr::Binary(BinaryOp::Ge, Box::new(path()), Box::new(one(&self.values)?))
            }
            Operator::LessThan => {
                Expr::Binary(BinaryOp::Lt, Box::new(path()), Box::new(one(&self.values)?))
            }
            Operator::LessOrEqual => {
                Expr::Binary(BinaryOp::Le, Box::new(path()), Box::new(one(&self.values)?))
            }
            Operator::In => {
                if self.values.is_empty() {
                    return Err(bad("`is one of` needs at least one value"));
                }
                let list = Expr::List(self.values.iter().map(Value::expr).collect());
                Expr::Binary(BinaryOp::In, Box::new(path()), Box::new(list))
            }
        };
        Ok(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_row_compiles_to_a_comparison() {
        let condition = Condition::single(Row::new(
            "status",
            Operator::Equals,
            Value::Text("Open".to_owned()),
        ));
        assert_eq!(
            condition.compile().ok(),
            Some("doc.status == \"Open\"".to_owned())
        );
    }

    #[test]
    fn rows_combine_with_and_or() {
        let condition = Condition {
            join: Join::Or,
            rows: vec![
                Row::new("priority", Operator::GreaterOrEqual, Value::Int(3)),
                Row::new("status", Operator::IsSet, Value::Bool(true)),
            ],
        };
        let text = condition.compile().unwrap_or_default();
        assert_eq!(text, "doc.priority >= 3 or doc.status != null");
        assert!(parse(&text).is_ok());
    }

    #[test]
    fn in_builds_a_list() {
        let row = Row {
            field: "status".to_owned(),
            operator: Operator::In,
            values: vec![
                Value::Text("Open".to_owned()),
                Value::Text("Closed".to_owned()),
            ],
        };
        let condition = Condition::single(row);
        assert_eq!(
            condition.compile().ok(),
            Some("doc.status in [\"Open\", \"Closed\"]".to_owned())
        );
    }

    #[test]
    fn an_empty_condition_is_refused() {
        let condition = Condition {
            join: Join::And,
            rows: vec![],
        };
        assert!(condition.compile().is_err());
    }

    #[test]
    fn fields_are_listed_once() {
        let condition = Condition {
            join: Join::And,
            rows: vec![
                Row::new("a", Operator::IsSet, Value::Bool(true)),
                Row::new("a", Operator::IsSet, Value::Bool(true)),
                Row::new("b", Operator::IsSet, Value::Bool(true)),
            ],
        };
        assert_eq!(condition.fields(), vec!["a", "b"]);
    }
}
