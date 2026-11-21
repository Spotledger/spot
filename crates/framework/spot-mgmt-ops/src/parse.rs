//! The parser: `parse(catalogue, args)` reads exactly what `render` writes, so the CLI's
//! argv form and the console's rendered string are the same language.

use std::collections::BTreeMap;

use crate::model::is_boolish;
use crate::model::{OpId, ParamType, ParamValue};

/// Why an argument vector failed to parse. One problem per parse — the CLI wants the
/// first thing that stopped it, not the full matrix (validation, by contrast, reports
/// every problem at once).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The command does not start with `spot ops` (the crate only parses its own lines).
    #[error("expected the command to start with `spot ops`, got `{0}`")]
    NotAnOpsCommand(String),

    /// The operation path is missing or malformed: 2 to 4 lower-case segments expected.
    #[error("expected an operation path of 2 to 4 lower-case segments after `spot ops`, got `{0}`")]
    BadOperationPath(String),

    /// The operation id is well-formed but the catalogue does not know it.
    #[error("no operation {0} is registered")]
    UnknownOperation(String),

    /// A flag is not spelled `--<name>` or its value is missing.
    #[error("expected `--<param> <value>` at `{0}`")]
    BadFlag(String),

    /// The flag names a parameter the operation does not declare.
    #[error("{op}: unknown parameter {param}")]
    UnknownParam {
        /// The operation id.
        op: String,
        /// The parameter name as typed (kebab form).
        param: String,
    },

    /// The value did not fit the parameter's type: an unparseable integer, a duration
    /// without its whole-second `s` form. The detail says what was expected.
    #[error("{op}: parameter {param}: {detail}")]
    BadValue {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
        /// What was expected, in words.
        detail: String,
    },

    /// The same non-list parameter was supplied twice.
    #[error("{op}: parameter {param} was supplied twice")]
    DuplicateParam {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
    },
}

/// The stable machine-readable code of a parse error, for logs and API responses.
impl ParseError {
    /// The stable machine-readable code of this error.
    ///
    /// The codes, with their meanings:
    ///
    /// - `ops.parse_not_ops` — the argv does not start with `spot ops`;
    /// - `ops.parse_bad_path` — the operation path is missing or malformed;
    /// - `ops.unknown_op` — the operation is not in the catalogue;
    /// - `ops.parse_bad_flag` — a flag or its value is malformed;
    /// - `ops.unknown_param` — the flag names no declared parameter;
    /// - `ops.parse_bad_value` — a value does not fit its parameter's type;
    /// - `ops.parse_duplicate` — a non-list parameter was supplied twice.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAnOpsCommand(_) => NOT_OPS,
            Self::BadOperationPath(_) => BAD_PATH,
            Self::UnknownOperation(_) => crate::error::UNKNOWN_OP,
            Self::BadFlag(_) => BAD_FLAG,
            Self::UnknownParam { .. } => crate::error::UNKNOWN_PARAM,
            Self::BadValue { .. } => BAD_VALUE,
            Self::DuplicateParam { .. } => DUPLICATE,
        }
    }
}

/// The argv does not start with `spot ops`.
pub const NOT_OPS: &str = "ops.parse_not_ops";
/// The operation path is missing or malformed.
pub const BAD_PATH: &str = "ops.parse_bad_path";
/// A flag or its value is malformed.
pub const BAD_FLAG: &str = "ops.parse_bad_flag";
/// A value does not fit its parameter's type.
pub const BAD_VALUE: &str = "ops.parse_bad_value";
/// A non-list parameter was supplied twice.
pub const DUPLICATE: &str = "ops.parse_duplicate";

