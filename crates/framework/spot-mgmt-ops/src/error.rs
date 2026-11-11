//! The one error type of the catalogue, with stable machine-readable `ops.*` codes.
//!
//! Codes are stable: for logs, metrics and API responses use [`CatalogueError::code`] and
//! [`InvocationError::code`], never the message text. Catalogue problems are authoring
//! mistakes (module code), so [`CatalogueError`] carries no operation spec; invocation
//! problems come from a caller's input, so [`InvocationError`] names the parameter involved.

/// The catalogue could not be built from the module-declared [`OperationSpec`]s.
///
/// `build` reports every problem it found, so one fix-and-rerun cycle clears them all.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogueError {
    /// Two modules (or two `add` calls) declared the same operation id.
    #[error("{module_a} and {module_b} both register operation id {id}")]
    DuplicateId {
        /// The first module to register the id.
        module_a: String,
        /// The other module that registered it.
        module_b: String,
        /// The operation id both modules declared.
        id: String,
    },

    /// One operation declares two parameters with the same name.
    #[error("{op}: parameter {param} is declared twice")]
    DuplicateParam {
        /// The operation id the duplicate was found in.
        op: String,
        /// The parameter name that appears twice.
        param: String,
    },

    /// A non-required parameter declares a default its own type does not accept.
    #[error("{op}: parameter {param}: {detail}")]
    DefaultInvalid {
        /// The operation id the parameter belongs to.
        op: String,
        /// The parameter name.
        param: String,
        /// What is wrong with the default, in words.
        detail: String,
    },

    /// A parameter marked `secret` also declares a default: a secret is always supplied
    /// at call time (or prompted for), never persisted in the spec.
    #[error("{op}: secret parameter {param} must not declare a default")]
    SecretWithDefault {
        /// The operation id the parameter belongs to.
        op: String,
        /// The parameter name.
        param: String,
    },

    /// A `Text` parameter's `pattern` is not a valid regular expression.
    #[error("{op}: parameter {param}: the pattern does not compile: {message}")]
    PatternInvalid {
        /// The operation id the parameter belongs to.
        op: String,
        /// The parameter name.
        param: String,
        /// What the regex engine said about the pattern.
        message: String,
    },

    /// A parameter name is not a lower-case identifier: it could never become a
    /// `--kebab-case` flag.
    #[error(
        "{op}: parameter name `{param}` must be a lower-case identifier (a-z, then a-z, 0-9 and underscore)"
    )]
    ParamName {
        /// The operation id the parameter belongs to.
        op: String,
        /// The offending name.
        param: String,
    },

    /// One parameter's flag spelling would collide with another's `--no-<name>` false
    /// form (for example a boolean `no_cleanup` and a boolean `cleanup` in one operation).
    #[error(
        "{op}: the flag --{flag} of parameter {param} collides with the --no-form of parameter {other}"
    )]
    FlagCollision {
        /// The operation id the parameters belong to.
        op: String,
        /// The flag spelling both parameters reach.
        flag: String,
        /// The parameter whose flag spells it.
        param: String,
        /// The boolean parameter whose `--no-form` spells it.
        other: String,
    },

    /// An `Int` parameter's bounds are inverted: `min` is greater than `max`, so no value
    /// could ever be accepted.
    #[error("{op}: parameter {param}: min {min} is greater than max {max}")]
    IntRangeEmpty {
        /// The operation id the parameter belongs to.
        op: String,
        /// The parameter name.
        param: String,
        /// The lower bound.
        min: i64,
        /// The upper bound.
        max: i64,
    },

    /// An `Enum` parameter's choice set is empty, so no value could ever be accepted.
    #[error("{op}: parameter {param} is an enum with no choices")]
    EnumEmpty {
        /// The operation id the parameter belongs to.
        op: String,
        /// The parameter name.
        param: String,
    },

    /// A `Text` parameter carries a blank `doc`-less name or an empty pattern: the
    /// pattern is present but matches nothing usefully (an empty regex is allowed; a
    /// pattern of whitespace-only text is not a pattern).
    #[error("{op}: parameter {param}: {detail}")]
    TextShape {
        /// The operation id the parameter belongs to.
        op: String,
        /// The parameter name.
        param: String,
        /// What is wrong, in words.
        detail: String,
    },
}

