//! The registry and the destroyer (WP-ALM-05.T6): reading the per-sandbox records, the lenient
//! listing the reaper uses, and idempotent destruction over a fake database server.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use spot_sandbox::registry::{
    AdminFuture, DatabaseAdmin, Destroyed, Entry, RECORD_FILE, ReadError, Record, destroy_sandbox,
    is_plain_name, is_plain_site_name, list_entries, list_records, read_record,
};

/// A scratch root removed on drop.
struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "spot-sandbox-registry-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(dir.join("sites")).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, site: &str, text: &str) {
        let dir = self.0.join("sites").join(site);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(RECORD_FILE), text).unwrap();
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn record_text(source: &str, database: &str, expires_at: i64) -> String {
    format!(
        "source = \"{source}\"\ndatabase = \"{database}\"\ncreated_at = 100\nexpires_at = {expires_at}\nstate = \"active\"\nbanner = true\n"
    )
}

/// Runs a future whose every await is ready at once (the fakes below never pend).
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

/// A database server holding a set of database names.
struct FakeServer {
    databases: Mutex<BTreeSet<String>>,
    fail: Option<&'static str>,
}

impl FakeServer {
    fn with(names: &[&str]) -> Self {
        Self {
            databases: Mutex::new(names.iter().map(|n| (*n).to_owned()).collect()),
            fail: None,
        }
    }

    fn has(&self, name: &str) -> bool {
        self.databases.lock().unwrap().contains(name)
    }
}

impl DatabaseAdmin for FakeServer {
    fn drop_database_if_exists<'a>(&'a self, database: &'a str) -> AdminFuture<'a, bool> {
        Box::pin(async move {
            if let Some(code) = self.fail {
                return Err(code);
            }
            Ok(self.databases.lock().unwrap().remove(database))
        })
    }
}

fn entry(name: &str, database: &str) -> Entry {
    Entry {
        name: name.to_owned(),
        source: "prod".to_owned(),
        database: database.to_owned(),
        created_at: 100,
        expires_at: 200,
        state: "active".to_owned(),
        keep: false,
    }
}

