//! Shared setup for the integration tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig, PgTransaction};
use spot_storage_contract::{
    Catalog, Collation, Isolation, Name, StorageBackend, StorageError, TxOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A scratch database and a backend on it, or `None` when no server is configured.
pub struct Env {
    pub db: TestDb,
    pub backend: PgBackend,
}

pub async fn env() -> Option<Env> {
    env_with(|_| {}).await
}

pub async fn env_with(tweak: impl FnOnce(&mut PgConfig)) -> Option<Env> {
    let db = TestDb::create().await?;
    let mut config = db.config();
    tweak(&mut config);
    let backend = PgBackend::connect(config).await.expect("connect");
    Some(Env { db, backend })
}

pub fn catalog() -> Arc<Catalog> {
    Arc::new(Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap())
}

pub fn options(label: &str, isolation: Isolation) -> TxOptions {
    TxOptions::new(Name::new(label).unwrap(), isolation)
}

impl Env {
    pub async fn begin(&self, options: TxOptions) -> Result<PgTransaction, StorageError> {
        self.backend.begin(catalog(), options).await
    }

    /// Creates the scratch table `t(id int primary key, v int not null)` with rows 1 and 2.
    pub async fn table(&self) {
        let c = self.db.raw_client().await;
        c.batch_execute(
            "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
             INSERT INTO t VALUES (1, 10), (2, 20);",
        )
        .await
        .unwrap();
    }

    pub async fn value(&self, id: i32) -> Option<i32> {
        let c = self.db.raw_client().await;
        c.query_opt("SELECT v FROM t WHERE id = $1", &[&id])
            .await
            .unwrap()
            .map(|r| r.get(0))
    }
}

/// A TCP forwarder to the test server that kills both sockets the moment it sees a `COMMIT`
/// statement, without forwarding it: the client loses its connection while committing, and the
/// server never sees the commit. Returns the proxy's port.
pub async fn commit_killing_proxy(target: &PgConfig) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = format!("{}:{}", target.host, target.port);
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let target = target.clone();
            tokio::spawn(async move {
                let Ok(mut server) = TcpStream::connect(&target).await else {
                    return;
                };
                let (mut cr, mut cw) = client.split();
                let (mut sr, mut sw) = server.split();
                let up = async {
                    let mut buf = [0u8; 8192];
                    loop {
                        let n = cr.read(&mut buf).await.unwrap_or(0);
                        if n == 0 || buf[..n].windows(7).any(|w| w == b"COMMIT\0") {
                            return;
                        }
                        if sw.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                };
                let down = async {
                    let mut buf = [0u8; 8192];
                    loop {
                        let n = sr.read(&mut buf).await.unwrap_or(0);
                        if n == 0 || cw.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                };
                tokio::select! { () = up => {}, () = down => {} }
            });
        }
    });
    port
}

pub const MARGIN: Duration = Duration::from_millis(1500);
