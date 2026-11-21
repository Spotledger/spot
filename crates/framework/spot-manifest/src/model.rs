//! The typed model of everything an app attaches to the platform, as ADR-17 declares it.
//!
//! Values are typed and already valid: [`HookPoint`]s and [`StepName`]s from the app API
//! contract, `semver` versions and requirements, parsed [`Cron`] schedules, `Duration`
//! budgets and relative file paths. A native app builds a manifest in code; every other
//! runtime gets one from [`parse`](crate::parse). Both then face [`validate`](crate::validate).

use std::fmt;
use std::time::Duration;

use semver::{Version, VersionReq};
use spot_app_api::prelude::Name;
use spot_app_api::{HookPoint, OverrideKind, StepDecl, StepName, StepOverride};

use crate::cron::Cron;
use crate::error::{INVALID_PATH, ManifestError};

/// Everything an app attaches to the platform, in one manifest (ADR-17). The installer
/// consumes it; nothing on the platform runs without being declared here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppManifest {
    /// Identity, versions and dependencies.
    pub app: AppMeta,
    /// Document lifecycle subscriptions (ADR-51).
    pub events: Vec<EventSub>,
    /// Session lifecycle hooks.
    pub session_events: Vec<SessionSub>,
    /// One-time boot data contributions.
    pub boot: Vec<BootSub>,
    /// Recurring jobs.
    pub schedules: Vec<ScheduleSub>,
    /// Overrides of steps (ADR-08).
    pub overrides: Vec<OverrideDecl>,
    /// Service seams the app provides or replaces (ADR-08, WP-EXT-02).
    pub seams: Vec<SeamDecl>,
    /// Whitelisted instance methods (ADR-16).
    pub methods: Vec<MethodSub>,
    /// Whitelisted module-level functions (ADR-16).
    pub functions: Vec<FunctionSub>,
    /// Records loaded into a DocType at install.
    pub fixtures: Vec<FixtureSub>,
    /// Schema or data migrations (ADR-22), listed in the order they run.
    pub patches: Vec<PatchSub>,
    /// HTTP endpoints the app serves.
    pub routes: Vec<RouteSub>,
    /// Static files served under the app's prefix.
    pub assets: Vec<AssetSub>,
    /// What the app may touch.
    pub capabilities: Capabilities,
    /// The named permissions the app declares (WP-PLT-03.T4). Declaring one grants it to
    /// nobody: grants are role data (`spot-permissions`), and a name no app declares is
    /// unknown, hence denied.
    pub permissions: Vec<PermissionSub>,
}

impl AppManifest {
    /// The named permissions this app declares, as the `(name, declaration)` pairs the site's
    /// catalogue is built from: one call per installed app, declaration order kept.
    ///
    /// The declaration carries the manifest's app name and never a grant: whether a role holds
    /// the permission is role data (`spot-permissions`), not something a manifest can say.
    #[must_use]
    pub fn app_permissions(&self) -> Vec<(AppPermissionName, AppPermissionDecl)> {
        let app = self.app.name.as_str().to_owned();
        self.permissions
            .iter()
            .map(|sub| {
                (
                    sub.name.clone(),
                    AppPermissionDecl {
                        app: app.clone(),
                        title: sub.title.clone(),
                        description: sub.description.clone().unwrap_or_default(),
                    },
                )
            })
            .collect()
    }

    /// The `[[events]]` entries as `(doctype, declaration)` pairs for the controllers.
    pub fn step_decls(&self) -> Vec<(String, StepDecl)> {
        self.events
            .iter()
            .map(|sub| {
                (
                    sub.doctype.clone(),
                    StepDecl {
                        name: sub.step.clone(),
                        point: sub.event.clone(),
                        priority: sub.priority,
                        budget: sub.budget,
                    },
                )
            })
            .collect()
    }

    /// The `[[overrides]]` entries as ADR-08 declarations.
    pub fn overrides(&self) -> Vec<StepOverride> {
        self.overrides
            .iter()
            .map(|o| StepOverride {
                target: o.target.clone(),
                kind: o.kind,
                by: o.by.clone(),
                reason: o.reason.clone(),
            })
            .collect()
    }
}

/// The app's identity and what it needs from the platform and from other apps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppMeta {
    /// The app's name; every step it declares is prefixed with it.
    pub name: Name,
    /// Its semver version.
    pub version: Version,
    /// A human title, for the admin.
    pub title: String,
    /// The platform versions this app runs on, when it states one.
    pub requires_host: Option<VersionReq>,
    /// The other apps it builds on.
    pub depends: Vec<AppDependency>,
}