/// The stable machine-readable code of a catalogue error, for logs, metrics and API
/// responses.
impl CatalogueError {
    /// The stable machine-readable code of this error.
    ///
    /// The codes, with their meanings:
    ///
    /// - `ops.duplicate_id` — two modules register the same operation id;
    /// - `ops.duplicate_param` — one operation declares a parameter name twice;
    /// - `ops.default_invalid` — a default does not satisfy its parameter's own type;
    /// - `ops.secret_with_default` — a secret parameter declares a default;
    /// - `ops.pattern_invalid` — a `Text` pattern does not compile as a regex;
    /// - `ops.param_name` — a parameter name is not a lower-case identifier;
    /// - `ops.flag_collision` — one parameter's flag collides with another's `--no-form`;
    /// - `ops.int_range_empty` — an `Int` parameter has `min > max`;
    /// - `ops.enum_empty` — an `Enum` parameter has no choices;
    /// - `ops.text_shape` — a `Text` parameter's bounds or pattern are unusable.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateId { .. } => DUPLICATE_ID,
            Self::DuplicateParam { .. } => DUPLICATE_PARAM,
            Self::DefaultInvalid { .. } => DEFAULT_INVALID,
            Self::SecretWithDefault { .. } => SECRET_WITH_DEFAULT,
            Self::PatternInvalid { .. } => PATTERN_INVALID,
            Self::ParamName { .. } => PARAM_NAME,
            Self::FlagCollision { .. } => FLAG_COLLISION,
            Self::IntRangeEmpty { .. } => INT_RANGE_EMPTY,
            Self::EnumEmpty { .. } => ENUM_EMPTY,
            Self::TextShape { .. } => TEXT_SHAPE,
        }
    }

    /// The operation id the error was found in (for grouping problems by operation).
    #[must_use]
    pub fn op(&self) -> &str {
        match self {
            Self::DuplicateId { .. } => "",
            Self::DuplicateParam { op, .. }
            | Self::DefaultInvalid { op, .. }
            | Self::SecretWithDefault { op, .. }
            | Self::PatternInvalid { op, .. }
            | Self::ParamName { op, .. }
            | Self::FlagCollision { op, .. }
            | Self::IntRangeEmpty { op, .. }
            | Self::EnumEmpty { op, .. }
            | Self::TextShape { op, .. } => op,
        }
    }
}

/// Two modules register the same operation id.
pub const DUPLICATE_ID: &str = "ops.duplicate_id";
/// One operation declares a parameter name twice.
pub const DUPLICATE_PARAM: &str = "ops.duplicate_param";
/// A default does not satisfy its parameter's own type.
pub const DEFAULT_INVALID: &str = "ops.default_invalid";
/// A secret parameter declares a default.
pub const SECRET_WITH_DEFAULT: &str = "ops.secret_with_default";
/// A `Text` pattern does not compile as a regex.
pub const PATTERN_INVALID: &str = "ops.pattern_invalid";
/// A parameter name is not a lower-case identifier.
pub const PARAM_NAME: &str = "ops.param_name";
/// A parameter's flag collides with another parameter's `--no-form`.
pub const FLAG_COLLISION: &str = "ops.flag_collision";
/// An `Int` parameter has `min > max`.
pub const INT_RANGE_EMPTY: &str = "ops.int_range_empty";
/// An `Enum` parameter has no choices.
pub const ENUM_EMPTY: &str = "ops.enum_empty";
/// A `Text` parameter's bounds or pattern are unusable.
pub const TEXT_SHAPE: &str = "ops.text_shape";

