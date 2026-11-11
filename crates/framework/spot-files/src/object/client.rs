//! Construction of the shared [`ObjectFileStore`] from a provider config: each backend
//! builder is compiled only under its own feature, and everything the store does past
//! construction lives in [`filestore`](super::filestore).

use std::fmt;
use std::sync::Arc;

use object_store::ObjectStore;

#[cfg(feature = "s3")]
use super::config::S3Config;
#[cfg(feature = "azure")]
use super::config::{AzureConfig, AzureCredential};
#[cfg(feature = "azure")]
use url::Url;

use super::{Backend, ObjectFileStore};
use crate::error::FileError;
use crate::store::FileResult;

impl ObjectFileStore {
    /// Builds the S3-compatible provider (feature `s3`).
    ///
    /// Conditional creates become `If-None-Match: *` requests
    /// (`S3ConditionalPut::ETagMatch`), the behaviour emulators and real AWS agree on;
    /// with an `endpoint` set, addressing is bucket-in-path, which self-hosted services
    /// require.
    #[cfg(feature = "s3")]
    pub fn new_s3(config: S3Config) -> FileResult<Self> {
        use object_store::aws::{AmazonS3Builder, S3ConditionalPut};

        tracing::info!(
            bucket = %config.bucket,
            endpoint = ?config.endpoint,
            "building the s3 file store"
        );
        let mut builder = AmazonS3Builder::new().with_bucket_name(&config.bucket);
        builder = builder.with_conditional_put(S3ConditionalPut::ETagMatch);
        if let Some(endpoint) = &config.endpoint {
            builder = builder.with_endpoint(endpoint.as_str());
        }
        if let Some(region) = &config.region {
            builder = builder.with_region(region);
        }
        if let Some(access_key_id) = &config.access_key_id {
            builder = builder.with_access_key_id(access_key_id);
        }
        if let Some(secret_access_key) = &config.secret_access_key {
            builder = builder.with_secret_access_key(secret_access_key);
        }
        // Virtual-host addressing (`<bucket>.<host>`) is only correct for real AWS with
        // no endpoint; everyone else gets bucket-in-path.
        builder = builder
            .with_virtual_hosted_style_request(config.endpoint.is_none() && !config.path_style);
        if config.allow_http {
            builder = builder.with_allow_http(true);
        }
        let store = builder
            .build()
            .map_err(|e| FileError::io(format!("building the s3 object store: {e}")))?;
        Ok(Self::from_parts(Backend::S3, store))
    }

    /// Builds the Azure Blob provider (feature `azure`).
    ///
    /// A plain `http` endpoint selects object_store's emulator mode (Azurite), which
    /// takes its service URL from the `AZURITE_BLOB_STORAGE_URL` environment (default
    /// `http://127.0.0.1:10000`); the configured endpoint must match, so callers set
    /// that environment to their endpoint.
    #[cfg(feature = "azure")]
    pub fn new_azure(config: AzureConfig) -> FileResult<Self> {
        use object_store::azure::MicrosoftAzureBuilder;

        tracing::info!(
            account = %config.account,
            container = %config.container,
            endpoint = ?config.endpoint,
            "building the azure file store"
        );
        let mut builder = MicrosoftAzureBuilder::new()
            .with_account(config.account.clone())
            .with_container_name(config.container.clone());
        if let Some(endpoint) = &config.endpoint {
            if endpoint.scheme() == "http" {
                // The emulator (Azurite): the standard branch only takes `https` URLs,
                // while the emulator branch reads its service URL from the
                // `AZURITE_BLOB_STORAGE_URL` environment (object_store's documented
                // mechanism, with its own `http://127.0.0.1:10000` default) and
                // resolves the account from the URL path, which the emulator expects.
                // The two endpoints must agree, so a mismatch fails at build time
                // instead of silently driving another instance.
                let default_url = Url::parse("http://127.0.0.1:10000")
                    .map_err(|e| FileError::io(format!("parsing the emulator URL default: {e}")))?;
                let configured_url = match std::env::var("AZURITE_BLOB_STORAGE_URL") {
                    Ok(text) => Some(Url::parse(&text).map_err(|e| {
                        FileError::io(format!("AZURITE_BLOB_STORAGE_URL is not a URL: {e}"))
                    })?),
                    Err(_) => None,
                };
                let service_url = configured_url.as_ref().unwrap_or(&default_url);
                if service_url != endpoint {
                    return Err(FileError::io(format!(
                        "the Azure endpoint {endpoint} does not match AZURITE_BLOB_STORAGE_URL \
                         ({service_url}: an http endpoint switches to the Azurite emulator, \
                         whose service URL object_store takes from the environment)"
                    )));
                }
                builder = builder.with_use_emulator(true);
            } else {
                builder = builder.with_url(endpoint.as_str());
            }
        }
        builder = match &config.credentials {
            AzureCredential::AccessKey(key) => builder.with_access_key(key),
            AzureCredential::SasToken(token) => builder.with_sas_authorization(sas_pairs(token)?),
        };
        if config.allow_http {
            builder = builder.with_allow_http(true);
        }
        let store = builder
            .build()
            .map_err(|e| FileError::io(format!("building the azure object store: {e}")))?;
        Ok(Self::from_parts(Backend::Azure, store))
    }

    /// Finishes a store behind the object-safe [`ObjectStore`] trait object.
    pub(super) fn from_parts(backend: Backend, store: impl ObjectStore + 'static) -> Self {
        Self {
            store: Arc::new(store),
            backend,
        }
    }
}

/// Debug shows which backend was built and nothing about its configuration.
impl fmt::Debug for ObjectFileStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectFileStore")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ObjectFileStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "an object store ({})", self.backend)
    }
}

/// Splits a SAS token's query-string form into its pairs for `with_sas_authorization`.
#[cfg(feature = "azure")]
fn sas_pairs(token: &str) -> FileResult<Vec<(String, String)>> {
    let text = token.trim_start_matches('?');
    let mut pairs: Vec<(String, String)> = Vec::new();
    for segment in text.split('&') {
        if segment.is_empty() {
            continue;
        }
        let Some((name, value)) = segment.split_once('=') else {
            return Err(FileError::io(format!(
                "the SAS token is not a query string: {segment:?} has no '='"
            )));
        };
        pairs.push((name.to_owned(), value.to_owned()));
    }
    Ok(pairs)
}
