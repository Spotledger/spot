//! The identity of a release.
//!
//! A release is named by its tag (`2026.10`, `v1.4.0`, a build label). This crate never parses a
//! version and never does arithmetic on one: the release that follows another is an argument the
//! release train supplies ([`Release::next`](crate::check::Release)), because only the train
//! knows whether the next release is `2.1.0` or a fresh `2026.11`. [`ReleaseId`] is therefore an
//! opaque, non-empty label, and its [`Ord`] is a stable presentation order (lexicographic), not
//! a version comparison.

use std::fmt;

use crate::error::Error;

/// Longest release label this crate accepts, in bytes.
pub const MAX_LABEL_LEN: usize = 64;

/// A release's label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseId(String);

impl ReleaseId {
    /// Wraps a release label, refusing an empty or whitespace-laden one.
    pub fn new(label: &str) -> Result<Self, Error> {
        let valid = !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && label.trim() == label
            && !label.chars().any(char::is_whitespace)
            && !label.chars().any(char::is_control);
        if valid {
            Ok(Self(label.to_owned()))
        } else {
            Err(Error::BadRelease {
                message: format!(
                    "`{label}` is not a release label: use 1 to {MAX_LABEL_LEN} bytes with no \
                     whitespace"
                ),
            })
        }
    }

    /// The label as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
