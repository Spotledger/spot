//! Typed errors for `spot-plan` (one enum, stable `plan.*` codes).
//!
//! Every variant carries the DocType and, where one applies, the field the problem is about,
//! so callers can locate it without parsing the message. Callers match on
//! [`PlanError::code`], never on the text: the codes are this crate's API contract and never
//! change in a breaking way.
//!
//! [`PlanSet::build`](crate::PlanSet::build) collects **all** problems (a `Vec<PlanError>`)
//! instead of stopping at the first; the single-value checks return one error.

/// One problem with a plan's compilation or with a checked value.
///
/// `code()` returns the stable machine-readable `plan.<snake_case>` code. [`PlanError`] is
/// `#[non_exhaustive]`: new variants may be added and callers must keep a catch-all arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// A field has a default in the metadata, but the compiled app view carries no parsed
    /// value for it: the default does not parse for the field's type. A compile-time error,
    /// so a malformed default can never reach a plan.
    #[error("`{doctype}.{field}`: default `{default}`: {message}")]
    BadDefault {
        /// The DocType holding the field.
        doctype: String,
        /// The field whose default does not parse.
        field: String,
        /// The raw default text, as written in the metadata.
        default: String,
        /// What is wrong with the default, in a sentence.
        message: String,
    },
    /// A DocType's definition and its compiled app view disagree (a field count, a field name
    /// or the view itself). This is a misuse of [`compile`](crate::DocPlan::compile) or a bug;
    /// a real `EffectiveMeta` always gives a matching view.
    #[error("`{doctype}`: {message}")]
    ViewMismatch {
        /// The DocType the pair disagrees about.
        doctype: String,
        /// What is mismatched, in a sentence.
        message: String,
    },
    /// A value was checked against a field the plan does not have.
    #[error("`{doctype}`: field `{field}` does not exist")]
    UnknownField {
        /// The DocType the check ran on.
        doctype: String,
        /// The unknown field name.
        field: String,
    },
    /// The value's shape does not fit the field's kind, or a field that accepts nothing got a
    /// value (child tables and secrets).
    #[error("`{doctype}.{field}`: {got} does not fit the field's kind")]
    KindMismatch {
        /// The DocType holding the field.
        doctype: String,
        /// The field the value was checked against.
        field: String,
        /// What the value is, in words.
        got: &'static str,
    },
    /// The text is longer than the field's `max_chars`.
    #[error("`{doctype}.{field}`: text is longer than {max} characters")]
    TooLong {
        /// The DocType holding the field.
        doctype: String,
        /// The field the value was checked against.
        field: String,
        /// The field's `max_chars`.
        max: u32,
    },
    /// The decimal has more fractional digits than the field's `scale`; it is rejected, never
    /// rounded (ADR-12).
    #[error("`{doctype}.{field}`: value has more than {scale} decimal places")]
    Scale {
        /// The DocType holding the field.
        doctype: String,
        /// The field the value was checked against.
        field: String,
        /// The field's `scale`.
        scale: u8,
    },
    /// The decimal does not fit the field's `precision` at the field's `scale`.
    #[error("`{doctype}.{field}`: value does not fit precision {precision}, scale {scale}")]
    Precision {
        /// The DocType holding the field.
        doctype: String,
        /// The field the value was checked against.
        field: String,
        /// The field's `precision`.
        precision: u8,
        /// The field's `scale`.
        scale: u8,
    },
    /// The value is not one of the select field's options.
    #[error("`{doctype}.{field}`: `{value}` is not one of the options")]
    NotAnOption {
        /// The DocType holding the field.
        doctype: String,
        /// The field the value was checked against.
        field: String,
        /// The rejected value.
        value: String,
    },
    /// An expression in a rule reads a field that doesn't exist.
    #[error(
        "`{doctype}.{field}`: {rule} expression reads unknown field `{unknown_field}` at position {position}"
    )]
    ExprUnknownField {
        /// The DocType holding the field.
        doctype: String,
        /// The field whose rule has the problem.
        field: String,
        /// The rule type (depends_on, mandatory_depends_on, etc.).
        rule: String,
        /// The unknown field name.
        unknown_field: String,
        /// Position in the expression text (character offset).
        position: usize,
    },
    /// A rule expression does not parse, or a `fetch_from` path is malformed.
    #[error("`{doctype}.{field}`: {rule} expression is invalid at position {position}: {message}")]
    ExprInvalid {
        /// The DocType holding the field.
        doctype: String,
        /// The field whose rule is invalid.
        field: String,
        /// The rule kind (`depends_on`, `mandatory_depends_on`, `read_only_depends_on`,
        /// `default` or `fetch_from`).
        rule: String,
        /// Character offset of the problem in the expression text.
        position: usize,
        /// The parser's message.
        message: String,
    },
    /// A rule expression failed to evaluate against a document (for example a non-boolean
    /// result where a boolean is required).
    #[error("`{doctype}.{field}`: {rule} expression did not evaluate: {message}")]
    ExprEval {
        /// The DocType holding the field.
        doctype: String,
        /// The field whose rule failed.
        field: String,
        /// The rule kind.
        rule: String,
        /// The evaluator's message.
        message: String,
    },
    /// A cycle was detected among fetch_from and default expressions.
    #[error("`{doctype}`: cycle detected in rule expressions: {cycle}")]
    ExprCycle {
        /// The DocType with the cycle.
        doctype: String,
        /// Description of the cycle (field names involved).
        cycle: String,
    },
    /// fetch_from references a link field whose target DocType has no plan or the target field doesn't exist.
    #[error(
        "`{doctype}.{field}`: fetch_from links to `{link_field}` whose target has no field `{target_field}`"
    )]
    FetchFromUnknownTarget {
        /// The DocType holding the field.
        doctype: String,
        /// The field with the fetch_from.
        field: String,
        /// The link field name.
        link_field: String,
        /// The target field name.
        target_field: String,
    },
    /// A naming pattern is malformed or invalid.
    #[error("`{doctype}`: naming pattern is invalid: {message}")]
    NamingInvalid {
        /// The DocType with the invalid naming pattern.
        doctype: String,
        /// Description of the error.
        message: String,
    },
}

