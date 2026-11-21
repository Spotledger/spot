//! The two identities AI admission is scoped by: the agent and the tenant.
//!
//! Both are validated the same way as a coordinator name: non-empty, at most
//! [`MAX_IDENTITY_BYTES`] bytes and printable. A scope key is the identity's text, so an
//! identity never carries a secret.

use std::fmt;

use crate::error::{AiError, Result};

/// The longest an agent or tenant identity may be, in bytes.
pub const MAX_IDENTITY_BYTES: usize = 128;

/// A caller's agent identity (`glm`, `support-agent-7`, ...).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(String);

impl AgentId {
    /// Validates and wraps an agent identity.
    ///
    /// # Errors
    ///
    /// [`crate::error::INVALID_ARGUMENT`] when the identity is empty, over
    /// [`MAX_IDENTITY_BYTES`] bytes or not printable.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate(&id, "agent")?;
        Ok(Self(id))
    }

    /// The identity as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tenant identity (`acme`, `acme/C1`, ...).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(String);

impl TenantId {
    /// Validates and wraps a tenant identity.
    ///
    /// # Errors
    ///
    /// [`crate::error::INVALID_ARGUMENT`] when the identity is empty, over
    /// [`MAX_IDENTITY_BYTES`] bytes or not printable.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate(&id, "tenant")?;
        Ok(Self(id))
    }

    /// The identity as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The shared rule: non-empty, at most [`MAX_IDENTITY_BYTES`] bytes, printable.
fn validate(id: &str, what: &str) -> Result<()> {
    if id.is_empty() {
        return Err(AiError::invalid_argument(format!(
            "an {what} id must not be empty"
        )));
    }
    if id.len() > MAX_IDENTITY_BYTES {
        return Err(AiError::invalid_argument(format!(
            "an {what} id is at most {MAX_IDENTITY_BYTES} bytes, this one has {}",
            id.len()
        )));
    }
    if id.chars().any(char::is_control) {
        return Err(AiError::invalid_argument(format!(
            "an {what} id must be printable: it has a control character"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::error::INVALID_ARGUMENT;

    #[test]
    fn identities_validate_length_and_printability() {
        assert!(AgentId::new("glm").is_ok());
        assert_eq!(
            AgentId::new("").err().map(|e| e.code),
            Some(INVALID_ARGUMENT)
        );
        assert_eq!(
            TenantId::new("a\u{7}").err().map(|e| e.code),
            Some(INVALID_ARGUMENT)
        );
        assert!(TenantId::new("x".repeat(MAX_IDENTITY_BYTES)).is_ok());
        assert!(TenantId::new("x".repeat(MAX_IDENTITY_BYTES + 1)).is_err());
    }
}
