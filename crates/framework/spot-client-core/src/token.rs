//! The session token: held in memory only.
//!
//! The type has no `Serialize`, no `Clone`, no `Display` and a redacting `Debug`, so it cannot
//! reach a log, a stored queue or an emitted event by accident. The one way out is
//! [`SessionToken::bearer_header`], used when the core builds an outgoing request.

use std::fmt;

/// An opaque session token. Memory only; see the module documentation.
pub struct SessionToken(String);

impl SessionToken {
    /// Wraps the token string the server returned.
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    /// The `Authorization` header value for an outgoing request.
    pub(crate) fn bearer_header(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::SessionToken;

    #[test]
    fn debug_never_shows_the_token() {
        let token = SessionToken::new("tok-secret".to_owned());
        assert!(!format!("{token:?}").contains("tok-secret"));
        assert_eq!(token.bearer_header(), "Bearer tok-secret");
    }
}
