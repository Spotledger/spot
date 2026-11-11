//! The secrets service: the site's key provider and the vault behind the engine's `secret`
//! fields (WP-SEC-03.T9; ADR-070 section 12). Operator guide: `docs/running-spot/secrets.md`.
//!
//! # What is hosted
//!
//! `[services.secrets]` of `common.toml` names a **local key file** (`provider = "local"`, the
//! only provider today; [`KeyProvider`] stays the seam for a KMS). [`open`] builds the site's
//! [`SecretsHost`] from it: the key provider ([`LocalKeyring`]) and the [`SecretBox`] the
//! document engine seals, opens and blind-indexes with. The host hands the vault to the engine
//! at site construction (`host.rs` calls [`open`] before `Engine::new`), so a sealed value is
//! written in the same transaction as its record.
//!
//! # Level, and what a failure does
//!
//! The service is **required** when `[services.secrets]` exists:
//!
//! - a table that is present but unusable (no `kek_file`, a path that is relative or inside a
//!   site folder, a missing, empty, unreadable, group- or world-accessible, foreign-owned or
//!   malformed key file, a key file that cannot wrap and unwrap its canary) stops the host from
//!   starting, naming the file and never a key; on a reload it makes the site answer 503;
//! - a stored value sealed under a key version the file does not hold (an older copy of the
//!   file) fails the start-up check the same way;
//! - there is never a plaintext fallback and never a default key: with no vault the engine
//!   answers `secretfield.not_configured` to every secret operation.
//!
//! A host whose `common.toml` has **no** `[services.secrets]` table starts without a vault, as
//! every host did before this service existed; a secret field then cannot be written. See the
//! report's open questions for the choice and how to make the table mandatory.
//!
//! # Keys are per site
//!
//! `kek_file` carries `{site}`, replaced by the site's name, so each site has its own key file.
//! A path without it is accepted only on a host with a single site.
//!
//! # Key material
//!
//! The KEK stays inside [`LocalKeyring`] (zeroed on drop). Nothing in this module reads, copies,
//! prints or returns it: errors name the file and the stable code of the cause, never a byte.
//! The commands (`spot secrets`) reach rotation through the public items here.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use spot_core::Clock;
use spot_document::secrets::SecretVault;
use spot_meta::EffectiveMeta;
use spot_postgres::PgBackend;
use spot_secrets::{KeyProvider, KeyRef, LocalKeyring, SecretBox, SecretError};
use spot_storage_contract::Catalog;
use tokio::sync::watch;

// The module files live in `secrets/` (the services directory is scanned flat by build.rs, so
// the path is spelled out).
#[path = "secrets/rotate.rs"]
mod rotate;
#[path = "secrets/scan.rs"]
mod scan;

pub use rotate::{
    RotationLock, SweepTotals, add_version, open_engine, retire_version, sweep_once,
};
pub use scan::{FieldVersions, rows_on_old_versions, scan};

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result, config};

/// Start order: after the framework's own services (the registry's modules use 1000 and up).
pub const ORDER: u32 = 1000;

/// The module and service name, and the `[services.<name>]` table it reads.
pub const NAME: &str = "secrets";

/// The stable codes this service answers with (see `catalogue/secrets_host.toml`). A refusal is
/// `"<code>: <text>"`; the text names a file or a count, never key material.
pub mod codes {
    /// The `[services.secrets]` table has an unknown key or a value of the wrong type.
    pub const SETTINGS_INVALID: &str = "secrets_host.settings_invalid";
    /// The table names no `kek_file`, or a provider other than `local`.
    pub const KEK_UNSET: &str = "secrets_host.kek_unset";
    /// The key file path is relative, inside a site folder, or shared by several sites.
    pub const KEK_PATH_REFUSED: &str = "secrets_host.kek_path_refused";
    /// The key file is missing, empty, unreadable or not a key file.
    pub const KEK_FILE_UNUSABLE: &str = "secrets_host.kek_file_unusable";
    /// The key file is not private to the service's user.
    pub const KEK_FILE_UNSAFE: &str = "secrets_host.kek_file_unsafe";
    /// The key provider could not wrap and unwrap its canary.
    pub const CANARY_FAILED: &str = "secrets_host.canary_failed";
    /// A stored value names a key version the key file does not hold.
    pub const KEY_VERSION_MISSING: &str = "secrets_host.key_version_missing";
    /// Another rotation holds the key file's lock.
    pub const ROTATION_RUNNING: &str = "secrets_host.rotation_running";
    /// A rewrap sweep did not finish cleanly.
    pub const REWRAP_FAILED: &str = "secrets_host.rewrap_failed";
    /// A key version cannot be retired: rows still use it, or it is the current one.
    pub const RETIRE_REFUSED: &str = "secrets_host.retire_refused";
    /// The identity audit chain cannot take the record the operation depends on.
    pub const AUDIT_UNAVAILABLE: &str = "secrets_host.audit_unavailable";
}

