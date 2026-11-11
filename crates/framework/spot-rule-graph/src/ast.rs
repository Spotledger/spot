//! The parsed rule program: rules, heads with aggregates, and body literals.
//!
//! The AST is the parser's output and the compiler's input. It is deliberately close to the
//! source: a body literal is a positive atom, a negated atom, or a guard (a shared-expression
//! [`Expr`]).

use rust_decimal::Decimal;
use spot_expr::Expr;

use crate::datum::Datum;

/// A 1-based source position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// The line.
    pub line: u32,
    /// The column.
    pub column: u32,
}

impl Span {
    /// A position.
    #[must_use]
    pub fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

/// The aggregation function of a head argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    /// The number of distinct values of the variable.
    Count,
    /// The sum of the integer or decimal values of the variable.
    Sum,
    /// The smallest value of the variable.
    Min,
    /// The largest value of the variable.
    Max,
}

impl AggFunc {
    /// The function's spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    /// The function named `name`, or `None`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "count" => Some(Self::Count),
            "sum" => Some(Self::Sum),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

/// A literal constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Const {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A decimal.
    Decimal(Decimal),
    /// Text.
    Text(String),
}

impl Const {
    /// The constant as a datum.
    #[must_use]
    pub fn to_datum(&self) -> Datum {
        match self {
            Self::Null => Datum::Null,
            Self::Bool(b) => Datum::Bool(*b),
            Self::Int(n) => Datum::Int(*n),
            Self::Decimal(d) => Datum::Decimal(*d),
            Self::Text(s) => Datum::Text(s.clone()),
        }
    }

    /// The constant's canonical text, for a content hash.
    #[must_use]
    pub fn canonical(&self) -> String {
        self.to_datum().canonical()
    }
}

/// One term of an atom: a variable, the anonymous variable, or a constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// A named variable.
    Var(String),
    /// `_`: binds nothing and constrains nothing.
    Anon,
    /// A constant.
    Const(Const),
}

/// One head argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadArg {
    /// A variable, one of the grouping keys.
    Var(String),
    /// A constant, one of the grouping keys.
    Const(Const),
    /// An aggregate over a variable.
    Agg {
        /// The function.
        func: AggFunc,
        /// The aggregated variable.
        var: String,
        /// Where the aggregate was written.
        span: Span,
    },
}

/// One body literal.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// A positive atom: `predicate(term, ...)`.
    Atom {
        /// The predicate name.
        predicate: String,
        /// Its arguments.
        args: Vec<Term>,
        /// Where it was written.
        span: Span,
    },
    /// A negated atom: `not predicate(term, ...)`.
    Neg {
        /// The predicate name.
        predicate: String,
        /// Its arguments.
        args: Vec<Term>,
        /// Where it was written.
        span: Span,
    },
    /// A guard: `where (expr)`, a shared expression over the rule's variables.
    Guard {
        /// The expression source, verbatim (without the parentheses).
        source: String,
        /// The parsed expression.
        expr: Expr,
        /// The variables the guard reads, in first-seen order.
        vars: Vec<String>,
        /// Where the guard's `where` was written.
        span: Span,
    },
}

/// A rule's head.
#[derive(Clone, Debug, PartialEq)]
pub struct Head {
    /// The derived predicate.
    pub predicate: String,
    /// Its arguments.
    pub args: Vec<HeadArg>,
    /// Where the head was written.
    pub span: Span,
}

/// One rule: `head :- body.`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    /// The head.
    pub head: Head,
    /// The body literals, in source order.
    pub body: Vec<Literal>,
    /// Where the rule began.
    pub span: Span,
}

/// A parsed program: rules in source order.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Program {
    /// The rules.
    pub rules: Vec<Rule>,
}