/// One app this app depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDependency {
    /// The app it depends on.
    pub app: Name,
    /// The versions of that app it accepts.
    pub version: VersionReq,
}

/// A document lifecycle subscription (ADR-51): one controller step at one hook point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventSub {
    /// The DocType whose events run the step.
    pub doctype: String,
    /// Where the step runs.
    pub event: HookPoint,
    /// The app's step that runs.
    pub step: StepName,
    /// Lower runs first; defaults to 0.
    pub priority: i32,
    /// The longest the step may take; defaults to 200 ms.
    pub budget: Duration,
}

/// A session lifecycle hook: one controller step around session events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSub {
    /// Which session event runs it.
    pub event: SessionEvent,
    /// The app's step that runs.
    pub step: StepName,
    /// The longest the step may take; defaults to 200 ms.
    pub budget: Duration,
}

/// The session events a manifest can subscribe to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionEvent {
    /// A user logged in.
    Login,
    /// A user logged out.
    Logout,
    /// The platform created a session.
    SessionCreated,
}

/// A step that contributes boot data to the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootSub {
    /// The app's step that runs.
    pub step: StepName,
    /// The longest it may take, when stated; otherwise the host's boot budget applies.
    pub budget: Option<Duration>,
}

/// A recurring job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleSub {
    /// Its name, unique in the manifest.
    pub name: String,
    /// When it fires: six fields, seconds first.
    pub cron: Cron,
    /// The app's step that runs.
    pub step: StepName,
    /// The queue it runs on; defaults to `default`.
    pub queue: Queue,
    /// The longest it may take; defaults to 600 000 ms.
    pub budget: Duration,
}

/// The queue a schedule runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Queue {
    /// The shared default queue.
    Default,
    /// The queue for jobs that must return quickly.
    Short,
    /// The queue for slow, best-effort work.
    Long,
}

/// A declared override of one step (ADR-08): the model of one `[[overrides]]` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverrideDecl {
    /// The step being changed, usually another app's.
    pub target: StepName,
    /// How it is changed.
    pub kind: OverrideKind,
    /// The step that runs instead of or around it; `None` only for `Disable`.
    pub by: Option<StepName>,
    /// Why, for the admin override listing; defaults to empty.
    pub reason: String,
}

/// What a seam is: provided by this app, or replacing another app's (ADR-08, WP-EXT-02).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeamKind {
    /// This app provides the seam.
    Provide,
    /// This app replaces another app's seam.
    Replace,
}

/// One seam this app provides or replaces: `<app>:<seam>` at a major version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeamDecl {
    /// The seam name; dotted or `<app>:<seam>`.
    pub name: String,
    /// Its major version, 1 or higher.
    pub version: u32,
    /// Whether this app provides the seam or replaces one.
    pub kind: SeamKind,
    /// Why the seam is replaced; only replacements declare one.
    pub reason: Option<String>,
}

/// A whitelisted instance method of one DocType (ADR-16): callable, not run here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodSub {
    /// The DocType the method belongs to.
    pub doctype: String,
    /// Its storage-style name.
    pub name: Name,
    /// Whether it changes documents; the native API then requires `If-Match`.
    pub mutates: bool,
}

/// A whitelisted module-level function (ADR-16).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionSub {
    /// The module the function lives in.
    pub module: Name,
    /// Its storage-style name.
    pub name: Name,
    /// The HTTP verbs it answers, when it answers HTTP.
    pub http: Option<Vec<HttpVerb>>,
    /// Whether unauthenticated guests may call it; defaults to `false`.
    pub guest: bool,
}

/// An HTTP verb a whitelisted function may answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HttpVerb {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
    /// `HEAD`.
    Head,
    /// `OPTIONS`.
    Options,
}

/// One JSON file loaded into a DocType at install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureSub {
    /// The DocType the records belong to.
    pub doctype: String,
    /// The app-relative file with the records.
    pub file: FilePath,
    /// A step that imports the records, instead of a plain insert.
    pub step: Option<StepName>,
}

/// One migration (ADR-22).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchSub {
    /// Its app-unique id, listed in ascending order.
    pub id: String,
    /// The app-relative TOML file with the patch.
    pub file: FilePath,
    /// What the patch may change.
    pub intent: PatchIntent,
    /// Whether the patch can be undone; defaults to `false`.
    pub reversible: bool,
    /// Whether applying it again is safe; defaults to `false`.
    pub idempotent: bool,
}

