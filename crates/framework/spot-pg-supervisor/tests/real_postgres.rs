//! Real-PostgreSQL smoke test, run only when `SPOT_PG_BIN` names a bundled-style
//! PostgreSQL installation (`.../bin`). Not part of the default run (`#[ignore]`):
//! CI machines have no PostgreSQL (see the CI runner notes).
//!
//! ```sh
//! SPOT_PG_BIN=/usr/pgsql-18/bin cargo test -p spot-pg-supervisor -- --ignored
//! ```

mod common;

use common::{TestEnv, must};
use spot_pg_supervisor::Supervisor;
use spot_pg_supervisor::layout::PgLayout;
use std::time::Duration;

/// Initialise a real cluster, start it, connect by raw TCP, stop gracefully.
#[tokio::test]
#[ignore = "runs against a real PostgreSQL; set SPOT_PG_BIN to enable"]
async fn initialise_start_connect_stop() {
    let pg_bin = match std::env::var("SPOT_PG_BIN") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => panic!("SPOT_PG_BIN must name the real PostgreSQL bin directory"),
    };
    let env = TestEnv::new();
    let layout = PgLayout {
        bin_dir: pg_bin,
        data_dir: env.data_dir.clone(),
        log_dir: env.log_dir.clone(),
        listen: spot_pg_supervisor::Listen::Localhost,
        port: env.port,
    };
    let supervisor = Supervisor::new(layout, env.settings(), env.config());
    must(supervisor.ensure_initialised("real-pg-smoke").await);
    let handle = must(supervisor.start().await);
    must(handle.ready().await);

    // Raw TCP connectivity (a real backend would run `SELECT 1` here).
    let probe = tokio::net::TcpStream::connect(("127.0.0.1", env.port)).await;
    assert!(
        probe.is_ok(),
        "real postgres not reachable on the layout port"
    );

    must(handle.stop(Duration::from_secs(30)).await);
    common::assert_no_orphans(&env);
}
