//! One problem a module contract declares, with the position of the declaration that is wrong.

use spot_ts::SourcePosition;

/// One thing wrong with a controller module's contract. A load reports **all** of them at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The stable code (`tscontroller.invalid_module`, `tscontroller.compile_failed`).
    pub code: String,
    /// A developer-facing sentence naming what is wrong.
    pub message: String,
    /// The dotted path inside the contract (`steps.before_save.point`), for the report.
    pub path: String,
    /// The position in the TypeScript the author wrote, when the engine or the transpiler knows
    /// one.
    pub position: Option<SourcePosition>,
}

impl Problem {
    /// A problem at `path` with no known position.
    pub fn new(code: &str, path: &str, message: impl Into<String>) -> Problem {
        Problem {
            code: code.to_owned(),
            message: message.into(),
            path: path.to_owned(),
            position: None,
        }
    }

    /// The same problem, at a known source position.
    #[must_use]
    pub fn at(mut self, position: SourcePosition) -> Problem {
        self.position = Some(position);
        self
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.position {
            Some(position) => write!(
                f,
                "{}: {}:{}: {}",
                self.path, position.line, position.column, self.message
            ),
            None => write!(f, "{}: {}", self.path, self.message),
        }
    }
}
