//! A `TestHost` over the PostgreSQL backend, for the contract and L3 suites.
//!
//! Each case gets its own scratch database (kept alive by the fixture until the run ends), so
//! the outbox and the documents of one case never leak into another. Tests using this module
//! must run on a multi-threaded runtime: the host's synchronous inspection methods block on the
//! engine through [`Blocking`](crate::host::Blocking).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use spot_app_api::contract_tests::{Fixture, TestHost};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;

use crate::host::{Block, Blocking, Host};

/// A fixture handing each case a fresh scratch database.
pub struct Postgres {
    databases: Mutex<Vec<TestDb>>,
}

impl Postgres {
    pub fn new() -> Self {
        Self {
            databases: Mutex::new(Vec::new()),
        }
    }
}

impl Fixture for Postgres {
    fn fresh(&self) -> Box<dyn TestHost> {
        let db = Blocking::run(TestDb::create()).expect("a test database");
        let backend = Blocking::run(PgBackend::connect(db.config())).expect("connect");
        self.databases.lock().unwrap().push(db);
        Box::new(Host::<PgBackend, Blocking>::with_backend(backend))
    }
}

/// Whether a PostgreSQL server is configured; creates and drops a probe database, printing
/// `SKIPPED:` (from [`TestDb::create`]) when there is none.
pub fn available() -> bool {
    match Blocking::run(TestDb::create()) {
        Some(db) => {
            drop(db);
            true
        }
        None => false,
    }
}
