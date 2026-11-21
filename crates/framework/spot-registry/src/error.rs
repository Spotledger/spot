use std::fmt;

/// A refusal with a stable registry, bundle or signing code. Dependency codes pass through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryError {
    /// Machine-readable refusal code.
    pub code: &'static str,
    /// Human explanation; never includes secret material.
    pub message: String,
}
impl RegistryError {
    /// Constructs an adapter refusal.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for RegistryError {}
impl From<std::io::Error> for RegistryError {
    fn from(e: std::io::Error) -> Self {
        Self::new("registry.storage", e.to_string())
    }
}
impl From<spot_bundle_signing::SigningError> for RegistryError {
    fn from(e: spot_bundle_signing::SigningError) -> Self {
        Self::new(e.code(), e.to_string())
    }
}
/// Registry operation result.
pub type Result<T> = std::result::Result<T, RegistryError>;
pub(crate) fn poisoned() -> RegistryError {
    RegistryError::new("registry.storage", "storage lock poisoned")
}
pub(crate) fn bundle_error(errors: Vec<spot_bundle::BundleError>) -> RegistryError {
    let code = errors.first().map_or("registry.invalid_bundle", |e| e.code);
    RegistryError::new(
        code,
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    )
}
