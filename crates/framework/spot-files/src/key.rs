//! [`FileKey`]: a validated relative path for stored bytes.
//!
//! Keys are built by the caller — this crate never invents a path from a user-supplied file
//! name (WP-PLT-10.T1 "must not"). The validation is what makes the local provider's path
//! containment true by construction: no valid key can escape its site directory. Keys are
//! lower-case only (lead review of #423): on a case-insensitive filesystem (NTFS, ADR-19)
//! keys that differ only in case would be one file, so upper case is rejected, never folded.

use std::fmt;

use crate::error::FileError;

/// Whether a file's bytes are private to the site or publicly readable. The first segment of
/// every [`FileKey`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Visibility {
    /// `private/` — attachment bytes for authorised users only (checked elsewhere).
    Private,
    /// `public/` — asset bytes a public page may reference.
    Public,
}

impl Visibility {
    /// The key segment for this visibility.
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Public => "public",
        }
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The longest full key, in bytes.
pub const MAX_KEY_BYTES: usize = 1024;
/// The longest one segment, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 255;

/// A validated, `/`-separated relative key, for example `private/2026/09/invoice.pdf`.
///
/// Rules (all enforced by [`FileKey::new`] and [`FileKey::parse`]):
/// - total length 1..=[`MAX_KEY_BYTES`] bytes;
/// - segments separated by `/`, each 1..=[`MAX_SEGMENT_BYTES`] bytes of `a-z 0-9 . _ -`
///   (lower-case only: upper case is rejected, never folded — lead review of #423);
/// - no empty segments and no leading `/`;
/// - the segments `.` and `..` are rejected, as is any segment starting with `.` or ending
///   with `.` or a space;
/// - Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`..`COM9`,
///   `LPT1`..`LPT9`) are rejected in any case, with or without an extension.
///
/// The first segment is the [`Visibility`], taken separately at construction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileKey {
    /// Full key text, `visibility/rest`; ASCII by validation.
    text: String,
    /// Number of bytes in the visibility segment + `/`, so the rest can be re-derived.
    vis_len: usize,
}

impl FileKey {
    /// Builds `visibility/rest`. `rest` is `/`-separated and must hold at least one segment;
    /// every segment is validated per the type's rules.
    pub fn new(visibility: Visibility, rest: &str) -> Result<Self, FileError> {
        reject(rest)?;
        let mut text = String::with_capacity(visibility.as_str().len() + 1 + rest.len());
        text.push_str(visibility.as_str());
        text.push('/');
        text.push_str(rest);
        // The visibility segment itself can never trip the checks (it is `private`/`public`),
        // so validating the full string validates exactly the rest plus the total length.
        check_key(&text)?;
        Ok(Self {
            text,
            vis_len: visibility.as_str().len() + 1,
        })
    }

    /// Parses a full key text, validating its first segment as the visibility.
    pub fn parse(text: &str) -> Result<(Visibility, Self), FileError> {
        for vis in [Visibility::Private, Visibility::Public] {
            if let Some(rest) = text
                .strip_prefix(vis.as_str())
                .and_then(|t| t.strip_prefix('/'))
            {
                let key = Self::new(vis, rest)?;
                return Ok((vis, key));
            }
        }
        Err(FileError::InvalidKey {
            reason: "the first segment must be `private` or `public`",
        })
    }

    /// The visibility segment.
    pub fn visibility(&self) -> Visibility {
        if self.text.starts_with(Visibility::Private.as_str()) {
            Visibility::Private
        } else {
            Visibility::Public
        }
    }

    /// The key without its visibility segment.
    pub fn rest(&self) -> &str {
        &self.text[self.vis_len..]
    }

    /// The full key text, `/`-separated.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// True when `self` starts at `prefix` on segment boundaries: either the prefix is empty
    /// or `self` equals it, or `self` continues it with a `/`.
    pub fn starts_with(&self, prefix: &FileKeyPrefix) -> bool {
        prefix.segments.is_empty()
            || self.text == prefix.segments
            || self.text.starts_with(&format!("{}/", prefix.segments))
    }
}

impl fmt::Display for FileKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// A validated prefix used by `FileStore::list`: zero or more whole segments that keys must
/// start at a segment boundary. An empty prefix lists the whole site.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileKeyPrefix {
    /// The prefix text without a trailing `/`; empty means "everything".
    segments: String,
}

impl FileKeyPrefix {
    /// The prefix that matches every key in the site.
    pub fn all() -> Self {
        Self {
            segments: String::new(),
        }
    }

    /// Builds a prefix from `/`-separated segments, validated like [`FileKey`] segments but
    /// allowing an empty prefix.
    pub fn new(text: &str) -> Result<Self, FileError> {
        if text.is_empty() {
            return Ok(Self::all());
        }
        check_segments(text)?;
        Ok(Self {
            segments: text.to_string(),
        })
    }

    /// The prefix text, no trailing `/`.
    pub fn as_str(&self) -> &str {
        &self.segments
    }
}

impl fmt::Display for FileKeyPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.segments)
    }
}

/// Rejects input a caller clearly did not mean as key segments: an empty rest and a
/// leading `/` are checked before the whole key is assembled.
fn reject(rest: &str) -> Result<(), FileError> {
    if rest.is_empty() {
        return Err(FileError::InvalidKey {
            reason: "the key needs a segment after the visibility",
        });
    }
    if rest.starts_with('/') {
        return Err(FileError::InvalidKey {
            reason: "a key must not start with '/'",
        });
    }
    Ok(())
}

/// Validates the assembled full key: total size plus every segment.
fn check_key(text: &str) -> Result<(), FileError> {
    if text.len() > MAX_KEY_BYTES {
        return Err(FileError::InvalidKey {
            reason: "the key is longer than 1024 bytes",
        });
    }
    check_segments(text)
}

/// Validates `/`-separated segments: charset, size, dot rules and Windows reserved names.
fn check_segments(text: &str) -> Result<(), FileError> {
    for seg in text.split('/') {
        check_segment(seg)?;
    }
    Ok(())
}

fn check_segment(seg: &str) -> Result<(), FileError> {
    if seg.is_empty() {
        return Err(FileError::InvalidKey {
            reason: "a segment is empty",
        });
    }
    if seg.len() > MAX_SEGMENT_BYTES {
        return Err(FileError::InvalidKey {
            reason: "a segment is longer than 255 bytes",
        });
    }
    if seg == "." || seg == ".." {
        return Err(FileError::InvalidKey {
            reason: "`..` and `.` segments are rejected",
        });
    }
    if seg.starts_with('.') {
        return Err(FileError::InvalidKey {
            reason: "a segment must not start with '.'",
        });
    }
    if seg.ends_with('.') || seg.ends_with(' ') {
        return Err(FileError::InvalidKey {
            reason: "a segment must not end with '.' or a space",
        });
    }
    if !seg.bytes().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'-'
    }) {
        return Err(FileError::InvalidKey {
            reason: "only a-z 0-9 . _ - are allowed in a segment; upper case is rejected, never folded",
        });
    }
    if is_windows_reserved(seg) {
        return Err(FileError::InvalidKey {
            reason: "the segment is a Windows reserved device name",
        });
    }
    Ok(())
}

/// True when any segment is (or, via its extension part, names) a Windows device: compare
/// the base name before the first dot, case-insensitively (ADR-19).
fn is_windows_reserved(seg: &str) -> bool {
    let base = seg.split('.').next().unwrap_or_default();
    let up = base.to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    RESERVED.contains(&up.as_str())
}
