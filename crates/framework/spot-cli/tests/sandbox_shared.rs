//! The registry and destroyer shared by `spot sandbox` and the hosted reaper (WP-ALM-05.T6): the
//! library functions called directly, and the command's and the reaper's views agreeing with them.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::task::{Context, Poll, Waker};

use spot_cli::services::sandbox_reaper::{FileRegistry, Registry};
use spot_core::SiteId;
use spot_sandbox::registry::{
    AdminFuture, DatabaseAdmin, Destroyed, Entry, RECORD_FILE, STALE_CREATING, destroy_sandbox,
    is_plain_site_name, list_entries, list_records,
};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

const RECORD: &str = "source = \"prod\"\ndatabase = \"sandbox_qa\"\ncreated_at = 100\nexpires_at = 200\nstate = \"active\"\nbanner = true\n";

struct AlwaysThere;
impl DatabaseAdmin for AlwaysThere {
    fn drop_database_if_exists<'a>(&'a self, _database: &'a str) -> AdminFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

fn root_with_one_sandbox() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("sites").join("qa");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(RECORD_FILE), RECORD).unwrap();
    root
}

#[test]
fn the_library_reads_and_destroys_directly() {
    let root = root_with_one_sandbox();
    let records = list_records(root.path()).unwrap();
    assert_eq!(records.len(), 1);
    let (name, dir, record) = &records[0];
    assert_eq!(name, "qa");
    assert_eq!(record.database, "sandbox_qa");
    let entry = Entry::from_record(name, record);
    assert!(!entry.keep);
    let outcome = block_on(destroy_sandbox(root.path(), &AlwaysThere, &entry));
    assert_eq!(outcome, Ok(Destroyed::Gone));
    assert!(!dir.exists(), "the folder and its record are gone");
}

#[test]
fn the_reaper_registry_is_the_library_listing() {
    let root = root_with_one_sandbox();
    let real: BTreeSet<String> = ["prod_db".to_owned()].into();
    let direct = list_entries(root.path(), &real);
    let registry = FileRegistry::new(root.path().to_path_buf(), real);
    let through_port = block_on(registry.list()).unwrap();
    assert_eq!(direct, through_port);
    assert_eq!(through_port.entries[0].name, "qa");
}

#[test]
fn the_reaper_and_the_command_share_the_constants() {
    assert_eq!(spot_cli::services::sandbox_reaper::RECORD_FILE, RECORD_FILE);
    assert_eq!(
        spot_cli::services::sandbox_reaper::STALE_CREATING,
        STALE_CREATING
    );
}

#[test]
fn the_site_name_rule_mirrors_site_id() {
    let long_label = "a".repeat(64);
    let long_name = format!(
        "{}.{}.{}.{}.{}",
        "a".repeat(60),
        "b".repeat(60),
        "c".repeat(60),
        "d".repeat(60),
        "e".repeat(60)
    );
    let samples = [
        "",
        ".",
        "..",
        "qa",
        "qa.example.com",
        "a..b",
        "-a",
        "a-",
        "a-b",
        "A",
        "a/b",
        "a_b",
        "a b",
        "a.",
        ".a",
        "../x",
        "x1",
        "1x",
        long_label.as_str(),
        long_name.as_str(),
    ];
    for name in samples {
        assert_eq!(
            is_plain_site_name(name),
            SiteId::new(name).is_ok(),
            "{name:?}"
        );
    }
}
