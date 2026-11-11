//! Errors with stable `devtools.*` codes (catalogue: `spot-errors/catalogue/devtools.toml`).
//!
//! Callers match on [`DevToolsError::code`], never on the text: the codes are this crate's API
//! contract.

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, DevToolsError>;

/// One developer-tools failure.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DevToolsError {
    /// A debugger or trace operation could not read or write its files.
    #[error("the devtools file operation on `{path}` failed: {message}")]
    Io {
        /// The path the operation named.
        path: String,
        /// What the operating system reported.
        message: String,
    },
    /// A Debug Adapter Protocol message was malformed.
    #[error("the debug adapter protocol message is malformed: {message}")]
    Protocol {
        /// What is wrong with the message.
        message: String,
    },
    /// The debugger or trace configuration is invalid.
    #[error("invalid devtools configuration: {message}")]
    Config {
        /// What is wrong.
        message: String,
    },
    /// Debugging was requested for a site marked production.
    #[error("site `{site}` is marked production: debugging is a development capability")]
    Production {
        /// The production site.
        site: String,
    },
    /// Debugging was requested in a release build.
    #[error("this is a release build: the debugger is a development-build capability")]
    ReleaseBuild,
    /// The development token is missing or wrong.
    #[error("the development token is missing or incorrect")]
    Token,
    /// A debug server could not bind its address.
    #[error("the debug server cannot bind {address}: {message}")]
    Bind {
        /// The address it tried.
        address: String,
        /// What the operating system reported.
        message: String,
    },
    /// A non-loopback debug address was requested without explicitly allowing it.
    #[error("the debug server address {address} is not loopback; set `allow_remote` to bind it")]
    NotLoopback {
        /// The rejected address.
        address: String,
    },
    /// The script did not transpile, compile or run.
    #[error("the script debug session failed: {message}")]
    Script {
        /// What failed.
        message: String,
    },
    /// The debug session is not in a state that can answer the request.
    #[error("the debug session cannot do that now: {message}")]
    Session {
        /// Why not.
        message: String,
    },
    /// A paused script was resumed because the debugger did not continue it in time.
    #[error("the script was resumed after {millis} ms without a debugger command")]
    PauseTimeout {
        /// The pause timeout in milliseconds.
        millis: u128,
    },
    /// The source instrumenter met source it cannot hook safely.
    #[error("the script cannot be instrumented: {message}")]
    Instrument {
        /// What is wrong.
        message: String,
    },
    /// A WebAssembly boundary report is inconsistent.
    #[error("the WebAssembly boundary report is invalid: {message}")]
    Wasm {
        /// What is wrong.
        message: String,
    },
}

impl DevToolsError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "devtools.io",
            Self::Protocol { .. } => "devtools.protocol",
            Self::Config { .. } => "devtools.config",
            Self::Production { .. } => "devtools.production",
            Self::ReleaseBuild => "devtools.release_build",
            Self::Token => "devtools.token",
            Self::Bind { .. } => "devtools.bind",
            Self::NotLoopback { .. } => "devtools.not_loopback",
            Self::Script { .. } => "devtools.script",
            Self::Session { .. } => "devtools.session",
            Self::PauseTimeout { .. } => "devtools.pause_timeout",
            Self::Instrument { .. } => "devtools.instrument",
            Self::Wasm { .. } => "devtools.wasm",
        }
    }

    /// An I/O error against `path`.
    pub fn io(path: impl AsRef<std::path::Path>, error: &std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            message: error.to_string(),
        }
    }

    /// The code and message as one line, for a log or a DAP error response.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{}: {self}", self.code())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::DevToolsError;

    #[test]
    fn every_code_is_dotted_lower_case_and_unique() {
        let errors = [
            DevToolsError::Io {
                path: "p".into(),
                message: String::new(),
            },
            DevToolsError::Protocol {
                message: String::new(),
            },
            DevToolsError::Config {
                message: String::new(),
            },
            DevToolsError::Production {
                site: "acme.example".into(),
            },
            DevToolsError::ReleaseBuild,
            DevToolsError::Token,
            DevToolsError::Bind {
                address: "127.0.0.1:1".into(),
                message: String::new(),
            },
            DevToolsError::NotLoopback {
                address: "0.0.0.0:1".into(),
            },
            DevToolsError::Script {
                message: String::new(),
            },
            DevToolsError::Session {
                message: String::new(),
            },
            DevToolsError::PauseTimeout { millis: 10 },
            DevToolsError::Instrument {
                message: String::new(),
            },
            DevToolsError::Wasm {
                message: String::new(),
            },
        ];
        let mut codes: Vec<&str> = errors.iter().map(DevToolsError::code).collect();
        for code in &codes {
            assert!(code.starts_with("devtools."), "code {code}");
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), errors.len(), "codes must be unique");
    }
}
