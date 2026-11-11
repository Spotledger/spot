//! Typed errors for `spot-devserver` (one enum, stable `devserver.*` codes).
//!
//! Callers match on [`DevError::code`], never on the text: the codes are this crate's API
//! contract and never change in a breaking way (see
//! `crates/framework/spot-errors/catalogue/devserver.toml`).

use std::path::Path;

/// Result of every fallible operation in this crate.
pub type DevResult<T> = Result<T, DevError>;

/// One development-server failure.
///
/// `#[non_exhaustive]`: new variants may be added and callers must keep a catch-all arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DevError {
    /// A file operation failed.
    #[error("the file operation on `{path}` failed: {message}")]
    Io {
        /// The path the operation named.
        path: String,
        /// What the operating system reported, in plain language.
        message: String,
    },
    /// The platform file watcher could not be started or failed.
    #[error("the file watcher failed: {message}")]
    Watch {
        /// What the watcher reported.
        message: String,
    },
    /// No reload step handles a change class — a configuration mistake, never a data error.
    #[error("no reload step handles `{kind}` changes")]
    UnknownStep {
        /// The change class with no step.
        kind: String,
    },
    /// The metadata did not build. The previous snapshot is still serving.
    #[error("the metadata reload failed with {} error(s)", .errors.len())]
    Metadata {
        /// Every metadata or plan error found, already rendered.
        errors: Vec<String>,
    },
    /// A view did not parse or did not validate.
    #[error("the view reload failed with {} error(s)", .errors.len())]
    View {
        /// Every view error found, already rendered.
        errors: Vec<String>,
    },
    /// A script did not transpile or did not type-check.
    #[error("the script reload failed with {} error(s)", .errors.len())]
    Script {
        /// Every script or declaration error found, already rendered.
        errors: Vec<String>,
    },
    /// The project build command failed.
    #[error("the build command `{command}` failed: {message}")]
    Build {
        /// The command that was run, rendered for the developer.
        command: String,
        /// The tail of its output, in plain language.
        message: String,
    },
    /// A rebuilt component did not validate or load.
    #[error("the component reload failed: {message}")]
    Component {
        /// What the component host reported.
        message: String,
    },
    /// A scratch-site operation failed.
    #[error("the dev site operation failed: {message}")]
    Site {
        /// What the site operation reported.
        message: String,
    },
    /// The configuration is invalid.
    #[error("invalid dev-server configuration: {message}")]
    Config {
        /// What is wrong with the configuration.
        message: String,
    },
}

impl DevError {
    /// The stable machine-readable code for this error, for logs and API errors. Every code is
    /// `devserver.<snake_case>`; two variants never share one.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "devserver.io",
            Self::Watch { .. } => "devserver.watch",
            Self::UnknownStep { .. } => "devserver.unknown_step",
            Self::Metadata { .. } => "devserver.metadata",
            Self::View { .. } => "devserver.view",
            Self::Script { .. } => "devserver.script",
            Self::Build { .. } => "devserver.build",
            Self::Component { .. } => "devserver.component",
            Self::Site { .. } => "devserver.site",
            Self::Config { .. } => "devserver.config",
        }
    }

    /// Whether the failure is one the running server recovers from on the next change.
    ///
    /// A metadata, view, script or component failure is a **change** failure: the server keeps
    /// running and the previous good version keeps serving. A watcher or configuration failure
    /// is fatal.
    #[must_use]
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Metadata { .. }
                | Self::View { .. }
                | Self::Script { .. }
                | Self::Build { .. }
                | Self::Component { .. }
        )
    }

    /// An I/O error against `path`.
    pub fn io(path: impl AsRef<Path>, error: &std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            message: error.to_string(),
        }
    }

    /// An I/O error whose message is already rendered.
    pub fn io_message(path: impl AsRef<Path>, message: impl Into<String>) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_code_is_dotted_lower_case_and_unique() {
        let errors = [
            DevError::Io {
                path: "a".into(),
                message: String::new(),
            },
            DevError::Watch {
                message: String::new(),
            },
            DevError::UnknownStep {
                kind: "other".into(),
            },
            DevError::Metadata { errors: vec![] },
            DevError::View { errors: vec![] },
            DevError::Script { errors: vec![] },
            DevError::Build {
                command: "cargo".into(),
                message: String::new(),
            },
            DevError::Component {
                message: String::new(),
            },
            DevError::Site {
                message: String::new(),
            },
            DevError::Config {
                message: String::new(),
            },
        ];
        let mut codes: Vec<&str> = errors.iter().map(DevError::code).collect();
        for code in &codes {
            assert!(code.starts_with("devserver."), "code {code}");
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), errors.len(), "codes must be unique");
    }

    #[test]
    fn change_failures_are_recoverable_and_watcher_failures_are_not() {
        assert!(DevError::Metadata { errors: vec![] }.is_recoverable());
        assert!(
            !DevError::Watch {
                message: String::new()
            }
            .is_recoverable()
        );
    }
}
