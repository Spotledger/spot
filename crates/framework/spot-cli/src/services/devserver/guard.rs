//! The refusals of the development server that need no running host, so they are tested alone:
//! loopback only, a path that leaves the app directory, the settings.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// `[services.devserver]`. Unknown keys are refused. Off by default: a site that does not set
/// `enabled = true` registers nothing, so the binary runs no watcher thread.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Turns hot reload on. Only for a developer's machine: every listener must be loopback.
    pub enabled: bool,
    /// The read-only status endpoint (`GET /dev/status`); loopback only.
    pub status_addr: String,
    /// How long a change must be quiet before it is applied, in milliseconds (10 to 10,000).
    pub debounce_ms: u64,
    /// How often the watcher is drained, in milliseconds (10 to 10,000).
    pub poll_ms: u64,
    /// How many reloads the status endpoint remembers (1 to 1,000).
    pub history: usize,
    /// The program a component change runs (`cargo`); unset disables component rebuilds.
    pub build: Option<String>,
    /// The arguments of the build program.
    pub build_args: Vec<String>,
    /// The component artefact a successful build loads, relative to the app directory.
    pub artifact: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            status_addr: "127.0.0.1:7311".to_owned(),
            debounce_ms: 200,
            poll_ms: 250,
            history: 100,
            build: None,
            build_args: Vec::new(),
            artifact: "target/wasm32-wasip1/release/app.wasm".to_owned(),
        }
    }
}

/// The settings once checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The status endpoint address (loopback).
    pub status_addr: SocketAddr,
    /// The debounce window.
    pub debounce: std::time::Duration,
    /// The poll interval.
    pub poll: std::time::Duration,
}

/// Refuses a start that is not safe for development: every `server.bind` address and the status
/// endpoint must be loopback, and the numbers must be in range.
///
/// # Errors
///
/// A sentence naming the setting, for an operator.
pub fn resolve(settings: &Settings, binds: &[SocketAddr]) -> Result<Resolved, String> {
    if let Some(bad) = binds.iter().find(|a| !a.ip().is_loopback()) {
        return Err(format!(
            "services.devserver is for development only and needs every server.bind address to be loopback; {bad} is not"
        ));
    }
    let status_addr: SocketAddr = settings
        .status_addr
        .parse()
        .map_err(|_| "services.devserver.status_addr is not a host:port address".to_owned())?;
    if !status_addr.ip().is_loopback() {
        return Err("services.devserver.status_addr must be a loopback address".to_owned());
    }
    if !(10..=10_000).contains(&settings.debounce_ms) {
        return Err("services.devserver.debounce_ms must be between 10 and 10000".to_owned());
    }
    if !(10..=10_000).contains(&settings.poll_ms) {
        return Err("services.devserver.poll_ms must be between 10 and 10000".to_owned());
    }
    if !(1..=1_000).contains(&settings.history) {
        return Err("services.devserver.history must be between 1 and 1000".to_owned());
    }
    if settings.build.is_none() && !settings.build_args.is_empty() {
        return Err(
            "services.devserver.build_args is set without services.devserver.build".to_owned(),
        );
    }
    Ok(Resolved {
        status_addr,
        debounce: std::time::Duration::from_millis(settings.debounce_ms),
        poll: std::time::Duration::from_millis(settings.poll_ms),
    })
}

/// Whether `path` stays inside `app_dir` once every symlink is resolved. A path that no longer
/// exists (a removed file) is judged by its nearest existing parent. A symlink that leaves the
/// app directory is never followed: the change is ignored.
#[must_use]
pub fn stays_inside(app_dir: &Path, path: &Path) -> bool {
    let Ok(root) = app_dir.canonicalize() else {
        return false;
    };
    let mut probe: PathBuf = path.to_path_buf();
    loop {
        if let Ok(real) = probe.canonicalize() {
            return real.starts_with(&root);
        }
        if !probe.pop() {
            return false;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]
mod tests {
    use super::*;

    fn loopback() -> Vec<SocketAddr> {
        vec!["127.0.0.1:8080".parse().unwrap()]
    }

    #[test]
    fn defaults_are_off_and_loopback() {
        let settings = Settings::default();
        assert!(!settings.enabled);
        let resolved = resolve(&settings, &loopback()).unwrap();
        assert!(resolved.status_addr.ip().is_loopback());
    }

    #[test]
    fn a_non_loopback_bind_or_status_address_is_refused() {
        let public: Vec<SocketAddr> = vec!["0.0.0.0:8080".parse().unwrap()];
        assert!(resolve(&Settings::default(), &public).is_err());
        let mut settings = Settings::default();
        settings.status_addr = "0.0.0.0:7311".to_owned();
        assert!(resolve(&settings, &loopback()).is_err());
        settings.status_addr = "not an address".to_owned();
        assert!(resolve(&settings, &loopback()).is_err());
    }

    #[test]
    fn numbers_out_of_range_are_refused() {
        let mut settings = Settings::default();
        settings.poll_ms = 5;
        assert!(resolve(&settings, &loopback()).is_err());
        let mut settings = Settings::default();
        settings.history = 0;
        assert!(resolve(&settings, &loopback()).is_err());
        let mut settings = Settings::default();
        settings.build_args = vec!["build".to_owned()];
        assert!(resolve(&settings, &loopback()).is_err());
    }

    #[test]
    fn a_path_inside_stays_and_a_removed_file_is_judged_by_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("doctypes")).unwrap();
        std::fs::write(dir.path().join("doctypes/a.toml"), "x").unwrap();
        assert!(stays_inside(
            dir.path(),
            &dir.path().join("doctypes/a.toml")
        ));
        assert!(stays_inside(
            dir.path(),
            &dir.path().join("doctypes/gone.toml")
        ));
        assert!(!stays_inside(dir.path(), Path::new("/")));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_leaves_the_directory_is_not_followed() {
        let app = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.toml"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), app.path().join("doctypes")).unwrap();
        assert!(!stays_inside(
            app.path(),
            &app.path().join("doctypes/secret.toml")
        ));
    }
}
