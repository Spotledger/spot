//! What a debug session must prove before it may exist.
//!
//! Debugging is a **development-site** capability, not a product feature:
//!
//! - it refuses to start for a site marked production, and in a release build;
//! - it requires the development token the site was started with;
//! - it never listens on a non-loopback interface unless the caller explicitly allows it
//!   ([`DebugPolicy::allow_remote`]), and even then the token still applies.
//!
//! The pause timeout lives here too: it is the debugger's promise that a script nobody continues
//! is resumed, so a forgotten breakpoint cannot hold a worker forever.

use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use spot_core::SiteId;

use crate::error::{DevToolsError, Result};

/// The shortest development token accepted. A short one is a guess away from open.
pub const MIN_TOKEN_LEN: usize = 16;

/// The default time a paused script waits for a debugger command before it is resumed.
pub const DEFAULT_PAUSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The development token a site started with. Printed once by `spot dev`; never logged here.
#[derive(Clone, PartialEq, Eq)]
pub struct DevToken(String);

impl std::fmt::Debug for DevToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DevToken(***)")
    }
}

impl DevToken {
    /// A token of at least [`MIN_TOKEN_LEN`] characters.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Config`] when the token is too short.
    pub fn new(token: &str) -> Result<Self> {
        if token.len() < MIN_TOKEN_LEN {
            return Err(DevToolsError::Config {
                message: format!("the development token needs at least {MIN_TOKEN_LEN} characters"),
            });
        }
        Ok(Self(token.to_owned()))
    }

    /// Whether `given` is the token. The comparison takes the same time for every input, so a
    /// wrong guess teaches nothing about how wrong it was.
    #[must_use]
    pub fn matches(&self, given: &str) -> bool {
        let (left, right) = (self.0.as_bytes(), given.as_bytes());
        let mut difference = u8::from(left.len() != right.len());
        let longest = left.len().max(right.len());
        for at in 0..longest {
            let a = left.get(at).copied().unwrap_or(0);
            let b = right.get(at).copied().unwrap_or(0);
            difference |= a ^ b;
        }
        difference == 0
    }

    /// Never print the token. Present so a caller cannot accidentally log one by formatting a
    /// configuration; the value is reached only through [`DevToken::matches`].
    #[must_use]
    pub fn redacted(&self) -> &'static str {
        "***"
    }
}

/// The rules a debug server runs under.
#[derive(Debug, Clone)]
pub struct DebugPolicy {
    /// The token every `launch`/`attach` must present.
    pub token: DevToken,
    /// Whether binding a non-loopback address is explicitly allowed. Default `false`.
    pub allow_remote: bool,
    /// How long a paused script waits for a command before it is resumed. Default
    /// [`DEFAULT_PAUSE_TIMEOUT`].
    pub pause_timeout: Duration,
}

impl DebugPolicy {
    /// A local-only policy with the default pause timeout, for `token`.
    #[must_use]
    pub fn new(token: DevToken) -> Self {
        Self {
            token,
            allow_remote: false,
            pause_timeout: DEFAULT_PAUSE_TIMEOUT,
        }
    }

    /// Allows (or stops allowing) a non-loopback bind. The token still guards every session.
    #[must_use]
    pub fn with_remote(mut self, allow: bool) -> Self {
        self.allow_remote = allow;
        self
    }

    /// The pause timeout.
    #[must_use]
    pub fn with_pause_timeout(mut self, timeout: Duration) -> Self {
        self.pause_timeout = timeout;
        self
    }

    /// Refuses a debug session for a production site.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Production`].
    pub fn check_site(&self, site: &SiteId, production: bool) -> Result<()> {
        if production {
            return Err(DevToolsError::Production {
                site: site.as_str().to_owned(),
            });
        }
        Ok(())
    }

    /// Refuses the debugger in a release build.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::ReleaseBuild`].
    pub fn check_build(&self) -> Result<()> {
        if !cfg!(debug_assertions) {
            return Err(DevToolsError::ReleaseBuild);
        }
        Ok(())
    }

    /// Checks a bind address: loopback is always allowed, anything else only with
    /// [`DebugPolicy::allow_remote`].
    ///
    /// # Errors
    ///
    /// [`DevToolsError::NotLoopback`] for a remote address that was not explicitly allowed.
    pub fn check_address(&self, address: SocketAddr) -> Result<()> {
        if self.allow_remote || is_loopback(address.ip()) {
            return Ok(());
        }
        Err(DevToolsError::NotLoopback {
            address: address.to_string(),
        })
    }

    /// Verifies a presented token.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Token`].
    pub fn check_token(&self, presented: &str) -> Result<()> {
        if self.token.matches(presented) {
            Ok(())
        } else {
            Err(DevToolsError::Token)
        }
    }
}

/// Whether an address is this machine only. IPv4 and IPv6 loopback (including `::1`) and the
/// IPv4-mapped form of `127.0.0.1` count; everything else, including `0.0.0.0` and `::`, does not.
#[must_use]
pub fn is_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    }
}

/// The largest `program` file a launch reads. A debug script is a source file, not a data dump.
pub const MAX_PROGRAM_BYTES: u64 = 1024 * 1024;