/// Parses an argument vector — exactly what `render` writes — into an [`Invocation`].
///
/// The catalogue supplies the operation's declared parameter types. A `--no-<name>` flag
/// yields `Bool(false)` for a boolean parameter; a list parameter consumes one value per
/// occurrence of its flag. The result is typed but NOT validated (defaults are not filled
/// in, ranges and patterns are not checked): the caller validates through
/// [`crate::Catalogue::validate`], matching the CLI's parse-then-validate flow.
///
/// # Errors
///
/// [`ParseError`] for the first structural problem: the prefix, the operation path,
/// unknown operations, unknown flags, values that do not fit their parameter's type, and
/// duplicate non-list parameters.
pub fn parse(
    catalogue: &crate::Catalogue,
    args: &[String],
) -> Result<crate::Invocation, ParseError> {
    // The prefix: `spot ops`.
    if args.len() < 2 || args[0] != "spot" || args[1] != "ops" {
        return Err(ParseError::NotAnOpsCommand(
            args.iter().take(3).cloned().collect::<Vec<_>>().join(" "),
        ));
    }

    // The operation path: 2 to 4 segments of valid shape, starting right after `ops`.
    let rest = &args[2..];
    let path_len = rest
        .iter()
        .take(4)
        .take_while(|arg| looks_like_segment(arg))
        .count();
    if path_len < 2 {
        return Err(ParseError::BadOperationPath(
            rest.iter().take(4).cloned().collect::<Vec<_>>().join(" "),
        ));
    }
    let id_text = rest[..path_len].join(".");
    let op_id = OpId::try_new(&id_text)
        .map_err(|_| ParseError::BadOperationPath(rest[..path_len].join(" ")))?;
    let spec = catalogue
        .get(&op_id)
        .ok_or_else(|| ParseError::UnknownOperation(op_id.as_str().to_owned()))?;

    // The flags: `--kebab-name value` per parameter; booleans ride bare or as `--no-*`;
    // lists repeat their flag, one value per occurrence.
    let mut params: BTreeMap<String, ParamValue> = BTreeMap::new();
    let mut lists: BTreeMap<String, Vec<ParamValue>> = BTreeMap::new();
    let mut position = path_len + 2; // index into `args`
    while position < args.len() {
        let arg = args[position].as_str();
        if !arg.starts_with("--") || arg.len() < 3 {
            return Err(ParseError::BadFlag(arg.to_owned()));
        }
        let kebab_name = &arg[2..];

        // The parameter this flag names: kebab -> snake, or the `--no-` form of a boolean.
        let (snake_name, is_no_form) = match kebab_name.strip_prefix("no-") {
            Some(inner) => (inner.replace('-', "_"), true),
            None => (kebab_name.replace('-', "_"), false),
        };
        let Some(param) = spec.params.iter().find(|p| p.name == snake_name) else {
            return Err(ParseError::UnknownParam {
                op: op_id.as_str().to_owned(),
                param: kebab_name.to_owned(),
            });
        };

        // A boolean takes no following value: bare `--flag` means true, `--no-flag`
        // means false. Every other parameter takes exactly one value in the next slot.
        let value = if is_boolish(&param.ty) {
            ParamValue::Bool(!is_no_form)
        } else {
            position += 1;
            let Some(raw) = args.get(position) else {
                return Err(ParseError::BadFlag(format!("--{kebab_name}")));
            };
            parse_value(&op_id, param, raw)?
        };

        // Lists accumulate; everything else refuses a second occurrence.
        if matches!(param.ty, ParamType::List(_)) {
            lists.entry(param.name.clone()).or_default().push(value);
        } else if params.insert(param.name.clone(), value).is_some() {
            return Err(ParseError::DuplicateParam {
                op: op_id.as_str().to_owned(),
                param: param.name.clone(),
            });
        }
        position += 1;
    }

    // Materialise the accumulated lists.
    for (name, values) in lists {
        params.insert(name, ParamValue::List(values));
    }

    Ok(crate::Invocation { op: op_id, params })
}

/// Whether an argument looks like one operation-path segment (`[a-z][a-z0-9_]*`).
fn looks_like_segment(arg: &str) -> bool {
    let mut chars = arg.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parses one raw value against its parameter's type. Parse is structural: the enum
/// membership, ranges and patterns are the validator's job — parse only converts.
fn parse_value(
    op: &OpId,
    param: &crate::model::ParamSpec,
    raw: &str,
) -> Result<ParamValue, ParseError> {
    let bad = |detail: &str| ParseError::BadValue {
        op: op.as_str().to_owned(),
        param: param.name.clone(),
        detail: detail.to_owned(),
    };
    match &param.ty {
        ParamType::Bool => Ok(ParamValue::Bool(raw != "false")),
        ParamType::Int { .. } => raw
            .parse::<i64>()
            .map(ParamValue::Int)
            .map_err(|_| bad("expected an integer")),
        ParamType::Enum(_) => Ok(ParamValue::Text(raw.to_owned())),
        ParamType::Duration => {
            let Some(seconds_text) = raw.strip_suffix('s') else {
                return Err(bad("expected a duration like `30s`"));
            };
            seconds_text
                .parse::<u64>()
                .map(ParamValue::Duration)
                .map_err(|_| bad("expected a duration in whole seconds, like `30s`"))
        }
        // Lists arrive element by element (one flag occurrence each); Text is stored as
        // its text.
        ParamType::List(_) | ParamType::Text { .. } => Ok(ParamValue::Text(raw.to_owned())),
    }
}
