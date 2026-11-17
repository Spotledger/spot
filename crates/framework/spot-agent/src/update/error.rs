//! What can go wrong before the updater engine takes over.

use std::fmt;

use spot_updater::{UpdaterError, UpdaterErrorCode};

use super::{CODE_FAILED, CODE_REFUSED, CODE_UNAVAILABLE};

/// Why an update step did not run. Carries names and codes only, never a key, a token, a
/// payload byte or a path of a secret file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateError {
    /// The gateway (or the repository) publishes no update for this agent.
    Unavailable,
    /// The gateway answered `agent.revoked`: the caller stops and deletes its keys.
    Revoked,
    /// Network, TLS, 429 or 5xx: try again later.
    Retryable(String),
    /// The source refused for a reason a retry will not change (a code).
    Source(String),
    /// A part of the offer is malformed (the part's name).
    Wire(&'static str),
    /// The trust anchor or the revocation store is missing or unreadable (a reason).
    Trust(String),
    /// The install layout is not what an update needs (a reason).
    Layout(String),
    /// A check of `spot-updater` refused, with its stable code.
    Updater(UpdaterErrorCode),
}

impl UpdateError {
    /// The result code the agent reports for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => CODE_UNAVAILABLE,
            Self::Wire(_) | Self::Updater(_) => CODE_REFUSED,
            Self::Revoked
            | Self::Retryable(_)
            | Self::Source(_)
            | Self::Trust(_)
            | Self::Layout(_) => CODE_FAILED,
        }
    }

    /// A short, secret-free sentence for the result output.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Unavailable => "no update is published for this agent".to_owned(),
            Self::Revoked => "the gateway revoked this agent".to_owned(),
            Self::Retryable(why) => format!("the update source is not reachable: {why}"),
            Self::Source(code) => format!("the update source refused: {code}"),
            Self::Wire(part) => format!("the offered {part} is malformed"),
            Self::Trust(why) => format!("the update trust files are unusable: {why}"),
            Self::Layout(why) => format!("the install layout is not updatable: {why}"),
            Self::Updater(code) => format!("refused: {}", code.as_str()),
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail())
    }
}

impl std::error::Error for UpdateError {}

impl From<UpdaterError> for UpdateError {
    fn from(error: UpdaterError) -> Self {
        Self::Updater(error.code())
    }
}