#[test]
fn a_record_is_read_and_a_missing_one_is_none() {
    let root = Root::new();
    root.write("qa", &record_text("prod", "sandbox_qa", 500));
    let dir = root.path().join("sites").join("qa");
    let record = read_record(&dir).unwrap().unwrap();
    assert_eq!(
        record,
        Record {
            source: "prod".to_owned(),
            database: "sandbox_qa".to_owned(),
            created_at: 100,
            expires_at: 500,
            state: "active".to_owned(),
            banner: true,
        }
    );
    assert!(
        read_record(&root.path().join("sites").join("none"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_record_that_does_not_parse_is_an_error_without_content() {
    let root = Root::new();
    root.write("bad", "source = \"secret-value\"\nunknown = 1\n");
    let error = read_record(&root.path().join("sites").join("bad")).unwrap_err();
    assert!(matches!(error, ReadError::Unparsable));
    let text = error.to_string();
    assert!(!text.contains("secret-value") && !text.contains("bad"));
    assert!(
        list_records(root.path()).is_err(),
        "the strict listing refuses it"
    );
}

#[test]
fn the_strict_listing_is_sorted_and_a_root_without_sites_is_empty() {
    let root = Root::new();
    root.write("b", &record_text("prod", "sandbox_b", 1));
    root.write("a", &record_text("prod", "sandbox_a", 1));
    std::fs::create_dir_all(root.path().join("sites").join("real")).unwrap();
    let names: Vec<String> = list_records(root.path())
        .unwrap()
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    assert_eq!(
        names,
        ["a", "b"],
        "a folder without a record is not a sandbox"
    );
    let empty = std::env::temp_dir().join("spot-sandbox-registry-no-such-root");
    assert!(list_records(&empty).unwrap().is_empty());
}

#[test]
fn the_lenient_listing_leaves_unreadable_records_alone_and_honours_keep() {
    let root = Root::new();
    root.write("good", &record_text("prod", "sandbox_good", 9));
    root.write("bad", "this is not toml [");
    root.write(
        "kept",
        "source = \"prod\"\ndatabase = \"sandbox_kept\"\ncreated_at = 1\nkeep = true\n",
    );
    let real: BTreeSet<String> = ["prod_db".to_owned()].into();
    let listing = list_entries(root.path(), &real);
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["good", "kept"]);
    assert!(listing.entries[1].keep);
    assert_eq!(
        listing.entries[1].expires_at, 0,
        "no expiry when none is written"
    );
    assert_eq!(listing.real_databases, real);
}

#[test]
fn destroying_drops_the_database_then_the_folder() {
    let root = Root::new();
    root.write("qa", &record_text("prod", "sandbox_qa", 5));
    let server = FakeServer::with(&["sandbox_qa", "prod_db"]);
    let outcome = block_on(destroy_sandbox(
        root.path(),
        &server,
        &entry("qa", "sandbox_qa"),
    ));
    assert_eq!(outcome, Ok(Destroyed::Gone));
    assert!(!server.has("sandbox_qa"));
    assert!(server.has("prod_db"), "only the named database goes");
    assert!(!root.path().join("sites").join("qa").exists());
}

#[test]
fn destroying_is_idempotent() {
    let root = Root::new();
    let server = FakeServer::with(&[]);
    let e = entry("qa", "sandbox_qa");
    // Database gone, folder still there: the job is finished.
    root.write("qa", &record_text("prod", "sandbox_qa", 5));
    assert_eq!(
        block_on(destroy_sandbox(root.path(), &server, &e)),
        Ok(Destroyed::Gone)
    );
    // Folder gone, database still there.
    let server = FakeServer::with(&["sandbox_qa"]);
    assert_eq!(
        block_on(destroy_sandbox(root.path(), &server, &e)),
        Ok(Destroyed::Gone)
    );
    // Neither: already done.
    assert_eq!(
        block_on(destroy_sandbox(root.path(), &server, &e)),
        Ok(Destroyed::AlreadyGone)
    );
}

#[test]
fn only_a_plain_sandbox_database_and_site_are_destroyed() {
    let root = Root::new();
    let server = FakeServer::with(&["prod_db", "sandbox_ok"]);
    let refused = |e: Entry| block_on(destroy_sandbox(root.path(), &server, &e));
    assert_eq!(
        refused(entry("qa", "prod_db")),
        Err("not_a_sandbox_database")
    );
    assert_eq!(refused(entry("qa", "Sandbox_X")), Err("invalid_database"));
    assert_eq!(
        refused(entry("qa", "sandbox_a\"; drop")),
        Err("invalid_database")
    );
    assert_eq!(refused(entry("qa", "")), Err("invalid_database"));
    assert_eq!(
        refused(entry("../escape", "sandbox_ok")),
        Err("invalid_site")
    );
    assert_eq!(refused(entry("", "sandbox_ok")), Err("invalid_site"));
    assert!(
        server.has("prod_db") && server.has("sandbox_ok"),
        "nothing was dropped"
    );
}

#[test]
fn a_server_failure_is_a_code_and_the_folder_stays() {
    let root = Root::new();
    root.write("qa", &record_text("prod", "sandbox_qa", 5));
    let mut server = FakeServer::with(&["sandbox_qa"]);
    server.fail = Some("database_unreachable");
    assert_eq!(
        block_on(destroy_sandbox(
            root.path(),
            &server,
            &entry("qa", "sandbox_qa")
        )),
        Err("database_unreachable")
    );
    assert!(
        root.path()
            .join("sites")
            .join("qa")
            .join(RECORD_FILE)
            .exists(),
        "the record stays so a later pass retries"
    );
}

#[test]
fn the_name_rules() {
    assert!(is_plain_name("sandbox_a1"));
    assert!(!is_plain_name("Sandbox"));
    assert!(!is_plain_name(&"a".repeat(64)));
    assert!(is_plain_site_name("qa.example.com"));
    assert!(is_plain_site_name("qa-1"));
    for bad in ["", ".", "..", "a..b", "-a", "a-", "A", "a/b", "a_b", "a b"] {
        assert!(!is_plain_site_name(bad), "{bad:?}");
    }
    assert!(!is_plain_site_name(&"a".repeat(64)));
}
