//! Starting the server from a configuration file: the PostgreSQL stores on one shared bridge,
//! the filesystem blob store, the keys, and the `serve --config` command.
//!
//! Everything that touches PostgreSQL is built on the main thread before the async runtime
//! starts: the shared bridge refuses to be built or called from inside a runtime thread.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use spot_bundle_signing::{PublicKey, TrustedRoot};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::{PgBackend, PgConfig};
use spot_registry::{BlobStore, BlobValidation, FsBlobStore, IndexStore};
use spot_registry_pg::PgIndexStore;
use spot_store_state::StoreStateStore;
use spot_store_state_pg::PgStoreState;

use crate::clock::SystemClock;
use crate::config::{Config, StartupError, TrustRootConfig};
use crate::keys::{AdminCredential, StoreKey};
use crate::router::serve;
use crate::service::{Service, ServiceParts};

/// A running deployment's parts that must outlive the service.
pub struct Deployment {
    /// The service.
    pub service: Arc<Service>,
    bridge: Arc<Bridge>,
    _backend: PgBackend,
}

impl Deployment {
    /// Shuts the shared bridge down (waits up to `grace` for calls in flight). Call it from a
    /// plain thread, after the runtime has stopped.
    pub fn close(&self, grace: Duration) {
        self.service.shutdown();
        let _ = self.bridge.shutdown(grace);
    }
}

fn load_roots(roots: &[TrustRootConfig]) -> Result<Vec<TrustedRoot>, StartupError> {
    let bad = |what: &str| StartupError::config(format!("a trust root public key file {what}"));
    let mut loaded = Vec::with_capacity(roots.len());
    for root in roots {
        let text =
            std::fs::read_to_string(&root.public_key_file).map_err(|_| bad("cannot be read"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(|_| bad("is not base64"))?;
        let key = PublicKey::from_bytes(&bytes).map_err(|_| bad("is not an Ed25519 public key"))?;
        loaded.push(TrustedRoot::new(key, root.valid_from, root.valid_to));
    }
    Ok(loaded)
}

/// Removes upload files a crashed process left behind. Runs before the server accepts anything.
fn clean_temp_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let stale = name
            .to_str()
            .is_some_and(|name| name.starts_with("upload-") && name.ends_with(".part"));
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn prepare_dirs(config: &Config) -> Result<(), StartupError> {
    std::fs::create_dir_all(&config.temp_dir)
        .map_err(|_| StartupError::startup("the temp directory cannot be created"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.temp_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| StartupError::startup("the temp directory cannot be made private"))?;
    }
    clean_temp_dir(&config.temp_dir);
    Ok(())
}

/// Builds the service over PostgreSQL and the filesystem blob store.
///
/// # Errors
/// `store_server.config`, `store_server.key_file` or `store_server.startup`, each with a sentence
/// that names no secret. A missing store key or operator secret fails here, before any socket.
pub fn build_deployment(config: Config) -> Result<Deployment, StartupError> {
    config.validate()?;
    let store_key = StoreKey::from_file(&config.store_key_file)?;
    let admin = AdminCredential::from_file(&config.admin_key_file)?;
    let roots = load_roots(&config.trust_roots)?;
    let url = config.database_url_text()?;
    let mut pg = PgConfig::from_url(&url)
        .map_err(|_| StartupError::config("the database URL is not a valid postgres URL"))?;
    pg.pool_max_size = config.database.pool_size;
    let backend = PgBackend::connect_unchecked(pg)
        .map_err(|_| StartupError::startup("the database pool could not be set up"))?;
    let bridge = Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "store_server".to_owned(),
                workers: 2,
                max_in_flight: config.database.max_in_flight,
                queue_capacity: config.database.queue_capacity,
                default_deadline: Duration::from_secs(5),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .map_err(|_| StartupError::startup("the database bridge refused its configuration"))?,
    );
    let pool = backend.pool();
    let index = Arc::new(
        PgIndexStore::new(Arc::clone(&bridge), &pool)
            .map_err(|_| StartupError::startup("the registry index could not be built"))?,
    );
    index
        .prepare()
        .map_err(|_| StartupError::startup("the registry index tables could not be prepared"))?;
    let state = Arc::new(
        PgStoreState::new(Arc::clone(&bridge), &pool)
            .map_err(|_| StartupError::startup("the store state could not be built"))?,
    );
    state
        .prepare()
        .map_err(|_| StartupError::startup("the store state tables could not be prepared"))?;
    let blobs = FsBlobStore::new(&config.blob_dir, BlobValidation::default())
        .map_err(|_| StartupError::startup("the blob directory cannot be used"))?;
    prepare_dirs(&config)?;
    let index: Arc<dyn IndexStore> = index;
    let state: Arc<dyn StoreStateStore> = state;
    let blobs: Arc<dyn BlobStore> = Arc::new(blobs);
    let service = Service::new(ServiceParts {
        config,
        index,
        blobs,
        state,
        clock: Arc::new(SystemClock),
        store_key,
        admin,
        trust_roots: roots,
    })?;
    Ok(Deployment {
        service,
        bridge,
        _backend: backend,
    })
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

fn serve_command(path: &Path) -> Result<(), StartupError> {
    let config = Config::load(path)?;
    let address = config.listen_addr()?;
    let deployment = build_deployment(config)?;
    deployment
        .service
        .spawn_maintenance()
        .map_err(|_| StartupError::startup("the maintenance thread could not be started"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| StartupError::startup("the async runtime could not be started"))?;
    let service = Arc::clone(&deployment.service);
    let outcome = runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|_| StartupError::startup("the listen address cannot be bound"))?;
        eprintln!(
            "{}",
            serde_json::json!({ "event": "listening", "address": address.to_string() })
        );
        serve(service, listener, shutdown_signal())
            .await
            .map_err(|_| StartupError::startup("the server stopped on an I/O error"))
    });
    drop(runtime);
    deployment.close(Duration::from_secs(10));
    outcome
}

/// The binary's command line: `spot-store-server serve --config <file>`. Returns the exit code.
#[must_use]
pub fn run_cli(args: &[String]) -> i32 {
    let usage = "usage: spot-store-server serve --config <file>";
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let path = match rest.as_slice() {
        ["serve", "--config", path] => Path::new(path),
        _ => {
            eprintln!("{usage}");
            return 2;
        }
    };
    match serve_command(path) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
