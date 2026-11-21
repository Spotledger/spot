//! An API key that cannot end up in a log by accident.

use std::fmt;

use spot_secrets::SecretString;
use zeroize::Zeroizing;

/// A provider API key. `Debug` and `Display` print `***` and the bytes are zeroised on drop,
/// so a key handed to a provider constructor cannot leak through formatting, tracing or
/// accident; getting at the value deliberately is one explicit call, [`ApiKey::expose`].
///
/// There is no `serde` implementation — build one with [`ApiKey::new`], [`From`] impls or
/// [`ApiKey::from_secret`], and move it where it is needed.
#[derive(Clone)]
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// Wraps `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Builds the key from a stored [`SecretString`], zeroising nothing it does not own.
    pub fn from_secret(secret: &SecretString) -> Self {
        Self::new(secret.expose())
    }

    /// Borrows the key to build one auth header. Never store or print what this returns.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for ApiKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for ApiKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<&SecretString> for ApiKey {
    fn from(secret: &SecretString) -> Self {
        Self::from_secret(secret)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}
