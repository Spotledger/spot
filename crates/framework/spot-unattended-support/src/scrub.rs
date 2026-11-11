//! Local scrubbing before anything is written to disk or sent: the shared redactor for secrets
//! and personal data, plus a path and user name scrubber for the places a stack trace or a
//! log line names the machine's user.

use spot_redact::{RedactionPolicy, Redactor};

use crate::error::{SupportError, codes};

/// The marker a scrubbed user name or home directory becomes.
pub const USER_MARKER: &str = "<user>";

/// Cleans text of secrets (via `spot-redact`), e-mail addresses and phone numbers, home
/// directories and user names.
pub struct Scrubber {
    redactor: Redactor,
    home_dirs: Vec<String>,
    user_names: Vec<String>,
}

impl Scrubber {
    /// A scrubber for a machine whose users have these home directories and names (both may be
    /// empty; the generic `/home/<name>`, `/Users/<name>` and `C:\Users\<name>` forms are
    /// scrubbed regardless).
    ///
    /// # Errors
    /// `redactor_failed` when the redaction rules cannot be built; nothing is then written,
    /// because an unredacted report must never reach the disk.
    pub fn new(home_dirs: &[String], user_names: &[String]) -> Result<Self, SupportError> {
        let mut policy = RedactionPolicy::default();
        policy.patterns.pii_email = true;
        policy.patterns.pii_phone = true;
        let redactor = Redactor::from_policy(&policy)
            .map_err(|e| SupportError::new(codes::REDACTOR_FAILED, e.to_string()))?;
        let mut home_dirs: Vec<String> =
            home_dirs.iter().filter(|h| h.len() > 1).cloned().collect();
        home_dirs.sort_by_key(|h| std::cmp::Reverse(h.len()));
        Ok(Self {
            redactor,
            home_dirs,
            user_names: user_names
                .iter()
                .filter(|n| n.len() >= 3)
                .cloned()
                .collect(),
        })
    }

    /// The scrubbed text.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for home in &self.home_dirs {
            out = out.replace(home.as_str(), "~");
        }
        out = scrub_user_paths(&out);
        for name in &self.user_names {
            out = out.replace(name.as_str(), USER_MARKER);
        }
        self.redactor.redact_str(&out).into_owned()
    }
}

/// Replaces the user segment of `/home/<name>`, `/Users/<name>` and `C:\Users\<name>` paths.
fn scrub_user_paths(text: &str) -> String {
    let mut out = text.to_owned();
    for prefix in ["/home/", "/Users/", "\\Users\\", "/users/"] {
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(at) = rest.find(prefix) {
            let (head, tail) = rest.split_at(at + prefix.len());
            result.push_str(head);
            let end = tail
                .find(['/', '\\', ' ', '"', '\'', ':', ')', '\n'])
                .unwrap_or(tail.len());
            if tail[..end].is_empty() || &tail[..end] == USER_MARKER {
                rest = tail;
            } else {
                result.push_str(USER_MARKER);
                rest = &tail[end..];
            }
        }
        result.push_str(rest);
        out = result;
    }
    out
}
