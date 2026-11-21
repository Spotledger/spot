//! The one error type, with stable machine-readable `money.*` codes.

use thiserror::Error;

/// Why a money operation failed. Every variant has a stable [`MoneyError::code`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MoneyError {
    /// Two amounts of different currencies were combined.
    #[error("money.currency_mismatch: cannot combine {left} and {right}; convert explicitly first")]
    CurrencyMismatch {
        /// The currency of the left operand.
        left: String,
        /// The currency of the right operand.
        right: String,
    },
    /// Two quantities of different units of measure were combined.
    #[error("money.uom_mismatch: cannot combine quantities in {left} and {right}")]
    UomMismatch {
        /// The unit of the left operand (`none` for a bare number).
        left: String,
        /// The unit of the right operand (`none` for a bare number).
        right: String,
    },
    /// The result does not fit a decimal.
    #[error("money.overflow: {operation} does not fit a decimal")]
    Overflow {
        /// The operation that overflowed.
        operation: &'static str,
    },
    /// The exact result needs more than 28 fractional digits, and is never rounded silently.
    #[error(
        "money.inexact: {operation} needs more than 28 fractional digits; round the inputs first"
    )]
    Inexact {
        /// The operation whose exact result does not fit.
        operation: &'static str,
    },
    /// A currency code is not three uppercase ASCII letters.
    #[error("money.bad_currency: `{0}` is not a currency code (three uppercase letters)")]
    BadCurrency(String),
    /// A currency is not in the table.
    #[error("money.unknown_currency: `{0}` is not in the currency table")]
    UnknownCurrency(String),
    /// A currency appears twice in a table.
    #[error("money.duplicate_currency: `{0}` appears twice in the currency table")]
    DuplicateCurrency(String),
    /// A unit of measure is empty, too long or contains whitespace or control characters.
    #[error("money.bad_uom: `{0}` is not a unit of measure (1 to 32 characters, no whitespace)")]
    BadUom(String),
    /// A scale is above 28, the most a decimal holds.
    #[error("money.bad_scale: scale {0} is above 28")]
    BadScale(u32),
    /// A rounding step is not positive, or has more fractional digits than the rule's scale.
    #[error("money.bad_step: {0}")]
    BadStep(String),
    /// A decimal could not be read exactly.
    #[error("money.bad_decimal: `{0}` is not an exact decimal")]
    BadDecimal(String),
    /// An amount is not a multiple of the rule's unit, so it cannot be split exactly.
    #[error("money.off_grid: {0} is not a multiple of the rule's unit; round it first")]
    OffGrid(String),
    /// Weights cannot apportion an amount.
    #[error("money.bad_weights: {0}")]
    BadWeights(&'static str),
    /// A rate is not usable (for example one that makes a tax divisor zero or negative).
    #[error("money.bad_rate: {0}")]
    BadRate(&'static str),
    /// No rule applies and there is no default.
    #[error("money.no_rule: no rule for {kind} `{key}` and no default")]
    NoRule {
        /// The kind that was looked up.
        kind: String,
        /// The key that was looked up.
        key: String,
    },
}

impl MoneyError {
    /// The stable machine-readable code, also the prefix of the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrencyMismatch { .. } => "money.currency_mismatch",
            Self::UomMismatch { .. } => "money.uom_mismatch",
            Self::Overflow { .. } => "money.overflow",
            Self::Inexact { .. } => "money.inexact",
            Self::BadCurrency(_) => "money.bad_currency",
            Self::UnknownCurrency(_) => "money.unknown_currency",
            Self::DuplicateCurrency(_) => "money.duplicate_currency",
            Self::BadUom(_) => "money.bad_uom",
            Self::BadScale(_) => "money.bad_scale",
            Self::BadStep(_) => "money.bad_step",
            Self::BadDecimal(_) => "money.bad_decimal",
            Self::OffGrid(_) => "money.off_grid",
            Self::BadWeights(_) => "money.bad_weights",
            Self::BadRate(_) => "money.bad_rate",
            Self::NoRule { .. } => "money.no_rule",
        }
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, MoneyError>;
