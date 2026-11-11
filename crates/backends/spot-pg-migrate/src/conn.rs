//! One dedicated connection per call, built from the backend's public config. A migration holds
//! a session advisory lock, so it must keep one connection for its whole run.

use std::sync::Arc;

use spot_postgres::{PgBackend, Tls};
use tokio_postgres::{Client, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::error::{MigrateError, db};

fn tls_connector() -> Result<MakeRustlsConnect, MigrateError> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(MigrateError::Database {
            message: "TLS is required but no platform root certificates could be loaded".into(),
        });
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| MigrateError::Database {
            message: "TLS protocol setup failed".into(),
        })?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// Opens a connection to the database `backend` is configured for and starts its driver task.
pub(crate) async fn connect(backend: &PgBackend) -> Result<Client, MigrateError> {
    let cfg = backend.config();
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&cfg.host)
        .port(cfg.port)
        .dbname(&cfg.dbname)
        .user(&cfg.user)
        .password(&cfg.password)
        .application_name("spot-migrate")
        .connect_timeout(cfg.connect_timeout);
    match cfg.tls {
        Tls::Disable => {
            let (client, connection) = driver.connect(NoTls).await.map_err(|e| db(&e))?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            Ok(client)
        }
        Tls::Require => {
            let (client, connection) =
                driver.connect(tls_connector()?).await.map_err(|e| db(&e))?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            Ok(client)
        }
    }
}
