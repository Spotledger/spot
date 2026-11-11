//! Typed errors with stable `release_meta.*` codes (catalogue: `release_meta.toml`).

/// Stable codes of this crate (`release_meta.*`, catalogued in `release_meta.toml`).
pub mod codes {
    /// A patch declaration file is not valid TOML, or a field is missing or out of range.
    pub const BAD_PATCH: &str = "release_meta.bad_patch";
    /// The behaviour changelog is not valid TOML, or an entry is missing a field.
    pub const BAD_CHANGELOG: &str = "release_meta.bad_changelog";
    /// A behaviour key is not `doctype:<name>` or `seam:<name>`.
    pub const BAD_KEY: &str = "release_meta.bad_key";
}

/// Anything the release-metadata reader refuses to accept.
///
/// The checker itself does not error on a bad release: it returns [`Problem`](crate::Problem)
/// values. These errors are for input that cannot be parsed at all, so the caller can say
/// "this file is malformed" instead of guessing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A patch declaration could not be read.
    #[error("{}: `{path}`: {message}", codes::BAD_PATCH)]
    BadPatch {
        /// The file the declaration came from, or a placeholder for an in-memory parse.
        path: String,
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The behaviour changelog could not be read.
    #[error("{}: {message}", codes::BAD_CHANGELOG)]
    BadChangelog {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// A behaviour key is not in the documented form.
    #[error("{}: `{key}`: {reason}", codes::BAD_KEY)]
    BadKey {
        /// The offending key, as written.
        key: String,
        /// Why it is not usable.
        reason: String,
    },
}

impl Error {
    /// The stable code, for callers that match on it rather than the message.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadPatch { .. } => codes::BAD_PATCH,
            Self::BadChangelog { .. } => codes::BAD_CHANGELOG,
            Self::BadKey { .. } => codes::BAD_KEY,
        }
    }

    /// A bad patch declaration quoted from a file path.
    pub(crate) fn bad_patch(path: &str, message: impl Into<String>) -> Self {
        Self::BadPatch {
            path: path.to_owned(),
            message: message.into(),
        }
    }
}
