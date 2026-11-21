//! Redaction: values that must never reach a log line.

use std::fmt;

use http::HeaderMap;

/// Wraps a value so that `Debug` and `Display` print `***`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Redacted<T>(pub T);

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// True if the value of this header must never be logged: `Authorization`, `Cookie`,
/// `Set-Cookie`, `X-Api-Key`, and any name containing `token` or `secret` (any case).
pub fn is_sensitive_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || lower.contains("token")
        || lower.contains("secret")
}

/// A loggable view of a header map: sensitive values print as `***`, and values that are not
/// text print as `<binary>`. Use it with `%` or `?` in a `tracing` field.
pub struct LoggableHeaders<'a>(&'a HeaderMap);

/// Wraps a header map for logging. See [`LoggableHeaders`].
pub fn loggable_headers(headers: &HeaderMap) -> LoggableHeaders<'_> {
    LoggableHeaders(headers)
}

impl fmt::Display for LoggableHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (name, value) in self.0 {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            f.write_str(name.as_str())?;
            f.write_str("=")?;
            if is_sensitive_header(name.as_str()) {
                f.write_str("***")?;
            } else {
                match value.to_str() {
                    Ok(text) => write!(f, "{text:?}")?,
                    Err(_) => f.write_str("<binary>")?,
                }
            }
        }
        Ok(())
    }
}

impl fmt::Debug for LoggableHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
