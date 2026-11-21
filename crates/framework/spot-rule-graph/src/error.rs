//! Typed errors and problems with the stable `rule_graph.*` codes (ADR-26).
//!
//! Callers match on [`RuleGraphError::code`] or on a [`Problem`]'s `code`, never on the
//! message. Every code is catalogued in `spot-errors`' `catalogue/rule_graph.toml`.
//!
//! Compilation never stops at the first fault: [`RuleGraphError::Invalid`] carries **every**
//! problem found, each with the line and column it was found at, so a person fixes a rule
//! file in one pass.

/// The rule text does not match the grammar, or a guard is not a valid expression.
pub const PARSE: &str = "rule_graph.parse";
/// A predicate names neither an edge type, nor a metadata field, nor a derived predicate.
pub const UNKNOWN_PREDICATE: &str = "rule_graph.unknown_predicate";
/// A name shaped like `doctype.field` names a link or table field, which is an edge, not a
/// readable field predicate.
pub const NOT_A_FIELD: &str = "rule_graph.not_a_field";
/// A predicate is used with a different number of arguments than it is defined with.
pub const ARITY: &str = "rule_graph.arity";
/// A derived predicate shares its name with an edge type or a metadata field.
pub const PREDICATE_CONFLICT: &str = "rule_graph.predicate_conflict";
/// A rule is not range-restricted: a variable in the head, a negated atom or a guard is not
/// bound by a positive body atom.
pub const UNSAFE: &str = "rule_graph.unsafe";
/// A guard reads a name that is not a bound variable of the rule.
pub const UNKNOWN_VARIABLE: &str = "rule_graph.unknown_variable";
/// The program cannot be stratified: a cycle runs through negation or through an aggregate.
pub const NOT_STRATIFIED: &str = "rule_graph.not_stratified";
/// An aggregate is malformed or applied to a value it cannot aggregate.
pub const AGGREGATE: &str = "rule_graph.aggregate";
/// The query names a predicate the program does not define.
pub const UNKNOWN_QUERY: &str = "rule_graph.unknown_query";
/// Evaluation derived more tuples than the engine's fuel allows.
pub const FUEL_EXHAUSTED: &str = "rule_graph.fuel_exhausted";
/// Evaluation failed (a guard raised, or a head value was unbound).
pub const EVAL: &str = "rule_graph.eval";
/// A registry operation failed.
pub const REGISTRY: &str = "rule_graph.registry";

/// One problem found while compiling a rule set: its stable code, the source position, and a
/// human sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The stable code ([`PARSE`], [`UNKNOWN_PREDICATE`], ...).
    pub code: &'static str,
    /// The 1-based line of the source text.
    pub line: u32,
    /// The 1-based column of the source text.
    pub column: u32,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl Problem {
    /// A problem at a source position.
    #[must_use]
    pub fn new(code: &'static str, line: u32, column: u32, message: impl Into<String>) -> Self {
        Self {
            code,
            line,
            column,
            message: message.into(),
        }
    }

    /// A problem with no meaningful position (line and column 1).
    #[must_use]
    pub fn plain(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(code, 1, 1, message)
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}: {}",
            self.line, self.column, self.code, self.message
        )
    }
}

/// Every error this crate returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleGraphError {
    /// The program was rejected. Carries every problem found, in source order.
    #[error("invalid rule program: {} problem(s)", .0.len())]
    Invalid(Vec<Problem>),
    /// Evaluation derived more tuples than the engine's fuel allows.
    #[error("rule evaluation exhausted its fuel of {limit} derived tuples")]
    FuelExhausted {
        /// The configured fuel limit.
        limit: u64,
    },
    /// Evaluation failed at run time.
    #[error("{code}: {message}")]
    Eval {
        /// The stable code ([`EVAL`], [`AGGREGATE`], [`UNKNOWN_QUERY`], ...).
        code: &'static str,
        /// What is wrong, in a sentence.
        message: String,
    },
    /// A registry operation failed.
    #[error("{code}: {message}")]
    Registry {
        /// The stable code ([`REGISTRY`]).
        code: &'static str,
        /// What is wrong, in a sentence.
        message: String,
    },
}

impl RuleGraphError {
    /// The primary stable code: the first problem's code for [`RuleGraphError::Invalid`], else
    /// the variant's own code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(problems) => problems.first().map_or(PARSE, |p| p.code),
            Self::FuelExhausted { .. } => FUEL_EXHAUSTED,
            Self::Eval { code, .. } | Self::Registry { code, .. } => code,
        }
    }

    /// A run-time evaluation failure.
    #[must_use]
    pub fn eval(message: impl Into<String>) -> Self {
        Self::Eval {
            code: EVAL,
            message: message.into(),
        }
    }

    /// A malformed aggregate or an aggregate over a value it cannot aggregate.
    #[must_use]
    pub fn aggregate(message: impl Into<String>) -> Self {
        Self::Eval {
            code: AGGREGATE,
            message: message.into(),
        }
    }

    /// A registry failure.
    #[must_use]
    pub fn registry(message: impl Into<String>) -> Self {
        Self::Registry {
            code: REGISTRY,
            message: message.into(),
        }
    }

    /// The problems, when this is [`RuleGraphError::Invalid`].
    #[must_use]
    pub fn problems(&self) -> &[Problem] {
        match self {
            Self::Invalid(problems) => problems,
            _ => &[],
        }
    }
}
