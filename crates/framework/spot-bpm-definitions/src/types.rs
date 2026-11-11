//! The stored version, the checks every store applies before it writes, and the content hash.

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_model::{ContentHash, NodeId, from_text};

use crate::error::{DefinitionError, Result};

/// The largest definition text a store accepts: 256 KiB.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// The largest publisher or note a store accepts.
pub const MAX_LABEL_BYTES: usize = 1024;
/// The most versions one `list` call returns.
pub const MAX_PAGE: usize = 500;

/// Whether a stored version may still be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionState {
    /// In force.
    Published,
    /// No longer started; kept for Runs pinned to it.
    Retired,
}

impl VersionState {
    /// The spelling the PostgreSQL store keeps.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Retired => "retired",
        }
    }

    /// Parses the stored spelling; anything else is `None` (a corrupt row).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "published" => Some(Self::Published),
            "retired" => Some(Self::Retired),
            _ => None,
        }
    }
}

/// One immutable published version. The text and the hash never change after publish; only the
/// state and `retired_at` do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    /// The process name.
    pub name: String,
    /// The version number the store assigned (`latest + 1`, starting at 1).
    pub version: u32,
    /// The process text as published.
    pub text: String,
    /// The content hash of the text's process (64 lowercase hex characters).
    pub content_hash: String,
    /// Published or retired.
    pub state: VersionState,
    /// Who published it.
    pub published_by: String,
    /// When it was published (the caller's `now`).
    pub published_at: OffsetDateTime,
    /// The publisher's note.
    pub note: Option<String>,
    /// When it was retired, when it is.
    pub retired_at: Option<OffsetDateTime>,
    /// Who retired it, when it is retired.
    pub retired_by: Option<String>,
}

/// Checks a process name against the model's grammar.
///
/// # Errors
/// [`DefinitionError::NameInvalid`].
pub fn check_name(name: &str) -> Result<()> {
    if NodeId::new(name).is_ok() {
        Ok(())
    } else {
        Err(DefinitionError::NameInvalid)
    }
}

/// The content hash of a process text: parse it and hash the process, as the model does.
///
/// # Errors
/// [`DefinitionError::TooLarge`] over [`MAX_TEXT_BYTES`]; [`DefinitionError::InputInvalid`] when
/// the text does not parse.
pub fn content_hash(text: &str) -> Result<String> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(DefinitionError::TooLarge {
            field: "text",
            len: text.len(),
            max: MAX_TEXT_BYTES,
        });
    }
    let version = from_text(text).map_err(|_| DefinitionError::InputInvalid { field: "text" })?;
    Ok(ContentHash::of(&version).as_hex().to_owned())
}

/// Everything a store checks before it writes a published version, in one place so every
/// backend refuses the same inputs the same way. Returns the content hash.
///
/// # Errors
/// [`DefinitionError::NameInvalid`], [`DefinitionError::TooLarge`] or
/// [`DefinitionError::InputInvalid`] (empty publisher, a note with a NUL, text that does not
/// parse or names another process).
pub fn check_publish(
    name: &str,
    text: &str,
    publisher: &str,
    note: Option<&str>,
) -> Result<String> {
    check_name(name)?;
    if publisher.is_empty() || publisher.contains('\0') {
        return Err(DefinitionError::InputInvalid { field: "publisher" });
    }
    if publisher.len() > MAX_LABEL_BYTES {
        return Err(DefinitionError::TooLarge {
            field: "publisher",
            len: publisher.len(),
            max: MAX_LABEL_BYTES,
        });
    }
    if let Some(note) = note {
        if note.contains('\0') {
            return Err(DefinitionError::InputInvalid { field: "note" });
        }
        if note.len() > MAX_LABEL_BYTES {
            return Err(DefinitionError::TooLarge {
                field: "note",
                len: note.len(),
                max: MAX_LABEL_BYTES,
            });
        }
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(DefinitionError::TooLarge {
            field: "text",
            len: text.len(),
            max: MAX_TEXT_BYTES,
        });
    }
    // PostgreSQL text cannot hold a NUL; refuse it in every store alike.
    if text.contains('\0') {
        return Err(DefinitionError::InputInvalid { field: "text" });
    }
    let parsed = from_text(text).map_err(|_| DefinitionError::InputInvalid { field: "text" })?;
    if parsed.name != name {
        return Err(DefinitionError::InputInvalid { field: "text" });
    }
    Ok(ContentHash::of(&parsed).as_hex().to_owned())
}
