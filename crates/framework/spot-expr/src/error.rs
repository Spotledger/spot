//! Errors, with the stable `expr.*` codes of the specification (section 9).

/// The stable error codes. They are the contract; messages are not.
pub mod codes {
    /// The text does not match the grammar, or an unexpected character.
    pub const SYNTAX: &str = "expr.syntax";
    /// A text literal is not closed.
    pub const UNTERMINATED_STRING: &str = "expr.unterminated_string";
    /// An unknown escape in a text literal.
    pub const BAD_ESCAPE: &str = "expr.bad_escape";
    /// A numeric literal does not fit its type.
    pub const NUMBER_RANGE: &str = "expr.number_range";
    /// More than 2000 nodes.
    pub const TOO_COMPLEX: &str = "expr.too_complex";
    /// Nesting deeper than 64.
    pub const TOO_DEEP: &str = "expr.too_deep";
    /// An identifier that is not a keyword, call or path root.
    pub const UNKNOWN_NAME: &str = "expr.unknown_name";
    /// A call to a function not in the library.
    pub const UNKNOWN_FUNCTION: &str = "expr.unknown_function";
    /// A call with an unacceptable number of arguments.
    pub const ARITY: &str = "expr.arity";
    /// An operand or argument of the wrong type.
    pub const TYPE: &str = "expr.type";
    /// `/` or `%` by zero.
    pub const DIVISION_BY_ZERO: &str = "expr.division_by_zero";
    /// A result out of range.
    pub const OVERFLOW: &str = "expr.overflow";
    /// Text that is not a date or datetime.
    pub const BAD_DATE: &str = "expr.bad_date";
    /// Text that is not an integer or decimal.
    pub const BAD_NUMBER: &str = "expr.bad_number";
}

/// A problem found while parsing: the code, a message and a character offset.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code} at {offset}: {message}")]
pub struct ParseError {
    /// The stable code.
    pub code: &'static str,
    /// What is wrong, in a sentence.
    pub message: String,
    /// The 0-based character offset of the offending token.
    pub offset: usize,
}

impl ParseError {
    pub(crate) fn new(code: &'static str, offset: usize, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            offset,
        }
    }
}

/// A problem found while evaluating: the code and a message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct EvalError {
    /// The stable code.
    pub code: &'static str,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl EvalError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub(crate) type EvalResult<T> = Result<T, EvalError>;
