//! Typed errors for the template engine (WP-PLT-12).

use std::fmt;

/// Everything that can fail while rendering a template.
///
/// Template name, line number and the offending source line are included
/// wherever the engine knows them. Sandbox limit violations (fuel, recursion
/// depth, output size), filter failures and money arithmetic are distinct
/// variants so callers can react precisely.
///
/// Line numbers are 1-based; [`usize`] zero means the engine could not
/// determine a line.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// The template was not found in its source.
    NotFound {
        /// The template name used for the lookup.
        name: String,
    },
    /// The template did not parse.
    Syntax {
        /// The template name.
        name: String,
        /// 1-based line of the error, or `0` when unknown.
        line: usize,
        /// The parser's message.
        message: String,
    },
    /// A missing value was rendered while strict undefined behaviour is on.
    Undefined {
        /// The template name.
        name: String,
        /// 1-based line, or `0` when unknown.
        line: usize,
        /// The variable (or attribute chain) that was undefined, e.g.
        /// `"invoice.total"`; `"<expression>"` when the failing text is not
        /// an identifier, `"unknown"` when the engine cannot locate it.
        variable: String,
        /// The offending source line, empty when unknown.
        line_text: String,
    },
    /// The fuel budget ran out (hard sandbox limit).
    FuelExhausted {
        /// The template name.
        name: String,
        /// 1-based line, or `0` when unknown.
        line: usize,
    },
    /// The recursion depth limit was exceeded (hard sandbox limit).
    RecursionLimitExceeded {
        /// The template name.
        name: String,
        /// 1-based line, or `0` when unknown.
        line: usize,
    },
    /// The rendered output exceeded the configured byte limit.
    OutputLimitExceeded {
        /// The configured maximum in bytes.
        limit: usize,
        /// The output size in bytes.
        actual: usize,
    },
    /// A template tried to compute money or quantities (ADR-12).
    NotComputable {
        /// Explanation of the rejected operation.
        message: String,
    },
    /// A `money`/`number`/`amount_in_words` filter argument was rejected, for
    /// example a wrong value kind or too many fractional digits.
    InvalidInput {
        /// Explanation of the rejected input.
        message: String,
    },
    /// An unknown currency code was passed to a filter; never a silent
    /// default.
    UnknownCurrency {
        /// The offending currency code.
        code: String,
    },
    /// An unknown locale identifier was passed to a filter; never a silent
    /// default.
    UnknownLocale {
        /// The offending locale identifier.
        id: String,
    },
    /// An unknown date or datetime format name was passed to a filter.
    InvalidDateFormat {
        /// The offending format name.
        format: String,
    },
    /// A filter failed while running.
    Filter {
        /// The filter's failure message.
        message: String,
    },
    /// Any other rendering failure.
    Render {
        /// The template name.
        name: String,
        /// 1-based line, or `0` when unknown.
        line: usize,
        /// The renderer's message.
        message: String,
    },
}

// Display: every variant names what the caller needs — template, line, and
// the offending detail — so logs and error pages are self-explanatory.
impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(f, "template not found: {name}"),
            Self::Syntax {
                name,
                line,
                message,
            } => {
                write!(
                    f,
                    "syntax error in template {name} ({}): {message}",
                    line_part(*line)
                )
            }
            Self::Undefined {
                name,
                line,
                variable,
                line_text,
            } => {
                if line_text.is_empty() {
                    write!(
                        f,
                        "undefined value `{variable}` in template {name} ({})",
                        line_part(*line)
                    )
                } else {
                    write!(
                        f,
                        "undefined value `{variable}` in template {name} ({}): {line_text}",
                        line_part(*line)
                    )
                }
            }
            Self::FuelExhausted { name, line } => {
                write!(
                    f,
                    "template {name} exhausted its fuel budget ({})",
                    line_part(*line)
                )
            }
            Self::RecursionLimitExceeded { name, line } => {
                write!(
                    f,
                    "template {name} exceeded the recursion limit ({})",
                    line_part(*line)
                )
            }
            Self::OutputLimitExceeded { limit, actual } => {
                write!(
                    f,
                    "template output grew to {actual} bytes, over the {limit} byte limit"
                )
            }
            Self::NotComputable { message } => write!(f, "{message}"),
            Self::InvalidInput { message } => write!(f, "{message}"),
            Self::UnknownCurrency { code } => write!(f, "unknown currency '{code}'"),
            Self::UnknownLocale { id } => write!(f, "unknown locale '{id}'"),
            Self::InvalidDateFormat { format } => write!(f, "unknown date format '{format}'"),
            Self::Filter { message } => write!(f, "{message}"),
            Self::Render {
                name,
                line,
                message,
            } => {
                write!(
                    f,
                    "render error in template {name} ({}): {message}",
                    line_part(*line)
                )
            }
        }
    }
}

impl std::error::Error for TemplateError {}

fn line_part(line: usize) -> String {
    if line == 0 {
        "unknown line".to_owned()
    } else {
        format!("line {line}")
    }
}

#[cfg(test)]
mod tests {
    use super::TemplateError;

    #[test]
    fn display_carries_names_and_lines() {
        let error = TemplateError::NotFound {
            name: "invoice.html".to_owned(),
        };
        assert_eq!(error.to_string(), "template not found: invoice.html");

        let error = TemplateError::Undefined {
            name: "t.txt".to_owned(),
            line: 2,
            variable: "invoice.total".to_owned(),
            line_text: "{{ missing }}".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("t.txt"), "{text}");
        assert!(text.contains("line 2"), "{text}");
        assert!(text.contains("invoice.total"), "{text}");
        assert!(text.contains("{{ missing }}"), "{text}");

        let error = TemplateError::FuelExhausted {
            name: "deep.typ".to_owned(),
            line: 0,
        };
        assert!(error.to_string().contains("unknown line"));
    }
}