/// An invocation failed validation: one entry per problem, all reported together.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvocationError {
    /// The invocation names an operation that is not in the catalogue.
    #[error("no operation {op} is registered")]
    UnknownOp {
        /// The operation id the caller asked for.
        op: String,
    },

    /// The caller supplied a parameter the operation does not declare.
    #[error("{op}: unknown parameter {param}")]
    UnknownParam {
        /// The operation id.
        op: String,
        /// The parameter name the caller supplied.
        param: String,
    },

    /// A required parameter was not supplied and has no default.
    #[error("{op}: required parameter {param} is missing")]
    MissingParam {
        /// The operation id.
        op: String,
        /// The parameter name that is missing.
        param: String,
    },

    /// The value's shape does not fit the parameter's type (an `Int` value for a `Text`
    /// parameter, a list for a scalar, a list element for its element type).
    #[error("{op}: parameter {param}: wrong type")]
    WrongType {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
    },

    /// An integer or duration value sits outside the parameter's bounds.
    #[error("{op}: parameter {param}: value {value} is out of range {min}..{max}")]
    OutOfRange {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
        /// The rejected value.
        value: i64,
        /// The smallest accepted value.
        min: i64,
        /// The largest accepted value.
        max: i64,
    },

    /// A text value does not match the parameter's whole-value pattern.
    #[error("{op}: parameter {param}: the value does not match the required pattern")]
    PatternMismatch {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
    },

    /// An enum value is not one of the parameter's choices.
    #[error("{op}: parameter {param}: the value is not one of the accepted choices")]
    NotInEnum {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
    },

    /// A text value is longer than the parameter's `max_len` (in characters).
    #[error("{op}: parameter {param}: the value is longer than {max_len} characters")]
    TooLong {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
        /// The maximum accepted length.
        max_len: usize,
    },
}

/// The stable machine-readable code of an invocation error.
impl InvocationError {
    /// The stable machine-readable code of this error.
    ///
    /// The codes, with their meanings:
    ///
    /// - `ops.unknown_op` — the operation id is not in the catalogue;
    /// - `ops.unknown_param` — the caller supplied a parameter the operation does not declare;
    /// - `ops.missing_param` — a required parameter was not supplied;
    /// - `ops.wrong_type` — the value's shape does not fit the parameter's type;
    /// - `ops.out_of_range` — an integer or duration value is outside the bounds;
    /// - `ops.pattern_mismatch` — a text value does not match the whole-value pattern;
    /// - `ops.not_in_enum` — an enum value is not one of the choices;
    /// - `ops.too_long` — a text value exceeds the parameter's maximum length.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownOp { .. } => UNKNOWN_OP,
            Self::UnknownParam { .. } => UNKNOWN_PARAM,
            Self::MissingParam { .. } => MISSING_PARAM,
            Self::WrongType { .. } => WRONG_TYPE,
            Self::OutOfRange { .. } => OUT_OF_RANGE,
            Self::PatternMismatch { .. } => PATTERN_MISMATCH,
            Self::NotInEnum { .. } => NOT_IN_ENUM,
            Self::TooLong { .. } => TOO_LONG,
        }
    }

    /// The parameter the error names, when it is about one specific parameter.
    #[must_use]
    pub fn param(&self) -> Option<&str> {
        match self {
            Self::UnknownOp { .. } => None,
            Self::UnknownParam { param, .. }
            | Self::MissingParam { param, .. }
            | Self::WrongType { param, .. }
            | Self::OutOfRange { param, .. }
            | Self::PatternMismatch { param, .. }
            | Self::NotInEnum { param, .. }
            | Self::TooLong { param, .. } => Some(param),
        }
    }
}

/// The operation id is not in the catalogue.
pub const UNKNOWN_OP: &str = "ops.unknown_op";
/// The caller supplied a parameter the operation does not declare.
pub const UNKNOWN_PARAM: &str = "ops.unknown_param";
/// A required parameter was not supplied.
pub const MISSING_PARAM: &str = "ops.missing_param";
/// The value's shape does not fit the parameter's type.
pub const WRONG_TYPE: &str = "ops.wrong_type";
/// An integer or duration value sits outside the parameter's bounds.
pub const OUT_OF_RANGE: &str = "ops.out_of_range";
/// A text value does not match the whole-value pattern.
pub const PATTERN_MISMATCH: &str = "ops.pattern_mismatch";
/// An enum value is not one of the parameter's choices.
pub const NOT_IN_ENUM: &str = "ops.not_in_enum";
/// A text value exceeds the parameter's maximum length.
pub const TOO_LONG: &str = "ops.too_long";
