//! Shared setup for the tests: a scratch database, a pool and small model builders.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use semver::Version;
use spot_bundle::Digest;
use spot_core::SiteId;
use spot_installer::{
    GrantedCapabilities, InstallChange, InstallJournal, InstalledApp, StepId, StepKind, StepState,
};
use spot_installer_pg::PgAppRegistry;
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

/// A pool of `size` connections whose server-side `application_name` is `app`.
pub fn pool(db: &TestDb, size: usize, app: &str) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

/// A prepared registry on a new pool.
pub async fn registry(db: &TestDb, size: usize, app: &str) -> PgAppRegistry {
    let registry = PgAppRegistry::new(pool(db, size, app));
    registry.prepare().await.unwrap();
    registry
}

pub fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

/// A digest whose 32 bytes are all `byte`.
pub fn digest(byte: u8) -> Digest {
    format!("{byte:02x}").repeat(32).parse().unwrap()
}

pub fn row(app: &str, index: u32, state: StepState) -> InstallJournal {
    InstallJournal::new(
        "plan-hash",
        StepId::new(index, StepKind::Commit, app),
        state,
        0,
        None,
    )
}

pub fn install_change(byte: u8) -> InstallChange {
    InstallChange {
        version: Version::new(1, 0, 0),
        digest: digest(byte),
        approved: GrantedCapabilities::default(),
        applied_patches: Vec::new(),
    }
}

pub fn installed(app: &str, byte: u8) -> InstalledApp {
    InstalledApp::new(app, Version::new(1, 0, 0), digest(byte))
}
