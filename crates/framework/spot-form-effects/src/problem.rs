//! What an invalid or forbidden effect turns into: a *problem* the response carries, never an
//! applied effect, and never an error that fails the whole form event.

use serde::{Deserialize, Serialize};

/// One effect that was dropped, and why.
///
/// Problems are safe to send to the client: [`EffectProblem::path`] is only filled in when the
/// triggering user may see the path, so a problem never reveals that a hidden field exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectProblem {
    /// The stable `form_effects.*` code.
    pub code: String,
    /// The effect's position in the emitted sequence (0-based), when one effect caused it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// The path the effect named, when the user may see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A plain-language explanation, safe to show.
    pub message: String,
}

impl EffectProblem {
    /// A problem about one effect.
    #[must_use]
    pub fn at(index: usize, code: &str, message: impl Into<String>) -> EffectProblem {
        EffectProblem {
            code: code.to_owned(),
            index: Some(index),
            path: None,
            message: message.into(),
        }
    }

    /// The same problem naming a path the user may see.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> EffectProblem {
        self.path = Some(path.into());
        self
    }

    /// The same problem at an effect position.
    #[must_use]
    pub fn with_index(mut self, index: usize) -> EffectProblem {
        self.index = Some(index);
        self
    }
}

/// A script's server-side failure: the operator's record of what went wrong.
///
/// This never reaches a client. The response carries one safe `show_message` instead; the details
/// (script name, engine code and source position) are for the server log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptFailure {
    /// The script's registered name.
    pub script: String,
    /// The stable code (`form_effects.script_failed`, `script.timeout`, …).
    pub code: String,
    /// The engine's or the runner's explanation.
    pub message: String,
    /// The line and column in the script, when the engine knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The column of [`ScriptFailure::line`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

impl ScriptFailure {
    /// A failure without a source position.
    #[must_use]
    pub fn new(script: &str, code: &str, message: impl Into<String>) -> ScriptFailure {
        ScriptFailure {
            script: script.to_owned(),
            code: code.to_owned(),
            message: message.into(),
            line: None,
            column: None,
        }
    }

    /// The same failure at a source position.
    #[must_use]
    pub fn at(mut self, line: Option<u32>, column: Option<u32>) -> ScriptFailure {
        self.line = line;
        self.column = column;
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn a_problem_without_a_path_omits_it_from_the_wire() {
        let wire =
            serde_json::to_value(EffectProblem::at(2, "form_effects.path_unknown", "no")).unwrap();
        assert_eq!(
            wire,
            json!({
                "code": "form_effects.path_unknown",
                "index": 2,
                "message": "no",
            })
        );
    }
}