/// What a patch may change (ADR-22).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PatchIntent {
    /// DocType and field definitions.
    Schema,
    /// Documents.
    Data,
    /// Everything else, such as roles and pages.
    Metadata,
}

/// A file path relative to the app crate: no leading `/`, no `\`, no `:`, and no empty, `.`
/// or `..` segment. The host resolves it inside the app crate, so it never escapes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FilePath(String);

impl FilePath {
    /// Validates an app-relative path, such as `fixtures/roles.json`.
    pub fn parse(raw: &str) -> Result<FilePath, ManifestError> {
        check_path(raw)?;
        Ok(FilePath(raw.to_owned()))
    }

    /// The path as written in the manifest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A route path under the app's URL prefix: starts with `/`, and has no empty, `.` or `..`
/// segment and no `\`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoutePath(pub String);

impl RoutePath {
    /// Validates a route path, such as `/portal/orders`.
    pub fn parse(raw: &str) -> Result<RoutePath, ManifestError> {
        check_route(raw)?;
        Ok(RoutePath(raw.to_owned()))
    }

    /// The path as written in the manifest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One HTTP endpoint the app serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSub {
    /// The path under the app's prefix.
    pub path: RoutePath,
    /// The app's step that handles it.
    pub step: StepName,
}

/// A static file served under the app's prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetSub {
    /// The app-relative file that is served.
    pub path: FilePath,
}

/// What the app may touch. Keys not listed default to the least access.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Hosts and ports the app may call over HTTP.
    pub network: Vec<Endpoint>,
    /// Whether the app may send e-mail.
    pub email: bool,
    /// The file-store access; defaults to `none`.
    pub files: FileAccess,
    /// DocTypes the app may read or change through queries.
    pub doctypes: DocTypeAccess,
}

/// The file-store access a manifest declares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FileAccess {
    /// No file access, the default.
    #[default]
    None,
    /// Read only.
    Read,
    /// Read and write.
    ReadWrite,
}

/// One `host:port` the app may call.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// The host name or IPv4 address.
    pub host: String,
    /// The port, 1 to 65535.
    pub port: u16,
}

/// Which DocTypes the app may read and change through queries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocTypeAccess {
    /// DocType labels the app may read, such as `Sales Invoice`.
    pub read: Vec<String>,
    /// DocType labels the app may change.
    pub write: Vec<String>,
}

/// The default budget for document lifecycle steps, in milliseconds.
pub const EVENT_BUDGET_DEFAULT_MS: u64 = 200;
/// The longest a document lifecycle step, session hook or boot step may take.
pub const EVENT_BUDGET_LIMIT_MS: u64 = 60_000;
/// The default budget for a schedule step, in milliseconds.
pub const SCHEDULE_BUDGET_DEFAULT_MS: u64 = 600_000;
/// The longest a schedule step may take.
pub const SCHEDULE_BUDGET_LIMIT_MS: u64 = 3_600_000;

/// Checks a `[[routes]]` `path`: it must stay under the app's URL prefix.
pub(crate) fn check_route(raw: &str) -> Result<(), ManifestError> {
    let bad = |why: String| {
        Err(ManifestError::new(
            crate::error::INVALID_ROUTE,
            format!("route path {raw:?}: {why} (example: /portal/orders)"),
        ))
    };
    let Some(rest) = raw.strip_prefix('/') else {
        return bad("it must start with `/`".to_owned());
    };
    if raw.contains('\\') {
        return bad("it must use `/`, not `\\`".to_owned());
    }
    for segment in rest.split('/') {
        if segment.is_empty() {
            return bad("it has an empty segment (`//`)".to_owned());
        }
        if matches!(segment, "." | "..") {
            return bad(format!("`{segment}` cannot be part of a URL path"));
        }
    }
    Ok(())
}

// [permissions]
/// The longest permission name, in bytes: the rule `spot-permissions` applies to
/// `AppPermissionName`, restated here because this crate may not depend on it.
pub const MAX_PERMISSION_NAME_BYTES: usize = 64;
/// The longest permission title, in characters.
pub const MAX_PERMISSION_TITLE_CHARS: usize = 120;

