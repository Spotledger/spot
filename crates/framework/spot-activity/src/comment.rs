//! Comments with mentions and edit rules (WP-DOC-07.T1).
//!
//! A [`Comment`] is one note on a document. [`parse_mentions`] extracts `@user` tokens with a
//! single left-to-right scan — no regex crate, so nothing can backtrack (ReDoS, WP-SEC-01).
//! Edits follow [`can_edit`]: the author, within a window, against a clock value passed in —
//! never a clock read (blueprint §4: pure functions).
//!
//! A soft-deleted comment keeps its row and id but its body is blanked in the rendered form
//! (`rendered_body`), so threads stay anchored and quoted text disappears.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::{OffsetDateTime, RecordId};

/// One comment on a document.
///
/// Every field is data: `at` is the creation instant, `mentions` the parsed tokens, `deleted`
/// the soft-delete mark. Construct with [`Comment::new`], which runs [`parse_mentions`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// The comment's own record id.
    #[serde(with = "crate::wire::record_id")]
    pub id: RecordId,
    /// The document the comment sits on.
    #[serde(with = "crate::wire::record_id")]
    pub doc: RecordId,
    /// Who wrote it, as the user's storage name (an `@user` mention is the same shape).
    pub author: String,
    /// When it was written (a parameter).
    #[serde(with = "crate::wire::rfc3339")]
    pub at: OffsetDateTime,
    /// The body as written. A deleted comment keeps the body in storage but
    /// [`Comment::rendered_body`] blanks it.
    pub body: String,
    /// The `@user` tokens the body mentions (parsed on construction; unique, in first-seen
    /// order).
    pub mentions: Vec<String>,
    /// When it was last edited, when it was.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::wire::opt_rfc3339"
    )]
    pub edited_at: Option<OffsetDateTime>,
    /// The soft-delete mark: the row stays, the body renders blank.
    pub deleted: bool,
}

impl Comment {
    /// A new comment: mentions parsed once on the way in.
    #[must_use]
    pub fn new(
        id: RecordId,
        doc: RecordId,
        author: &str,
        at: OffsetDateTime,
        body: &str,
    ) -> Comment {
        Comment {
            id,
            doc,
            author: author.to_owned(),
            at,
            body: body.to_owned(),
            mentions: parse_mentions(body),
            edited_at: None,
            deleted: false,
        }
    }

    /// Body shown to a reader: the body, or blank for a soft-deleted comment (the row stays,
    /// quoted text disappears).
    #[must_use]
    pub fn rendered_body(&self) -> &str {
        if self.deleted { "" } else { &self.body }
    }

    /// The comment still counts as present (a deleted one does not).
    #[must_use]
    pub fn is_present(&self) -> bool {
        !self.deleted
    }
}

/// The result of an edit check: allowed, or the reason it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditCheck {
    /// The edit may proceed.
    Allowed,
    /// Only the author edits a comment.
    NotAuthor,
    /// The window passed has closed (`now` is after `at + window`).
    WindowClosed,
    /// A deleted comment never edits again.
    Deleted,
}

impl EditCheck {
    /// True when the edit may proceed.
    #[must_use]
    pub fn is_allowed(self) -> bool {
        self == EditCheck::Allowed
    }
}

/// Whether `user` may edit `comment` at `now`.
///
/// The rules, as the brief fixes them: the author only; within a window; the window is a
/// parameter and `now` is a clock value passed in (a function argument) — this function never
/// reads a clock, so tests are exact and hosts inject their own time.
#[must_use]
pub fn can_edit(comment: &Comment, user: &str, now: OffsetDateTime, window: Duration) -> EditCheck {
    if comment.deleted {
        return EditCheck::Deleted;
    }
    if comment.author != user {
        return EditCheck::NotAuthor;
    }
    // No float arithmetic (ADR-12): the window arrives as a std `Duration`; its whole seconds
    // become a `time::SignedDuration`, saturated at `i64::MAX`.
    let window = time::Duration::new(
        i64::try_from(window.as_secs()).unwrap_or(i64::MAX),
        i32::try_from(window.subsec_nanos()).unwrap_or(0),
    );
    let deadline = comment.at.checked_add(window).unwrap_or(comment.at);
    if now > deadline {
        return EditCheck::WindowClosed;
    }
    EditCheck::Allowed
}

/// Extracts the `@user` tokens of a comment body.
///
/// One left-to-right scan, no regex (nothing can backtrack): a mention is `@` followed by a
/// storage name — `[a-z][a-z0-9_]*`, at most 63 bytes, the same shape a `Name` takes — at a
/// position where a word can start (the text's start, whitespace, or opening punctuation).
/// These are **not** mentions:
///
/// - `name@example.com` — an e-mail address (`@` mid-word);
/// - `user@@host` (`@` immediately after a `@` mid-token), `@` alone;
/// - `@User` or `@user-x` — a capital or a hyphen is not a name character, so the scan stops
///   there; `@user` is taken from `@user-x` (the hyphen ends the token);
/// - punctuation right after the name (`@user, thanks`) — the comma ends the token, the
///   mention stays `user`.
///
/// The result is unique and in first-seen order.
#[must_use]
pub fn parse_mentions(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let started = i == 0 || !is_word_byte(bytes[i - 1]);
        if bytes[i] == b'@' && started {
            // Read the name that follows: ASCII lowercase start, then lowercase/digit/_.
            let mut j = i + 1;
            while j < bytes.len() && is_name_byte(bytes[j]) {
                j += 1;
            }
            let len = j - (i + 1);
            // A name is at least one character and at most 63 bytes (`Name`'s limit).
            if (1..=63).contains(&len) {
                let name = &body[i + 1..j];
                if !out.iter().any(|m| m == name) {
                    out.push(name.to_owned());
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Whether the byte can sit inside the scanned word (a mention starts at a word boundary).
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether the byte can sit in a mention name: `[a-z0-9_]`, first byte `[a-z]`.
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

/// The window the engine passes for comment edits: a parameter, defined here so callers share
/// one constant instead of inventing their own.
pub const EDIT_WINDOW: Duration = Duration::from_secs(15 * 60);
