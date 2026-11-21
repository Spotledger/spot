//! Invocation typing: [`Invocation`] in, [`Validated`] out — the value render and the
//! agent both consume.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{OpId, ParamType, ParamValue};

/// One call to a management operation: which operation and which parameters.
///
/// Typed but not yet validated — the catalogue checks it and fills defaults into
/// [`Validated`]. Parameter values are keyed by name in a `BTreeMap`, so an invocation
/// serialises deterministically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invocation {
    /// The operation being called.
    pub op: OpId,
    /// The parameters the caller supplies, by name.
    pub params: BTreeMap<String, ParamValue>,
}

impl Invocation {
    /// An invocation of `op` with no parameters (a catalogue can fill defaults in).
    #[must_use]
    pub fn new(op: OpId) -> Self {
        Self {
            op,
            params: BTreeMap::new(),
        }
    }

    /// Adds one parameter value, replacing any previous value of the same name.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: ParamValue) -> Self {
        self.params.insert(name.into(), value);
        self
    }
}

/// One validated parameter after the catalogue's checks: name, declared type, the value
/// (caller-supplied or the spec's default), where the value came from, and whether it is
/// a secret. Declaration order is preserved — this is what render walks.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedParam {
    /// The parameter's name (snake form, as declared).
    pub name: String,
    /// The parameter's declared type.
    pub ty: ParamType,
    /// The value: what the caller supplied, or the spec's default.
    pub value: ParamValue,
    /// Whether the value came from the spec's default rather than the caller.
    pub from_default: bool,
    /// Whether the parameter is a secret (its value is redacted in `Debug` and never
    /// rendered by `render`).
    pub secret: bool,
}

/// An invocation that passed every check: the operation, and every declared parameter that
/// ends up with a value (caller-supplied or default), in declaration order.
///
/// This is what `render` consumes and what the signed command envelope later carries.
/// Secrets are present as values here but redacted in `Debug`, and render never prints
/// them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validated {
    /// The operation being called.
    pub op: OpId,
    /// The parameters, in declaration order.
    pub params: Vec<AcceptedParam>,
}

impl std::fmt::Debug for Validated {
    /// Prints the operation id and every parameter's name, type and origin; a secret's
    /// value is `[redacted]`, so a dumped `Validated` never leaks one into a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Validated { op: ")?;
        write!(f, "{:?}", self.op.as_str())?;
        f.write_str(", params: [")?;
        for (index, param) in self.params.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            if param.secret {
                write!(
                    f,
                    "{}: {} = [redacted]{}",
                    param.name,
                    param.ty_debug_name(),
                    if param.from_default { " (default)" } else { "" }
                )?;
            } else {
                write!(
                    f,
                    "{}: {} = {:?}{}",
                    param.name,
                    param.ty_debug_name(),
                    param.value,
                    if param.from_default { " (default)" } else { "" }
                )?;
            }
        }
        f.write_str("] }")
    }
}

impl AcceptedParam {
    /// A short human name of the parameter's type, for diagnostics (`int 1..10`,
    /// `text 64`, `enum`, `duration`, `bool`, `list of text`).
    #[must_use]
    pub fn ty_debug_name(&self) -> String {
        ty_debug_name(&self.ty)
    }
}

/// A short human name of a parameter type, for diagnostics.
#[must_use]
pub(crate) fn ty_debug_name(ty: &ParamType) -> String {
    match ty {
        ParamType::Bool => "bool".to_owned(),
        ParamType::Int { min, max } => format!("int {min}..{max}"),
        ParamType::Text { max_len, pattern } => {
            if pattern.is_some() {
                format!("text {max_len} patterned")
            } else {
                format!("text {max_len}")
            }
        }
        ParamType::Enum(_) => "enum".to_owned(),
        ParamType::Duration => "duration".to_owned(),
        ParamType::List(inner) => format!("list of {}", ty_debug_name(inner)),
    }
}

/// The placeholder `render` writes for a secret parameter's value, documented as the one
/// spelling both shells pass through as a single argument.
///
/// A command line that carries a secret never carries the secret: the console (or the
/// CLI) asks the user at execution time, and the agent resolves it out of band. The text
/// between the angle brackets is literal — `<prompt>` — not a slot to fill in; anything
/// that wants the real value must ask a human or a secret store, never the command line.
pub const SECRET_PLACEHOLDER: &str = "<prompt>";