fn refused(why: &str) -> DevToolsError {
    DevToolsError::Config {
        message: format!("`program` is refused: {why}"),
    }
}

/// Resolves a `program` path against the configured `root`.
///
/// Refused, before the filesystem is touched: an empty path, a NUL byte, an absolute path (a
/// leading separator, a drive or UNC prefix) and any `..` component (either separator). Then the
/// root and the path are canonicalised, so symlinks are followed, and the result must still be
/// inside the root. The root must exist and must not be the filesystem root. Messages never name
/// the resolved path.
///
/// # Errors
///
/// [`DevToolsError::Config`].
pub fn resolve_program(root: &Path, program: &str) -> Result<PathBuf> {
    if program.is_empty() {
        return Err(refused("it is empty"));
    }
    if program.contains('\0') {
        return Err(refused("it contains a NUL byte"));
    }
    let bytes = program.as_bytes();
    let drive = bytes.get(1) == Some(&b':') && bytes.first().is_some_and(u8::is_ascii_alphabetic);
    if program.starts_with(['/', '\\']) || drive || Path::new(program).has_root() {
        return Err(refused("it must be relative to the program root"));
    }
    if program.split(['/', '\\']).any(|part| part == "..") {
        return Err(refused("it contains a `..` component"));
    }
    let root = root.canonicalize().map_err(|_| DevToolsError::Config {
        message: "the configured program root cannot be resolved".into(),
    })?;
    if root.parent().is_none() {
        return Err(DevToolsError::Config {
            message: "the program root must not be the filesystem root".into(),
        });
    }
    let resolved = root
        .join(program)
        .canonicalize()
        .map_err(|_| refused("it does not name a file under the program root"))?;
    if !resolved.starts_with(&root) {
        return Err(refused("it resolves outside the program root"));
    }
    Ok(resolved)
}

/// Reads the `program` file under `root` (see [`resolve_program`]), at most
/// [`MAX_PROGRAM_BYTES`], as UTF-8. With no configured root every program launch is refused:
/// there is no default of `/`.
///
/// # Errors
///
/// [`DevToolsError::Config`].
pub fn read_program(root: Option<&Path>, program: &str) -> Result<String> {
    let root = root.ok_or_else(|| DevToolsError::Config {
        message: "launching a `program` needs a configured program root".into(),
    })?;
    let path = resolve_program(root, program)?;
    let unreadable = || refused("it cannot be read as a text file");
    let file = std::fs::File::open(&path).map_err(|_| unreadable())?;
    let metadata = file.metadata().map_err(|_| unreadable())?;
    if !metadata.is_file() {
        return Err(refused("it is not a regular file"));
    }
    if metadata.len() > MAX_PROGRAM_BYTES {
        return Err(refused("it is larger than the program size limit"));
    }
    let mut text = String::new();
    file.take(MAX_PROGRAM_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| unreadable())?;
    if text.len() as u64 > MAX_PROGRAM_BYTES {
        return Err(refused("it is larger than the program size limit"));
    }
    Ok(text)
}

/// The address a debug server listens on when it is not stdio.
pub const DEFAULT_DEBUG_ADDRESS: SocketAddr =
    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9229);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{DebugPolicy, DevToken, is_loopback};

    fn policy() -> DebugPolicy {
        DebugPolicy::new(DevToken::new("dev-token-0123456789").expect("long enough"))
    }

    #[test]
    fn the_token_round_trips_and_is_never_printed() {
        let token = DevToken::new("dev-token-0123456789").expect("long enough");
        assert!(token.matches("dev-token-0123456789"));
        assert!(!token.matches("dev-token-012345678"));
        assert!(!token.matches("dev-token-01234567890"));
        assert_eq!(format!("{token:?}"), "DevToken(***)");
        assert_eq!(token.redacted(), "***");
        assert!(DevToken::new("short").is_err());
        assert_eq!(
            DevToken::new("short").unwrap_err().code(),
            "devtools.config"
        );
    }

    #[test]
    fn only_loopback_binds_without_an_explicit_opt_in() {
        let p = policy();
        assert!(p.check_address("127.0.0.1:9229".parse().unwrap()).is_ok());
        assert!(p.check_address("[::1]:9229".parse().unwrap()).is_ok());
        assert_eq!(
            p.check_address("0.0.0.0:9229".parse().unwrap())
                .unwrap_err()
                .code(),
            "devtools.not_loopback"
        );
        assert!(
            p.clone()
                .with_remote(true)
                .check_address("0.0.0.0:9229".parse().unwrap())
                .is_ok()
        );
        assert!(is_loopback(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_loopback(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_loopback(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
    }

    #[test]
    fn production_and_release_are_refused() {
        let p = policy();
        let site = spot_core::SiteId::new("acme.example").unwrap();
        assert!(p.check_site(&site, false).is_ok());
        assert_eq!(
            p.check_site(&site, true).unwrap_err().code(),
            "devtools.production"
        );
        if cfg!(debug_assertions) {
            assert!(p.check_build().is_ok());
        }
        assert_eq!(p.check_token("nope").unwrap_err().code(), "devtools.token");
        assert!(p.check_token("dev-token-0123456789").is_ok());
    }
}
