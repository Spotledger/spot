//! The object-store file providers, hosted (WP-PLT-10.T8): `services.files.provider = "s3"` or
//! `"azure"` stores a site's files in a bucket or container.
//!
//! The providers live in `spot-files` behind its `s3` and `azure` features; the host exposes them
//! as the `files-s3` and `files-azure` features of `spot-cli`, both **off by default**, so the
//! default build pulls no cloud client. A provider whose feature is not compiled in is refused at
//! start-up, and so is anything that cannot work: a missing bucket, a reference that names no
//! readable credential, plain HTTP to a remote endpoint. Credentials are **references**
//! (`env:NAME` or `file:/absolute/path`), resolved here, handed to the client and dropped; no
//! settings type holds a credential value, and no message, log line or readiness text carries one.
//!
//! This is a helper module (no `ORDER`, no `register`): `host.rs` calls [`open`] while it builds the
//! files settings, `config.rs` calls [`validate`], and the files service's start-up check calls
//! [`probe`].
//!
//! # Keys
//!
//! The object store keeps `<site>/<key>`: one bucket can serve several sites. There is no setting
//! for a prefix.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use spot_core::SiteId;
use spot_files::key::{FileKey, Visibility};
use spot_files::local::LocalFileStore;
use spot_files::memory::MemoryFileStore;
use spot_files::store::{FileStore, PutOptions, body_from_vec};
use tokio::io::AsyncReadExt;

use crate::config::{Files, FilesAzure, FilesS3};
use crate::{Error, Result};

/// The longest the start-up probe may take before it fails with `timeout`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the readiness text says about an object provider: never a key, a query string or a
/// credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// `s3 bucket <name>` or `azure container <name>`.
    pub label: String,
    /// Plain HTTP to a remote endpoint was explicitly allowed.
    pub insecure: bool,
}

impl ObjectInfo {
    /// The text a readiness line shows.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.insecure {
            format!("{} (insecure)", self.label)
        } else {
            self.label.clone()
        }
    }
}

/// What [`open`] built.
pub struct Opened {
    /// The store the files service uses.
    pub store: Arc<dyn FileStore>,
    /// Set for the object providers.
    pub object: Option<ObjectInfo>,
}

fn key(table: &str, name: &str) -> String {
    format!(
        "services.files.{table}{}{name}",
        if table.is_empty() { "" } else { "." }
    )
}

/// A reference to a credential: `env:NAME` or `file:/absolute/path`. It never holds the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialRef {
    /// An environment variable of this process.
    Env(String),
    /// A file, absolute; on Unix not readable by its group or others.
    File(PathBuf),
}

impl CredentialRef {
    /// Parses `text`; `setting` (for example `services.files.s3.secret_access_key_ref`) names the
    /// key in the message. The text itself is never quoted.
    ///
    /// # Errors
    ///
    /// Anything that is neither `env:NAME` nor an absolute `file:` path.
    pub fn parse(setting: &str, text: &str) -> Result<Self> {
        if let Some(name) = text.strip_prefix("env:") {
            let ok = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit());
            if !ok {
                return Err(Error(format!(
                    "{setting}: an `env:` reference names an upper-case variable (A-Z, 0-9, `_`)"
                )));
            }
            return Ok(Self::Env(name.to_owned()));
        }
        if let Some(path) = text.strip_prefix("file:") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(Error(format!(
                    "{setting}: a `file:` reference is an absolute path"
                )));
            }
            return Ok(Self::File(path));
        }
        Err(Error(format!(
            "{setting}: expected `env:NAME` or `file:/absolute/path`; the credential itself is never configured"
        )))
    }

    /// Reads the credential. The text of an error names the variable or file, never a value.
    ///
    /// # Errors
    ///
    /// A missing or empty variable or file, or (Unix) a file others can read.
    pub fn resolve(&self, setting: &str) -> Result<String> {
        match self {
            Self::Env(name) => match std::env::var(name) {
                Ok(value) if !value.is_empty() => Ok(value),
                _ => Err(Error(format!(
                    "{setting}: the environment variable `{name}` is not set or is empty"
                ))),
            },
            Self::File(path) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let meta = std::fs::metadata(path).map_err(|_| unreadable(setting, path))?;
                    if meta.permissions().mode() & 0o077 != 0 {
                        return Err(Error(format!(
                            "{setting}: the file `{}` is readable by its group or others; make it private (mode 0600)",
                            path.display()
                        )));
                    }
                }
                let text = std::fs::read_to_string(path).map_err(|_| unreadable(setting, path))?;
                let value = text.trim_end_matches(['\r', '\n']);
                if value.is_empty() {
                    return Err(Error(format!(
                        "{setting}: the file `{}` is empty",
                        path.display()
                    )));
                }
                Ok(value.to_owned())
            }
        }
    }
}

