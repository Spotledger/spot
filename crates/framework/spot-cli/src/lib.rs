//! Spotledger product host: strict configuration, provisioning and native HTTP serving.
pub mod appdir;
pub mod apps;
pub mod audit;
mod auth;
pub mod bundle_install;
pub mod config;
mod decode;
pub mod gate;
mod host;
pub mod live;
pub mod login;
pub mod natives;
pub mod onetime;
pub mod ops;
pub mod password_throttle;
pub mod permissions;
pub mod schema;
pub mod seed;
pub mod services;
pub mod sitefile;
pub mod users;

use spot_auth::ApiKey;
use spot_core::SystemClock;
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, EffectiveMeta, Registry};
use spot_postgres::{PgBackend, create_database, database_exists};
use spot_seams::RegistryBuilder;
use std::path::Path;
use std::sync::Arc;

/// The effect classes a platform host knows, re-exported from the one shared list in
/// `spot-seams` (WP-EXT-02.T5) so no second copy can drift.
pub use spot_seams::PLATFORM_EFFECT_CLASSES;

/// A step registry builder for a platform host: strict, with the shared platform effect classes
/// ([`spot_seams::platform_effect_classes`]) as the known effect classes. A step declaring any
/// other class fails the registry build with `effects.unknown_class`.
///
/// # Errors
///
/// None today; the `Result` is kept so the signature stays stable.
pub fn platform_registry_builder() -> Result<RegistryBuilder> {
    let mut builder = RegistryBuilder::for_platform();
    builder.known_effect_classes(&spot_seams::platform_effect_classes());
    Ok(builder)
}

/// A platform host's registry with no controllers (the seed and bootstrap engines).
///
/// # Errors
///
/// Any conflict the registry build finds.
pub fn platform_registry() -> Result<spot_seams::Registry> {
    platform_registry_builder()?
        .build()
        .map_err(|e| Error(format!("invalid registry: {e:?}")))
}

pub use host::{Options, Running, request_reload, start, start_with, start_with_audit};
/// Host failure. Secret configuration values are never included.
#[derive(Debug)]
pub struct Error(pub String);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}
/// Host operation result.
pub type Result<T> = std::result::Result<T, Error>;
fn error(e: impl std::fmt::Display) -> Error {
    Error(e.to_string())
}

