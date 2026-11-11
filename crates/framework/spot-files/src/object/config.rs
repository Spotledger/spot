//! The provider settings: [`S3Config`] and [`AzureConfig`] with their credentials, each
//! redacting every secret in `Debug`.

use std::fmt;

use url::Url;

/// Settings for the S3-compatible provider (feature `s3`).
///
/// `endpoint`, when set, names the service base URL and switches away from AWS:
/// `http://127.0.0.1:9000` for a local MinIO, `https://<account>.r2.cloudflarestorage.com`
/// for R2. Static credentials (`access_key_id`/`secret_access_key`) override the builder
/// default, which falls back to the process environment (`AWS_ACCESS_KEY_ID`, …).
#[derive(Clone, PartialEq, Eq)]
pub struct S3Config {
    /// The service base URL; `None` selects real AWS with virtual-host addressing.
    pub endpoint: Option<Url>,
    /// The region, e.g. `us-east-1`; `None` leaves the choice to the builder defaults.
    pub region: Option<String>,
    /// The bucket, which must already exist (a `FileStore` never creates buckets).
    pub bucket: String,
    /// The static access key id, when not read from the environment.
    pub access_key_id: Option<String>,
    /// The static secret access key, when not read from the environment.
    pub secret_access_key: Option<String>,
    /// Allow plain HTTP endpoints, i.e. self-hosted emulators and services.
    pub allow_http: bool,
    /// Force bucket-in-path addressing (`http://…/<bucket>/<key>`) even for AWS itself.
    pub path_style: bool,
}

/// Debug never leaks credentials: both key fields render as `[secret]`.
impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &redact(self.access_key_id.as_deref()))
            .field(
                "secret_access_key",
                &redact(self.secret_access_key.as_deref()),
            )
            .field("allow_http", &self.allow_http)
            .field("path_style", &self.path_style)
            .finish()
    }
}

/// Credentials for the Azure provider (feature `azure`).
#[derive(Clone, PartialEq, Eq)]
pub enum AzureCredential {
    /// The account's shared key, the 88-character base64 secret.
    AccessKey(String),
    /// A shared-access-signature token in query-string form, optionally with a leading
    /// `?` (e.g. `sv=2023-11-03&ss=bf`); the pairs are decoded and handed to the client.
    SasToken(String),
}

/// Debug never leaks credentials.
impl fmt::Debug for AzureCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AzureCredential::AccessKey(_) => f.write_str("AccessKey([secret])"),
            AzureCredential::SasToken(_) => f.write_str("SasToken([secret])"),
        }
    }
}

/// Settings for the Azure Blob Storage provider (feature `azure`).
///
/// `endpoint`, when set, is the service base URL — for the Azurite emulator
/// `http://127.0.0.1:10000` (an `http` scheme selects object_store's emulator mode,
/// whose service URL comes from `AZURITE_BLOB_STORAGE_URL`, which callers then set);
/// `None` selects the real `https://<account>.blob.core.windows.net`.
#[derive(Clone, PartialEq, Eq)]
pub struct AzureConfig {
    /// The service base URL without the account; `None` selects real Azure Blob.
    pub endpoint: Option<Url>,
    /// The storage account name.
    pub account: String,
    /// The blob container, which must already exist (a `FileStore` never creates
    /// containers).
    pub container: String,
    /// How the provider authenticates with the account.
    pub credentials: AzureCredential,
    /// Allow plain HTTP endpoints, i.e. the Azurite emulator or a private gateway.
    pub allow_http: bool,
}

/// Debug never leaks credentials.
impl fmt::Debug for AzureConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureConfig")
            .field("endpoint", &self.endpoint)
            .field("account", &self.account)
            .field("container", &self.container)
            .field("credentials", &self.credentials)
            .field("allow_http", &self.allow_http)
            .finish()
    }
}

/// Renders credential text for debug output: present values collapse to `[secret]`.
fn redact(value: Option<&str>) -> &'static str {
    if value.is_some() { "[secret]" } else { "unset" }
}
