//! Site identifiers (ADR-34: one tenant = one site = one logical database).

use std::fmt;
use std::str::FromStr;

/// The name of one site, which is also its default host name, for example `acme.spotledger.app`
/// or `localhost`.
///
/// A valid site id is a lower-case DNS host name: 1 to 253 bytes of dot-separated labels, each
/// 1 to 63 bytes of `a-z`, `0-9` and `-`, not starting or ending with `-`. Upper-case input is
/// rejected, not folded, so a site id compares exactly. It is safe to use as a directory name,
/// a cache key prefix and a log field without escaping.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SiteId(String);

/// Why a string is not a valid [`SiteId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidSiteId {
    /// The rejected input.
    pub input: String,
    /// Plain-language reason.
    pub reason: &'static str,
}

impl fmt::Display for InvalidSiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid site id {:?}: {}", self.input, self.reason)
    }
}

impl std::error::Error for InvalidSiteId {}

impl SiteId {
    /// Validates and wraps a site name.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidSiteId> {
        let name = name.into();
        match check(&name) {
            Ok(()) => Ok(Self(name)),
            Err(reason) => Err(InvalidSiteId {
                input: name,
                reason,
            }),
        }
    }

    /// The site name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn check(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("it is empty");
    }
    if name.len() > 253 {
        return Err("it is longer than 253 bytes");
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err("it has an empty label");
        }
        if label.len() > 63 {
            return Err("a label is longer than 63 bytes");
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("only a-z, 0-9, '-' and '.' are allowed");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("a label starts or ends with '-'");
        }
    }
    Ok(())
}

impl fmt::Display for SiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SiteId {
    type Err = InvalidSiteId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for SiteId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::SiteId;

    #[test]
    fn accepts_host_names() {
        for ok in ["localhost", "acme.spotledger.app", "a", "x-1.b2", "127"] {
            assert_eq!(SiteId::new(ok).map(|s| s.to_string()).as_deref(), Ok(ok));
        }
    }

    #[test]
    fn rejects_everything_else() {
        let long_label = "a".repeat(64);
        let long_name = ["a"; 128].join("."); // 255 bytes
        for bad in [
            "",
            "Acme",
            "acme..app",
            ".acme",
            "acme.",
            "-acme",
            "acme-",
            "ac_me",
            "ac me",
            "../etc",
            "acme:8000",
            long_label.as_str(),
            long_name.as_str(),
        ] {
            assert!(SiteId::new(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(SiteId::new("a".repeat(63)).is_ok());
        assert!(SiteId::new(["a"; 127].join(".")).is_ok()); // exactly 253 bytes
    }
}