pub(crate) fn metadata(root: &Path, loaded: &config::Loaded) -> Result<Arc<EffectiveMeta>> {
    let mut definitions = Vec::new();
    let mut errors = Vec::new();
    for app in &loaded.common.apps {
        let mut entries: Vec<_> =
            std::fs::read_dir(root.join(app).join("doctypes"))?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match Definition::parse(
                &path.display().to_string(),
                &std::fs::read_to_string(&path)?,
            ) {
                Ok(d) => definitions.push(d),
                Err(e) => errors.push(e.to_string()),
            }
        }
    }
    if !errors.is_empty() {
        return Err(Error(errors.join("\n")));
    }
    Registry::new(definitions)
        .map(|r| r.effective())
        .map_err(|errors| {
            Error(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
}
async fn engine(
    config: spot_postgres::PgConfig,
    meta: Arc<EffectiveMeta>,
) -> Result<Engine<PgBackend>> {
    let backend = PgBackend::connect(config).await.map_err(error)?;
    let engine = Engine::new(
        backend,
        meta,
        Arc::new(platform_registry()?),
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .map_err(error)?;
    engine.prepare().await.map_err(error)?;
    Ok(engine)
}
/// How a new site is provisioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteKind {
    /// Installs the platform app and every catalogue app in `common.toml` `apps`.
    HostWide,
    /// Installs only the platform app; further apps arrive with `spot install-app`.
    Bare,
}
/// Creates a site without overwriting any existing directory or database.
/// Returns the Administrator's one-time first sign-in secret for one-time display only. The
/// secret works as the password for 72 hours and can only be used to choose a password; it is
/// never an API key. See [`new_site_with`] to choose the lifetime or ask for an API key file.
pub async fn new_site(root: &Path, name: &str, collation: &str) -> Result<ApiKey> {
    new_site_of(root, name, collation, SiteKind::HostWide).await
}
/// Creates a site of the given kind, issuing credentials now with the default lifetimes.
pub async fn new_site_of(
    root: &Path,
    name: &str,
    collation: &str,
    kind: SiteKind,
) -> Result<ApiKey> {
    new_site_with(root, name, collation, kind, &onetime::Issue::system(), None).await
}
/// Creates a site of the given kind. See [`new_site`].
///
/// With `admin_key_file`, a separately generated Administrator API key is written to that path
/// (created exclusively with mode 0600 on Unix; an existing path is refused before anything is
/// created). The key expires at `issue.key_expires_at()` and, like the one-time secret, can only
/// change the password until the Administrator has chosen one (`spot set-password`).
pub async fn new_site_with(
    root: &Path,
    name: &str,
    collation: &str,
    kind: SiteKind,
    issue: &onetime::Issue,
    admin_key_file: Option<&Path>,
) -> Result<ApiKey> {
    if !config::site_name(name) {
        return Err(Error("invalid site name".into()));
    }
    if !config::collation(collation) {
        return Err(Error("invalid collation".into()));
    }
    if let Some(path) = admin_key_file
        && path.symlink_metadata().is_ok()
    {
        return Err(Error(format!(
            "{} already exists; choose a new path for the Administrator API key file",
            path.display()
        )));
    }
    if let Some(path) = admin_key_file {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        if !parent.is_dir() {
            return Err(Error(format!(
                "the folder for the Administrator API key file ({}) does not exist",
                parent.display()
            )));
        }
    }
    let loaded = config::load(root, false)?;
    // Every site starts with the platform app; a host-wide site also starts with every app in
    // the catalogue, a bare one waits for `install-app`.
    let apps = apps::new_site_apps(root, &loaded, kind == SiteKind::Bare)?;
    let meta = apps::effective(
        root,
        &loaded,
        &config::Site::named(name, "", collation, apps.clone()),
        &apps,
    )?;
    let database = name.replace('-', "_");
    let directory = root.join("sites").join(name);
    if directory.exists()
        || database_exists(&loaded.admin, &database)
            .await
            .map_err(error)?
    {
        return Err(Error(
            "site or database already exists; refusing to overwrite".into(),
        ));
    }
    // create_dir is an exclusive reservation against concurrent new-site invocations.
    std::fs::create_dir_all(root.join("sites"))?;
    std::fs::create_dir(&directory)?;
    // Past the exclusive reservation, any failure must not leave a directory without a
    // `site.toml`: `serve` would then refuse to start for every site, not just this one.
    let created = provision(
        &loaded,
        meta,
        &directory,
        &database,
        name,
        collation,
        apps,
        issue,
        admin_key_file,
    )
    .await;
    if created.is_err() {
        let _ = std::fs::remove_file(directory.join("site.toml"));
        let _ = std::fs::remove_dir(&directory);
    }
    created
}
#[allow(clippy::too_many_arguments)]
async fn provision(
    loaded: &config::Loaded,
    meta: Arc<EffectiveMeta>,
    directory: &Path,
    database: &str,
    name: &str,
    collation: &str,
    apps: Vec<String>,
    issue: &onetime::Issue,
    admin_key_file: Option<&Path>,
) -> Result<ApiKey> {
    create_database(&loaded.admin, database, collation)
        .await
        .map_err(error)?;
    let mut pg = loaded.admin.clone().with_dbname(database);
    pg.pool_max_size = loaded.common.postgres.pool_max_size;
    let engine = engine(pg, meta.clone()).await?;
    // The platform's non-document tables (edge sync's `spot_graph_edge` and drift tables, the
    // mail sent log) are created through the migrator's plan, the path `spot migrate` takes, so a
    // site is ready to serve as soon as it exists and nothing is created behind the plan's back.
    let plan =
        crate::schema::plan_with_platform_tables(engine.backend(), &crate::schema::catalog(&meta)?)
            .await?;
    crate::schema::refusals(&plan)?;
    if !plan.is_empty() {
        crate::schema::apply(engine.backend(), &plan, None).await?;
    }
    // The audit chain's role, grants and guard triggers: the plan creates its tables only.
    crate::schema::protect_audit(engine.backend()).await?;
    // Every site starts with the platform app's Administrator (`bootstrap::seed` is idempotent
    // and writes the Administrator, the system roles and the Administrator's assignment).
    let user = users::seed_platform(engine.backend(), &meta).await?;
    drop(engine);
    let (secret, credential) = users::issue_first_sign_in(user, issue.secret_expires_at());
    let mut site = config::Site::named(name, database, collation, apps);
    site.credentials.push(credential);
    // The key file is written first and removed again if the site file cannot be, so a failed
    // provisioning leaves no credential lying around that opens nothing.
    let mut written = None;
    if let Some(path) = admin_key_file {
        let (key_secret, key) = users::issue_bootstrap_key(user, issue.key_expires_at());
        write_secret_file(path, &key_secret.text())?;
        written = Some(path);
        site.api_keys.push(key);
    }
    if let Err(e) = sitefile::create(&directory.join("site.toml"), &site) {
        if let Some(path) = written {
            let _ = std::fs::remove_file(path);
        }
        return Err(e);
    }
    Ok(secret)
}

/// Creates `path` exclusively (it must not exist) with mode 0600 and writes one line.
fn write_secret_file(path: &Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error(format!(
                "{} already exists; refusing to overwrite",
                path.display()
            ))
        } else {
            Error(format!("cannot create {}: {}", path.display(), e.kind()))
        }
    })?;
    writeln!(file, "{text}")?;
    file.sync_all()?;
    Ok(())
}