/// A validated app permission name: non-empty, at most [`MAX_PERMISSION_NAME_BYTES`] bytes,
/// lowercase ASCII letters, digits and `_`, starting with a lowercase letter.
///
/// The rules mirror `spot_permissions::AppPermissionName` exactly, so a name this crate
/// accepts is one the catalogue accepts; the type is declared here because `spot-manifest`
/// does not depend on `spot-permissions` (WP-PLT-03.T4). Names are compared exactly: never
/// trimmed or normalised.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppPermissionName(String);

impl AppPermissionName {
    /// Validates `raw` as a permission name, such as `approve_note`.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] with the code `manifest.permission_name` for an empty name, one over
    /// [`MAX_PERMISSION_NAME_BYTES`] bytes, one starting with anything but a lowercase letter,
    /// or one holding anything but lowercase letters, digits and `_`.
    pub fn parse(raw: &str) -> Result<AppPermissionName, ManifestError> {
        let bad = |why: &str| {
            Err(ManifestError::new(
                crate::error::PERMISSION_NAME,
                format!(
                    "permission name {raw:?}: {why} (use lowercase letters, digits and `_`, \
                     starting with a letter, at most {MAX_PERMISSION_NAME_BYTES} bytes; \
                     example: approve_note)"
                ),
            ))
        };
        if raw.is_empty() {
            return bad("it is empty");
        }
        if raw.len() > MAX_PERMISSION_NAME_BYTES {
            return bad("it is longer than 64 bytes");
        }
        let mut bytes = raw.bytes();
        if !bytes.next().is_some_and(|b| b.is_ascii_lowercase()) {
            return bad("it does not start with a lowercase letter");
        }
        if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return bad("it holds a character other than a lowercase letter, a digit or `_`");
        }
        Ok(AppPermissionName(raw.to_owned()))
    }

    /// The name as written in the manifest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppPermissionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `[[permissions]]` row: a named permission the app declares, so its whitelisted methods
/// can require it with `right = permission("<name>")`. Declaring it grants nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionSub {
    /// The name a method's right and a role grant refer to.
    pub name: AppPermissionName,
    /// A short title for the role editor, one line and at most
    /// [`MAX_PERMISSION_TITLE_CHARS`] characters.
    pub title: String,
    /// What holding it lets a user do; empty when the app states none.
    pub description: Option<String>,
}

/// What an app says about one permission it declares, as the site's catalogue keeps it
/// (`spot_permissions::AppPermissionDecl`, field for field, so the host maps it one to one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPermissionDecl {
    /// The app that declares it.
    pub app: String,
    /// A short title for the role editor.
    pub title: String,
    /// What holding it lets a user do; empty when the app states none.
    pub description: String,
}

/// Checks a relative path used for files, fixtures, patches and assets.
pub(crate) fn check_path(raw: &str) -> Result<(), ManifestError> {
    let bad = |why: String| {
        Err(ManifestError::new(
            INVALID_PATH,
            format!("file path {raw:?}: {why} (example: fixtures/roles.json)"),
        ))
    };
    if raw.is_empty() {
        return bad("it is empty".to_owned());
    }
    if raw.starts_with('/') {
        return bad(
            "it must be relative to the app crate, so it must not start with `/`".to_owned(),
        );
    }
    if raw.contains(':') {
        return bad("a `:` looks absolute on Windows, so it is not allowed".to_owned());
    }
    if raw.contains('\\') {
        return bad("it must use `/`, not `\\`".to_owned());
    }
    for segment in raw.split('/') {
        if segment.is_empty() {
            return bad("it has an empty segment (`//`)".to_owned());
        }
        if matches!(segment, "." | "..") {
            return bad(format!(
                "a `{segment}` segment would escape the app directory"
            ));
        }
    }
    Ok(())
}

/// Checks a DocType label: letters, digits and single spaces, starting and ending with a
/// letter or digit, as in `Sales Invoice`.
pub(crate) fn check_doctype(raw: &str) -> Result<(), ManifestError> {
    let bad = |why: String| {
        Err(ManifestError::new(
            crate::error::INVALID_CAPABILITY,
            format!("DocType name {raw:?}: {why} (example: Sales Invoice)"),
        ))
    };
    if raw.is_empty() {
        return bad("it is empty".to_owned());
    }
    let mut at_space = true;
    for ch in raw.chars() {
        if ch == ' ' {
            if at_space {
                return bad("it has a leading or double space".to_owned());
            }
            at_space = true;
        } else if ch.is_ascii_alphanumeric() {
            at_space = false;
        } else {
            return bad("it must use letters, digits and single spaces".to_owned());
        }
    }
    if at_space {
        return bad("it ends with a space".to_owned());
    }
    Ok(())
}
