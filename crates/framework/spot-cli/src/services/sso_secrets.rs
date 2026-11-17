//! Client secrets for the single sign-on host (WP-IAM-05.T16): [`SsoSecrets`] over references the
//! operator wrote in `[services.sso_identity] client_secrets`.
//!
//! A provider's configuration holds only the **name** of its client secret
//! (`ClientAuth::ClientSecretRef`); the value is read here, at the moment of use, and handed to
//! the host as a [`SecretString`], whose `Debug` is redacted. There is no named-secret store in
//! `spot-secrets` (it seals values and rotates keys; it does not name them), so the reference is
//! to something the operator already protects:
//!
//! - `env:NAME`: the process environment variable `NAME`;
//! - `file:/absolute/path`: a file whose content is the secret (one trailing newline is
//!   dropped). On Unix the file must not be readable by group or others (mode `0600` or
//!   stricter); a looser file is refused, and on other platforms a file reference is refused.
//!
//! Anything that goes wrong (an unknown name, another site, an unset variable, an empty or
//! oversized value, a loose file) is `None`: the host refuses the sign-in, and nothing is logged
//! that names a value. The reader never writes the secret anywhere, and `private_key_jwt` keys are
//! not read here (`spot-sso` refuses that scheme as unavailable until a key reader exists).

use std::collections::BTreeMap;
use std::path::PathBuf;

use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_secrets::SecretString;

use crate::services::sso::SsoSecrets;

/// The longest secret read, in bytes.
const MAX_SECRET: usize = 4096;

/// Whether `name` is a secret name: 1 to 200 of letters, digits, `_`, `.`, `-`.
#[must_use]
pub fn name_ok(name: &str) -> bool {
    (1..=200).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Where a secret's value is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A process environment variable.
    Env(String),
    /// A file, by absolute path.
    File(PathBuf),
}

impl Source {
    /// Parses `env:NAME` or `file:/absolute/path`; `None` for anything else.
    #[must_use]
    pub fn parse(reference: &str) -> Option<Source> {
        if let Some(var) = reference.strip_prefix("env:") {
            let ok = !var.is_empty()
                && var.len() <= 128
                && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            return ok.then(|| Source::Env(var.to_owned()));
        }
        let path = reference.strip_prefix("file:")?;
        let path = PathBuf::from(path);
        (path.is_absolute() && !path.as_os_str().is_empty()).then_some(Source::File(path))
    }

    /// Reads the value. `None` for every failure.
    fn read(&self) -> Option<String> {
        let value = match self {
            Source::Env(var) => std::env::var(var).ok()?,
            Source::File(path) => read_protected(path)?,
        };
        let value = value.strip_suffix('\n').unwrap_or(&value).to_owned();
        (!value.is_empty() && value.len() <= MAX_SECRET).then_some(value)
    }
}

#[cfg(unix)]
fn read_protected(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file()
        || meta.permissions().mode() & 0o077 != 0
        || meta.len() > MAX_SECRET as u64 + 1
    {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(not(unix))]
fn read_protected(_path: &std::path::Path) -> Option<String> {
    None
}

/// [`SsoSecrets`] over the configured references of one site.
pub struct ConfiguredSecrets {
    site: String,
    entries: BTreeMap<String, Source>,
}

impl ConfiguredSecrets {
    /// The reader of `site`. An entry whose reference does not parse is dropped (the settings
    /// validation refuses it before this is reached).
    #[must_use]
    pub fn new(site: &str, references: &BTreeMap<String, String>) -> Self {
        let entries = references
            .iter()
            .filter(|(name, _)| name_ok(name))
            .filter_map(|(name, reference)| Some((name.clone(), Source::parse(reference)?)))
            .collect();
        Self {
            site: site.to_owned(),
            entries,
        }
    }
}

impl SsoSecrets for ConfiguredSecrets {
    fn client_secret<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, Option<SecretString>> {
        Box::pin(async move {
            // A site reads its own references only.
            if ctx.site.as_str() != self.site || !name_ok(name) {
                return None;
            }
            let source = self.entries.get(name)?.clone();
            // Reading a file blocks: off the async workers.
            tokio::task::spawn_blocking(move || source.read())
                .await
                .ok()
                .flatten()
                .map(SecretString::new)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_and_references_are_closed_grammars() {
        assert!(name_ok("okta-prod.client_secret"));
        for bad in ["", "a b", "a/b", "a\u{0}", &"x".repeat(201)] {
            assert!(!name_ok(bad), "{bad:?}");
        }
        assert_eq!(
            Source::parse("env:OKTA_SECRET"),
            Some(Source::Env("OKTA_SECRET".into()))
        );
        assert_eq!(
            Source::parse("file:/etc/spot/okta"),
            Some(Source::File("/etc/spot/okta".into()))
        );
        for bad in [
            "",
            "env:",
            "env:A-B",
            "file:relative/path",
            "file:",
            "http://x",
            "secret",
        ] {
            assert_eq!(Source::parse(bad), None, "{bad:?}");
        }
    }
}