fn unreadable(setting: &str, path: &std::path::Path) -> Error {
    Error(format!(
        "{setting}: the file `{}` cannot be read",
        path.display()
    ))
}

/// Whether `endpoint` points at this machine (a local emulator).
fn is_loopback(endpoint: &url::Url) -> bool {
    match endpoint.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Checks an endpoint text: a URL with an `http` or `https` scheme; returns whether plain HTTP to
/// a remote host was explicitly allowed (`insecure`).
fn check_endpoint(
    setting: &str,
    endpoint: Option<&str>,
    allow_http: bool,
    allow_insecure_remote: bool,
    errors: &mut Vec<String>,
) -> bool {
    let Some(text) = endpoint else {
        // No endpoint: the real service over TLS; `allow_http` has nothing to apply to.
        if allow_http {
            errors.push(format!("{setting}.allow_http needs an endpoint"));
        }
        return false;
    };
    let Ok(url) = url::Url::parse(text) else {
        errors.push(format!("{setting}.endpoint is not a valid URL"));
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        errors.push(format!("{setting}.endpoint must be an http or https URL"));
        return false;
    }
    if url.scheme() == "http" && !allow_http {
        errors.push(format!(
            "{setting}.endpoint is plain HTTP: set allow_http (a loopback emulator only)"
        ));
    }
    if allow_http && !is_loopback(&url) {
        if allow_insecure_remote {
            return true;
        }
        errors.push(format!(
            "{setting}.allow_http on a remote endpoint is refused: set allow_insecure_remote = true to accept plain HTTP over a network (never for production)"
        ));
    }
    false
}

/// Every refusal of the provider settings that needs no I/O, by key: a sub-table for another
/// provider than the selected one, a missing bucket, container or account, a malformed
/// reference, plain HTTP to a remote endpoint. Empty for `local` and `memory` (their keys are
/// unchanged).
#[must_use]
pub fn validate(files: &Files) -> Vec<String> {
    let mut errors = Vec::new();
    let provider = files.provider.as_str();
    if files.s3.is_some() && provider != "s3" {
        errors.push("services.files.s3 is set but services.files.provider is not \"s3\"".into());
    }
    if files.azure.is_some() && provider != "azure" {
        errors.push(
            "services.files.azure is set but services.files.provider is not \"azure\"".into(),
        );
    }
    match provider {
        "s3" => match &files.s3 {
            None => errors.push(
                "services.files.s3: provider \"s3\" needs the [services.files.s3] table".into(),
            ),
            Some(s3) => validate_s3(s3, &mut errors),
        },
        "azure" => match &files.azure {
            None => errors.push(
                "services.files.azure: provider \"azure\" needs the [services.files.azure] table"
                    .into(),
            ),
            Some(az) => validate_azure(az, &mut errors),
        },
        _ => {}
    }
    errors
}

fn validate_s3(s3: &FilesS3, errors: &mut Vec<String>) {
    let table = "services.files.s3";
    if s3.bucket.is_empty() {
        errors.push(format!("{table}.bucket is required"));
    }
    check_endpoint(
        table,
        s3.endpoint.as_deref(),
        s3.allow_http,
        s3.allow_insecure_remote,
        errors,
    );
    for (name, value) in [
        ("access_key_id_ref", &s3.access_key_id_ref),
        ("secret_access_key_ref", &s3.secret_access_key_ref),
    ] {
        if let Some(Err(e)) = value
            .as_deref()
            .map(|text| CredentialRef::parse(&format!("{table}.{name}"), text))
        {
            errors.push(e.to_string());
        }
    }
    if s3.access_key_id_ref.is_some() != s3.secret_access_key_ref.is_some() {
        errors.push(format!(
            "{table}: set both access_key_id_ref and secret_access_key_ref, or neither (the process environment's AWS credentials are then used)"
        ));
    }
}

fn validate_azure(az: &FilesAzure, errors: &mut Vec<String>) {
    let table = "services.files.azure";
    if az.account.is_empty() {
        errors.push(format!("{table}.account is required"));
    }
    if az.container.is_empty() {
        errors.push(format!("{table}.container is required"));
    }
    if !matches!(az.credential_kind.as_str(), "access_key" | "sas") {
        errors.push(format!("{table}.credential_kind must be access_key or sas"));
    }
    match &az.credential_ref {
        None => errors.push(format!("{table}.credential_ref is required")),
        Some(text) => {
            if let Err(e) = CredentialRef::parse(&format!("{table}.credential_ref"), text) {
                errors.push(e.to_string());
            }
        }
    }
    check_endpoint(
        table,
        az.endpoint.as_deref(),
        az.allow_http,
        az.allow_insecure_remote,
        errors,
    );
}

/// Builds the store of `files.provider`: `memory`, `local` under `local_root`, or an object
/// provider from its table.
///
/// # Errors
///
/// An unknown provider (it used to mean `local` silently), a setting [`validate`] refuses, a
/// provider whose cargo feature is not compiled in, an unreadable credential, or a client the
/// provider cannot build. The text names `services.files.<key>` and never a credential.
pub fn open(files: &Files, local_root: PathBuf) -> Result<Opened> {
    let provider = files.provider.as_str();
    match provider {
        "memory" => Ok(Opened {
            store: Arc::new(MemoryFileStore::new()),
            object: None,
        }),
        "local" => Ok(Opened {
            store: Arc::new(LocalFileStore::new(local_root)),
            object: None,
        }),
        "s3" | "azure" => {
            if let Some(first) = validate(files).into_iter().next() {
                return Err(Error(first));
            }
            if provider == "s3" {
                open_s3(files)
            } else {
                open_azure(files)
            }
        }
        other => Err(Error(format!(
            "{}: unknown provider `{other}`; expected local, memory, s3 or azure",
            key("", "provider")
        ))),
    }
}

#[cfg(any(feature = "files-s3", feature = "files-azure"))]
fn parse_endpoint(setting: &str, endpoint: Option<&String>) -> Result<Option<url::Url>> {
    endpoint
        .map(|text| {
            url::Url::parse(text)
                .map_err(|_| Error(format!("{setting}.endpoint is not a valid URL")))
        })
        .transpose()
}

#[cfg(feature = "files-s3")]
fn open_s3(files: &Files) -> Result<Opened> {
    use spot_files::object::{ObjectFileStore, S3Config};
    let table = "services.files.s3";
    let s3 = files
        .s3
        .as_ref()
        .ok_or_else(|| Error(format!("{table} is missing")))?;
    let endpoint = parse_endpoint(table, s3.endpoint.as_ref())?;
    let resolve = |name: &str, value: &Option<String>| -> Result<Option<String>> {
        let setting = format!("{table}.{name}");
        value
            .as_deref()
            .map(|text| CredentialRef::parse(&setting, text)?.resolve(&setting))
            .transpose()
    };
    let config = S3Config {
        endpoint,
        region: s3.region.clone(),
        bucket: s3.bucket.clone(),
        access_key_id: resolve("access_key_id_ref", &s3.access_key_id_ref)?,
        secret_access_key: resolve("secret_access_key_ref", &s3.secret_access_key_ref)?,
        allow_http: s3.allow_http,
        path_style: s3.path_style,
    };
    let store = ObjectFileStore::new_s3(config).map_err(|e| {
        Error(format!(
            "{table}: the s3 client cannot be built: {}",
            e.code()
        ))
    })?;
    let insecure = s3.allow_http
        && s3
            .endpoint
            .as_deref()
            .is_some_and(|text| url::Url::parse(text).is_ok_and(|u| !is_loopback(&u)));
    Ok(Opened {
        store: Arc::new(store),
        object: Some(ObjectInfo {
            label: format!("s3 bucket {}", s3.bucket),
            insecure,
        }),
    })
}

#[cfg(not(feature = "files-s3"))]
fn open_s3(_files: &Files) -> Result<Opened> {
    Err(Error(
        "services.files.provider = \"s3\": this binary was built without the files-s3 feature"
            .into(),
    ))
}

#[cfg(feature = "files-azure")]
fn open_azure(files: &Files) -> Result<Opened> {
    use spot_files::object::{AzureConfig, AzureCredential, ObjectFileStore};
    let table = "services.files.azure";
    let az = files
        .azure
        .as_ref()
        .ok_or_else(|| Error(format!("{table} is missing")))?;
    let endpoint = parse_endpoint(table, az.endpoint.as_ref())?;
    let setting = format!("{table}.credential_ref");
    let reference = az
        .credential_ref
        .as_deref()
        .ok_or_else(|| Error(format!("{setting} is required")))?;
    let secret = CredentialRef::parse(&setting, reference)?.resolve(&setting)?;
    let credentials = if az.credential_kind == "sas" {
        AzureCredential::SasToken(secret)
    } else {
        AzureCredential::AccessKey(secret)
    };
    let config = AzureConfig {
        endpoint,
        account: az.account.clone(),
        container: az.container.clone(),
        credentials,
        allow_http: az.allow_http,
    };
    let store = ObjectFileStore::new_azure(config).map_err(|e| {
        Error(format!(
            "{table}: the azure client cannot be built: {}",
            e.code()
        ))
    })?;
    let insecure = az.allow_http
        && az
            .endpoint
            .as_deref()
            .is_some_and(|text| url::Url::parse(text).is_ok_and(|u| !is_loopback(&u)));
    Ok(Opened {
        store: Arc::new(store),
        object: Some(ObjectInfo {
            label: format!("azure container {}", az.container),
            insecure,
        }),
    })
}

#[cfg(not(feature = "files-azure"))]
fn open_azure(_files: &Files) -> Result<Opened> {
    Err(Error(
        "services.files.provider = \"azure\": this binary was built without the files-azure feature"
            .into(),
    ))
}

/// The start-up probe of an object provider, in the site's own prefix: put 16 bytes, read them
/// back and compare, delete. Each failure is a distinct message that names no key, endpoint or
/// credential; a probe that takes longer than `timeout` fails with `timeout`.
///
/// The key is `private/probe-<random>`: a [`FileKey`] segment cannot start with a dot, so the
/// brief's `.probe/` prefix is not expressible.
///
/// # Errors
///
/// `bucket not reachable`, `cannot write`, `cannot read back`, `cannot delete` or `timeout`.
pub async fn probe(
    store: &dyn FileStore,
    site: &SiteId,
    timeout: Duration,
) -> std::result::Result<(), String> {
    match tokio::time::timeout(timeout, probe_inner(store, site)).await {
        Ok(result) => result,
        Err(_) => Err("the file store probe timed out (timeout)".to_owned()),
    }
}

async fn probe_inner(store: &dyn FileStore, site: &SiteId) -> std::result::Result<(), String> {
    use spot_files::error::FileError;
    let id = spot_storage_contract::RecordId::new_v7().to_string();
    let file_key = FileKey::new(Visibility::Private, &format!("probe-{id}"))
        .map_err(|_| "the probe key is invalid".to_owned())?;
    let value: Vec<u8> = (0_u8..16).collect();
    let opts = PutOptions::new("application/octet-stream", 1024, false);
    if let Err(e) = store
        .put(site, file_key.clone(), body_from_vec(value.clone()), opts)
        .await
    {
        // A refusal the store itself names is not a reachability problem.
        return Err(match e {
            FileError::Io { .. } => "the file store cannot write (bucket not reachable)".to_owned(),
            _ => "the file store cannot write".to_owned(),
        });
    }
    let read = async {
        let (_, mut body) = store.get(site, file_key.clone()).await?;
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)
            .await
            .map_err(FileError::from)?;
        Ok::<_, FileError>(bytes)
    }
    .await;
    match read {
        Ok(bytes) if bytes == value => {}
        _ => {
            let _ = store.delete(site, file_key).await;
            return Err("the file store cannot read back what it wrote".to_owned());
        }
    }
    match store.delete(site, file_key).await {
        Ok(true) => Ok(()),
        _ => Err("the file store cannot delete its probe".to_owned()),
    }
}