impl PlanError {
    /// The stable machine-readable code for this error, for logs and API errors. Every code is
    /// `plan.<snake_case>`; two variants never share one.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadDefault { .. } => "plan.compile.bad_default",
            Self::ViewMismatch { .. } => "plan.compile.view_mismatch",
            Self::UnknownField { .. } => "plan.field.unknown",
            Self::KindMismatch { .. } => "plan.value.kind",
            Self::TooLong { .. } => "plan.value.too_long",
            Self::Scale { .. } => "plan.value.scale",
            Self::Precision { .. } => "plan.value.precision",
            Self::NotAnOption { .. } => "plan.value.not_an_option",
            Self::ExprUnknownField { .. } => "plan.expr.unknown_field",
            Self::ExprInvalid { .. } => "plan.expr.invalid",
            Self::ExprEval { .. } => "plan.expr.eval",
            Self::ExprCycle { .. } => "plan.expr.cycle",
            Self::FetchFromUnknownTarget { .. } => "plan.fetch_from.unknown_target",
            Self::NamingInvalid { .. } => "plan.naming.invalid",
        }
    }

    /// The DocType the problem is about.
    #[must_use]
    pub fn doctype(&self) -> &str {
        match self {
            Self::BadDefault { doctype, .. }
            | Self::ViewMismatch { doctype, .. }
            | Self::UnknownField { doctype, .. }
            | Self::KindMismatch { doctype, .. }
            | Self::TooLong { doctype, .. }
            | Self::Scale { doctype, .. }
            | Self::Precision { doctype, .. }
            | Self::NotAnOption { doctype, .. }
            | Self::ExprUnknownField { doctype, .. }
            | Self::ExprInvalid { doctype, .. }
            | Self::ExprEval { doctype, .. }
            | Self::ExprCycle { doctype, .. }
            | Self::FetchFromUnknownTarget { doctype, .. }
            | Self::NamingInvalid { doctype, .. } => doctype,
        }
    }

    /// The field the problem is about; `None` for DocType-level problems.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::BadDefault { field, .. }
            | Self::UnknownField { field, .. }
            | Self::KindMismatch { field, .. }
            | Self::TooLong { field, .. }
            | Self::Scale { field, .. }
            | Self::Precision { field, .. }
            | Self::NotAnOption { field, .. }
            | Self::ExprUnknownField { field, .. }
            | Self::ExprInvalid { field, .. }
            | Self::ExprEval { field, .. }
            | Self::FetchFromUnknownTarget { field, .. } => Some(field),
            Self::ViewMismatch { .. } | Self::ExprCycle { .. } | Self::NamingInvalid { .. } => None,
        }
    }
}
