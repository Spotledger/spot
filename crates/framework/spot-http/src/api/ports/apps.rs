//! The installed-apps seam of native API 0.25.0 (`app_list`, WP-EXT-05.T3).
//!
//! `spot-http` does not depend on the app runtime. It defines [`AppsPort`]; the adapter over
//! the registry of `spot-cli`'s `services/apps.rs` (WP-EXT-05.T2) is a host task.
//! [`MemoryAppsPort`] is the fake the tests use.
//!
//! The port decides who may see the list (the administrator right, `apps.read`) and which
//! tenant's apps are shown; the route only shapes. A view has no field for bundle contents, a
//! key or a secret, and a failure is a stable code, never a stack trace.

use std::sync::Mutex;

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`AppsPort`].
pub const KEY: PortKey = PortKey::of::<dyn AppsPort>();

/// An installed app's state (`InstalledApp.state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppState {
    /// Installed and enabled.
    Installed,
    /// Switched off.
    Disabled,
    /// Failed verification or loading (see `last_error_code`).
    Failed,
}

impl AppState {
    /// The contract's token.
    pub fn as_str(self) -> &'static str {
        match self {
            AppState::Installed => "installed",
            AppState::Disabled => "disabled",
            AppState::Failed => "failed",
        }
    }
}

/// The contract's `InstalledApp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppView {
    /// The app's name (at most 100 characters).
    pub name: String,
    /// Its version (at most 40 characters).
    pub version: String,
    /// The bundle digest (at most 128 characters).
    pub digest: String,
    /// The state.
    pub state: AppState,
    /// The publisher.
    pub publisher: Option<String>,
    /// The capabilities an administrator granted.
    pub capabilities_granted: Vec<String>,
    /// The capabilities the app asked for.
    pub capabilities_requested: Vec<String>,
    /// Whether the app is loaded in the runtime now.
    pub loaded: bool,
    /// The stable code of the last failure.
    pub last_error_code: Option<String>,
}

/// One page of apps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPage {
    /// The apps, at most `limit`.
    pub apps: Vec<AppView>,
    /// The next cursor; `None` on the last page.
    pub next_cursor: Option<String>,
}

/// Why the list was refused. No variant carries text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppsPortError {
    /// The caller lacks the administrator right.
    PermissionDenied,
    /// The cursor is not one the port issued.
    CursorInvalid,
    /// The credential is missing, expired or revoked.
    Unauthenticated,
    /// The service is temporarily unavailable.
    Unavailable,
    /// Anything else.
    Internal,
}

/// The installed apps of one site.
pub trait AppsPort: Send + Sync {
    /// The apps of the caller's site, by name; `limit` is 1 to 100.
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<AppPage, AppsPortError>>;
}

#[derive(Default)]
struct State {
    apps: Vec<(String, AppView)>,
    admins: Vec<(String, String)>,
    users: Vec<(String, String)>,
}

/// The in-memory [`AppsPort`]: the caller is the credential's token itself; an administrator is
/// registered with [`MemoryAppsPort::grant_admin`], any other known user with
/// [`MemoryAppsPort::grant_user`].
#[derive(Default)]
pub struct MemoryAppsPort {
    state: Mutex<State>,
}

impl MemoryAppsPort {
    /// An empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Adds an app on `site`.
    pub fn add_app(&self, site: &str, app: AppView) {
        self.lock().apps.push((site.to_owned(), app));
    }

    /// `token` is an administrator of `site`.
    pub fn grant_admin(&self, site: &str, token: &str) {
        self.lock().admins.push((site.to_owned(), token.to_owned()));
    }

    /// `token` is an ordinary user of `site`.
    pub fn grant_user(&self, site: &str, token: &str) {
        self.lock().users.push((site.to_owned(), token.to_owned()));
    }

    /// An installed, loaded app with no capability, for seeding.
    pub fn simple(name: &str) -> AppView {
        AppView {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            digest: "sha256:00".to_owned(),
            state: AppState::Installed,
            publisher: Some("acme".to_owned()),
            capabilities_granted: Vec::new(),
            capabilities_requested: Vec::new(),
            loaded: true,
            last_error_code: None,
        }
    }
}

impl AppsPort for MemoryAppsPort {
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<AppPage, AppsPortError>> {
        Box::pin(async move {
            let state = self.lock();
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .ok_or(AppsPortError::Unauthenticated)?;
            let site = ctx.site.as_str().to_owned();
            let key = (site.clone(), token);
            if state.admins.contains(&key) {
                // fall through
            } else if state.users.contains(&key) {
                return Err(AppsPortError::PermissionDenied);
            } else {
                return Err(AppsPortError::Unauthenticated);
            }
            let mut apps: Vec<&AppView> = state
                .apps
                .iter()
                .filter(|(s, _)| *s == site)
                .map(|(_, a)| a)
                .collect();
            apps.sort_by(|a, b| a.name.cmp(&b.name));
            let start = match cursor {
                None => 0,
                Some(c) => c
                    .strip_prefix('o')
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n <= apps.len())
                    .ok_or(AppsPortError::CursorInvalid)?,
            };
            let end = (start + limit).min(apps.len());
            Ok(AppPage {
                apps: apps[start..end].iter().map(|a| (*a).clone()).collect(),
                next_cursor: (end < apps.len()).then(|| format!("o{end}")),
            })
        })
    }
}
