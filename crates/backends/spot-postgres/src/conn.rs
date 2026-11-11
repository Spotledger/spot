//! Connections: TLS setup, pool construction and one-off raw connections.

use std::sync::Arc;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use spot_storage_contract::{ErrorKind, StorageError, StorageResult};
use tokio_postgres::NoTls;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::config::{PgConfig, Tls};
use crate::error::map_pg_error;

/// The TLS connector for a config.
#[derive(Clone)]
pub(crate) enum TlsMode {
    Plain,
    Rustls(MakeRustlsConnect),
}

impl TlsMode {
    /// Builds the connector. `Require` loads the platform's root certificates.
    pub(crate) fn new(tls: Tls) -> StorageResult<Self> {
        match tls {
            Tls::Disable => Ok(Self::Plain),
            Tls::Require => {
                let mut roots = rustls::RootCertStore::empty();
                let loaded = rustls_native_certs::load_native_certs();
                for error in &loaded.errors {
                    tracing::warn!(%error, "could not load a platform root certificate");
                }
                for cert in loaded.certs {
                    if let Err(error) = roots.add(cert) {
                        tracing::warn!(%error, "ignoring an invalid platform root certificate");
                    }
                }
                if roots.is_empty() {
                    return Err(StorageError::new(
                        ErrorKind::Internal,
                        "TLS is required but no platform root certificates could be loaded",
                    ));
                }
                let provider = Arc::new(rustls::crypto::ring::default_provider());
                let client = rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .map_err(|_| {
                        StorageError::new(ErrorKind::Internal, "TLS protocol setup failed")
                    })?
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                Ok(Self::Rustls(MakeRustlsConnect::new(client)))
            }
        }
    }

    /// The connector a pool built with this mode uses, for a caller that must send a request over
    /// the same transport as the pool (the bridge's statement cancel).
    ///
    /// `None` for `Plain`: there is no connector, and a request over `NoTls` is then exactly what
    /// the pool itself does. `Some` is a clone of the one connector this mode holds, so the pool
    /// and the other request share the roots and the settings; a clone bumps a reference count —
    /// it opens no connection and loads no certificate.
    pub(crate) fn connector(&self) -> Option<MakeRustlsConnect> {
        match self {
            Self::Plain => None,
            Self::Rustls(connector) => Some(connector.clone()),
        }
    }
}

/// Builds the pool without opening a connection.
pub(crate) fn build_pool(config: &PgConfig, tls: &TlsMode) -> StorageResult<Pool> {
    let manager_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };
    let driver = config.driver_config();
    let manager = match tls {
        TlsMode::Plain => Manager::from_config(driver, NoTls, manager_config),
        TlsMode::Rustls(t) => Manager::from_config(driver, t.clone(), manager_config),
    };
    Pool::builder(manager)
        .max_size(config.pool_max_size)
        .wait_timeout(Some(config.pool_wait_timeout))
        .create_timeout(Some(config.connect_timeout))
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(|_| {
            StorageError::new(
                ErrorKind::Internal,
                "the connection pool could not be built",
            )
        })
}

/// Opens one connection outside any pool and starts its driver task. Dropping the client ends it.
pub(crate) async fn connect_raw(
    config: &PgConfig,
    tls: &TlsMode,
) -> StorageResult<tokio_postgres::Client> {
    let driver = config.driver_config();
    match tls {
        TlsMode::Plain => {
            let (client, connection) = driver
                .connect(NoTls)
                .await
                .map_err(|e| map_pg_error(&e, None))?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    tracing::debug!(%error, "raw connection ended");
                }
            });
            Ok(client)
        }
        TlsMode::Rustls(t) => {
            let (client, connection) = driver
                .connect(t.clone())
                .await
                .map_err(|e| map_pg_error(&e, None))?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    tracing::debug!(%error, "raw connection ended");
                }
            });
            Ok(client)
        }
    }
}