fn refuse(code: &str, text: impl fmt::Display) -> Error {
    Error(format!("{code}: {text}"))
}

/// The stable code of a provider failure; the rest of the error is dropped on purpose.
fn cause(error: &SecretError) -> &'static str {
    error.code()
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `[services.secrets]` of `common.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The key provider. Only `local` (a key file) exists.
    #[serde(default = "local")]
    pub provider: String,
    /// The key file: an absolute path outside the site folders, with `{site}` for the site's
    /// name. It is created by `spot secrets init`, never by the server.
    #[serde(default)]
    pub kek_file: Option<PathBuf>,
}

fn local() -> String {
    "local".into()
}

impl Settings {
    /// The table of `common`, or `None` when the host configures no secrets service.
    ///
    /// # Errors
    ///
    /// `secrets_host.settings_invalid`, naming the key and never a value.
    pub fn from_common(common: &config::Common) -> Result<Option<Settings>> {
        let Some(table) = common.services.services_extra.get(NAME) else {
            return Ok(None);
        };
        match table.clone().try_into::<Settings>() {
            Ok(settings) => Ok(Some(settings)),
            Err(e) => {
                let message = e.message();
                // `unknown field` and `missing field` quote key names only; anything else can
                // quote the value, and the value may be a path an operator keeps private.
                let why = if message.starts_with("unknown field") {
                    message.to_owned()
                } else {
                    "a value has the wrong type".to_owned()
                };
                Err(refuse(
                    codes::SETTINGS_INVALID,
                    format!("services.secrets: {why}"),
                ))
            }
        }
    }

    /// The key file of `site`, checked as a path (not yet as a file).
    ///
    /// # Errors
    ///
    /// `secrets_host.kek_unset`, or `secrets_host.kek_path_refused` for a relative path, a `..`
    /// component, a path inside the site folders (`<root>/sites`) or one several sites share.
    pub fn kek_path(&self, root: &Path, site: &str, sites: usize) -> Result<PathBuf> {
        if self.provider != "local" {
            return Err(refuse(
                codes::KEK_UNSET,
                "services.secrets.provider: only `local` is supported",
            ));
        }
        let Some(template) = &self.kek_file else {
            return Err(refuse(
                codes::KEK_UNSET,
                "services.secrets.kek_file is not set; the secrets service has no key",
            ));
        };
        let text = template.to_string_lossy();
        if sites > 1 && !text.contains("{site}") {
            return Err(refuse(
                codes::KEK_PATH_REFUSED,
                "services.secrets.kek_file is shared by several sites; put `{site}` in the path so each site has its own key file",
            ));
        }
        let path = PathBuf::from(text.replace("{site}", site));
        if !path.is_absolute() {
            return Err(refuse(
                codes::KEK_PATH_REFUSED,
                "services.secrets.kek_file must be an absolute path",
            ));
        }
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(refuse(
                codes::KEK_PATH_REFUSED,
                "services.secrets.kek_file must not contain `..`",
            ));
        }
        let sites_dir = canonical_or(&root.join("sites"));
        let parent = path.parent().map(canonical_or).unwrap_or_default();
        if parent.starts_with(&sites_dir) {
            return Err(refuse(
                codes::KEK_PATH_REFUSED,
                format!(
                    "the key file {} is inside the site folders; a backup or copy of a site would carry the key with the data",
                    path.display()
                ),
            ));
        }
        Ok(path)
    }
}

fn canonical_or(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------------------------
// The key file
// ---------------------------------------------------------------------------------------------

/// Refuses a key file that is not a non-empty regular file private to this user.
///
/// On Unix the mode must grant nothing to group and others, and the file must belong to the
/// user the process runs as (when that can be told). Elsewhere only the file's kind and size are
/// checked; the operator guide says what to do on Windows.
///
/// # Errors
///
/// `secrets_host.kek_file_unusable` or `secrets_host.kek_file_unsafe`, naming the file.
pub fn check_key_file(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path).map_err(|e| {
        refuse(
            codes::KEK_FILE_UNUSABLE,
            format!("the key file {} cannot be read ({})", path.display(), e.kind()),
        )
    })?;
    if !meta.is_file() {
        return Err(refuse(
            codes::KEK_FILE_UNUSABLE,
            format!("the key file {} is not a regular file", path.display()),
        ));
    }
    if meta.len() == 0 {
        return Err(refuse(
            codes::KEK_FILE_UNUSABLE,
            format!("the key file {} is empty", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(refuse(
                codes::KEK_FILE_UNSAFE,
                format!(
                    "the key file {} is accessible to group or others (mode {mode:o}); run `chmod 600` on it",
                    path.display()
                ),
            ));
        }
        // The process's own user owns `/proc/self` (Linux). Where it is not there, the check is
        // skipped rather than guessed.
        if let Ok(own) = std::fs::metadata("/proc/self")
            && own.uid() != meta.uid()
        {
            return Err(refuse(
                codes::KEK_FILE_UNSAFE,
                format!(
                    "the key file {} belongs to another user than the one running spot",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Equality that takes the same time wherever the first difference is.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------------------------
// The hosted provider and vault
// ---------------------------------------------------------------------------------------------

/// The canary the start-up check wraps and unwraps. It is not a secret.
const CANARY: &[u8] = b"spot secrets start-up canary v1.";
const CANARY_AAD: &[u8] = b"spot-secrets/canary";

/// One site's key provider and vault.
///
/// Debug output shows the site, the file and the current version; never a key.
pub struct SecretsHost {
    site: String,
    path: PathBuf,
    keyring: Arc<LocalKeyring>,
    vault: Arc<SecretBox<Arc<LocalKeyring>>>,
    legacy: usize,
}

impl fmt::Debug for SecretsHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretsHost")
            .field("site", &self.site)
            .field("kek_file", &self.path)
            .field("current_version", &self.keyring.current().version)
            .finish_non_exhaustive()
    }
}

impl SecretsHost {
    /// The vault the engine seals with.
    pub fn vault(&self) -> Arc<dyn SecretVault> {
        self.vault.clone()
    }

    /// The key provider (for the commands that rotate and retire).
    pub fn keyring(&self) -> &Arc<LocalKeyring> {
        &self.keyring
    }

    /// The version new values are sealed under.
    pub fn current_version(&self) -> u32 {
        self.keyring.current().version
    }

    /// The key file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The site.
    pub fn site(&self) -> &str {
        &self.site
    }

    /// How many credentials the site file still holds (the migration is WP-PLT-02.T8).
    pub fn legacy_credentials(&self) -> usize {
        self.legacy
    }

    /// Wraps and unwraps a canary under the current version and checks the blind-index key
    /// derives. A failure names the file and the cause's code.
    ///
    /// # Errors
    ///
    /// `secrets_host.canary_failed`.
    pub async fn canary(&self) -> Result<()> {
        canary_of(&*self.keyring, &self.path).await
    }

    /// The key versions the file knows: the current one, the others that still hold key
    /// material, and the retired ones.
    pub async fn versions(&self) -> Versions {
        let current = self.keyring.current();
        let mut held = Vec::new();
        let mut retired = Vec::new();
        for version in 1..=current.version {
            let key = KeyRef {
                id: current.id.clone(),
                version,
            };
            match self.keyring.index_key(&key).await {
                Ok(_) => held.push(version),
                Err(SecretError::RetiredKey { .. }) => retired.push(version),
                Err(_) => {}
            }
        }
        Versions {
            id: current.id,
            current: current.version,
            held,
            retired,
        }
    }
}

/// The start-up check over any provider: wrap and unwrap the canary under the current version,
/// compare in constant time, derive the blind-index key. `path` is only named in the message.
///
/// # Errors
///
/// `secrets_host.canary_failed`, with the file and the stable code of the cause.
pub async fn canary_of(provider: &dyn KeyProvider, path: &Path) -> Result<()> {
    let fail = |why: &str| {
        refuse(
            codes::CANARY_FAILED,
            format!("the key file {} failed its start-up check ({why})", path.display()),
        )
    };
    let current = provider.current();
    let wrapped = provider
        .wrap(CANARY, CANARY_AAD)
        .await
        .map_err(|e| fail(cause(&e)))?;
    if wrapped.key != current {
        return Err(fail("the wrapping key is not the current one"));
    }
    let back = provider
        .unwrap(&wrapped.key, &wrapped, CANARY_AAD)
        .await
        .map_err(|e| fail(cause(&e)))?;
    if !ct_eq(back.expose(), CANARY) {
        return Err(fail("the canary did not come back"));
    }
    let index = provider
        .index_key(&current)
        .await
        .map_err(|e| fail(cause(&e)))?;
    if index.expose().is_empty() {
        return Err(fail("the blind-index key is empty"));
    }
    Ok(())
}

/// The key versions a key file holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versions {
    /// The key id (a name, never key material).
    pub id: String,
    /// The version new values are sealed under.
    pub current: u32,
    /// Every version with key material, the current one included, ascending.
    pub held: Vec<u32>,
    /// Versions whose key material was destroyed.
    pub retired: Vec<u32>,
}

/// Builds the site's [`SecretsHost`] from `[services.secrets]`, or `None` when the host has no
/// such table.
///
/// # Errors
///
/// Every refusal listed in the module docs; the text names the file, never a key.
pub async fn open(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
) -> Result<Option<Arc<SecretsHost>>> {
    let Some(settings) = Settings::from_common(&loaded.common)? else {
        return Ok(None);
    };
    let path = settings.kek_path(root, &site.site, loaded.sites.len())?;
    host_at(&path, site).await.map(Some)
}

/// The host over the key file at `path` (already a vetted path).
async fn host_at(path: &Path, site: &config::Site) -> Result<Arc<SecretsHost>> {
    check_key_file(path)?;
    let keyring = LocalKeyring::open(path).await.map_err(|e| {
        refuse(
            codes::KEK_FILE_UNUSABLE,
            format!("the key file {} is not usable ({})", path.display(), cause(&e)),
        )
    })?;
    let keyring = Arc::new(keyring);
    let host = SecretsHost {
        site: site.site.clone(),
        path: path.to_path_buf(),
        vault: Arc::new(SecretBox::new(keyring.clone())),
        keyring,
        legacy: site.credentials.len() + site.api_keys.len(),
    };
    host.canary().await?;
    Ok(Arc::new(host))
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

/// How many of the newest sealed values of each field the start-up check reads.
const PROBE: u32 = 20;

struct SecretsService {
    host: Arc<SecretsHost>,
    backend: PgBackend,
    catalog: Arc<Catalog>,
    meta: Arc<EffectiveMeta>,
}

impl SecretsService {
    async fn verify(&self) -> Result<()> {
        self.host.canary().await?;
        verify_values(&self.host, &self.backend, &self.catalog, &self.meta).await
    }
}

/// Reads the newest sealed values of every secret field and refuses when one names a key
/// version the key file does not hold: the file is an older copy, and new values sealed under
/// its current version would sit beside values nothing could open.
///
/// # Errors
///
/// `secrets_host.key_version_missing`, naming the file and the version; a storage error.
pub async fn verify_values(
    host: &SecretsHost,
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    meta: &EffectiveMeta,
) -> Result<()> {
    let found = scan(backend, catalog, meta, Some(PROBE)).await?;
    for field in &found {
        for ((id, version), rows) in &field.by_key {
            let key = KeyRef {
                id: id.clone(),
                version: *version,
            };
            if host.keyring.index_key(&key).await.is_err() {
                return Err(refuse(
                    codes::KEY_VERSION_MISSING,
                    format!(
                        "{rows} stored value(s) of `{}.{}` are sealed under key version {version}, which the key file {} does not hold; restore the key file that holds it",
                        field.doctype,
                        field.field,
                        host.path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

impl Service for SecretsService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.verify().await.map_err(|e| e.0) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

/// Joins the site: adds the required service when the host built a vault for it. The host
/// provides the [`SecretsHost`] port (it must exist before the engine, which the registry
/// builds after), so this module only checks it and reports what remains in the site file.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let Some(host) = ctx.port::<SecretsHost>() else {
        tracing::info!(
            site = ctx.label(),
            "secrets: no [services.secrets] table; secret fields cannot be written"
        );
        return Ok(());
    };
    if ctx.assembling() {
        if host.legacy_credentials() > 0 {
            tracing::warn!(
                site = ctx.label(),
                remaining = host.legacy_credentials(),
                "secrets: credentials still live in the site file (the migration to the vault is WP-PLT-02.T8)"
            );
        }
        let env = ctx.env()?;
        let service = SecretsService {
            host,
            backend: env.backend.clone(),
            catalog: env.permission.catalog.clone(),
            meta: env.permission.meta.clone(),
        };
        ctx.add(Arc::new(service));
    }
    Ok(())
}


// ---------------------------------------------------------------------------------------------
// What the commands use besides the above
// ---------------------------------------------------------------------------------------------

/// The settings of `site`'s host and its vetted key file path, or `None` when the host has no
/// `[services.secrets]` table.
///
/// # Errors
///
/// The refusals of [`Settings::kek_path`].
pub fn key_path(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
) -> Result<Option<PathBuf>> {
    let Some(settings) = Settings::from_common(&loaded.common)? else {
        return Ok(None);
    };
    settings
        .kek_path(root, &site.site, loaded.sites.len())
        .map(Some)
}

/// Creates the site's key file with a random version 1, mode 0600, in a folder that is created
/// private (0700) when it does not exist. Refuses to touch a file that is there.
///
/// # Errors
///
/// `secrets_host.kek_file_unusable` when the file exists or cannot be written.
pub async fn init_key_file(path: &Path, site: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent).map_err(|e| {
            refuse(
                codes::KEK_FILE_UNUSABLE,
                format!("cannot create {} ({})", parent.display(), e.kind()),
            )
        })?;
    }
    LocalKeyring::create(path, &format!("spot-{site}"))
        .await
        .map(|_| ())
        .map_err(|e| {
            refuse(
                codes::KEK_FILE_UNUSABLE,
                format!("the key file {} was not created ({})", path.display(), cause(&e)),
            )
        })
}

/// The identity audit chain as the commands write to it.
pub struct OperatorAudit {
    recorder: crate::audit::Recorder,
}

impl OperatorAudit {
    /// Opens the site's chain sink.
    ///
    /// # Errors
    ///
    /// `secrets_host.audit_unavailable` when the database or the audit store is not ready: an
    /// operation that must be audited does not run without it.
    pub async fn open(
        loaded: &config::Loaded,
        site: &config::Site,
        clock: Arc<dyn Clock>,
    ) -> Result<OperatorAudit> {
        let pg = loaded.admin.clone().with_dbname(&site.database);
        match crate::audit::chain::ChainAudit::open(pg, &site.site, None).await {
            Ok(Ok((sink, _))) => Ok(OperatorAudit {
                recorder: crate::audit::Recorder::new(&site.site, Arc::new(sink), clock),
            }),
            _ => Err(refuse(
                codes::AUDIT_UNAVAILABLE,
                "the identity audit chain is not available; run `spot migrate --apply` and check the database",
            )),
        }
    }

    fn record(&self, kind: &str, count: u64, reason: Option<&str>) -> crate::audit::AuditRecord {
        let mut record = self.recorder.record(kind);
        record.count = Some(count);
        record.reason = reason.map(str::to_owned);
        record
    }

    /// Records `kind` (a count and an optional reason class) and fails when it cannot be stored.
    ///
    /// # Errors
    ///
    /// `secrets_host.audit_unavailable`.
    pub fn required(&self, kind: &str, count: u64, reason: Option<&str>) -> Result<()> {
        self.recorder
            .required(&self.record(kind, count, reason))
            .map_err(|_| refuse(codes::AUDIT_UNAVAILABLE, "the audit record could not be stored"))
    }

    /// Records `kind`; a failure is logged by the recorder and does not change the outcome (the
    /// act already ended).
    pub fn always(&self, kind: &str, count: u64, reason: Option<&str>) {
        self.recorder.always(&self.record(kind, count, reason));
    }
}

#[cfg(test)]
#[path = "secrets/tests.rs"]
mod tests;
