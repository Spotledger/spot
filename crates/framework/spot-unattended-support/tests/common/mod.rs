//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};

use spot_unattended_support::consent::ConsentStore;
use spot_unattended_support::crash::{CrashContext, CrashReport};
use spot_unattended_support::scrub::Scrubber;

/// A fresh empty folder under the target directory.
pub fn temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("unattended-{label}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn context() -> CrashContext {
    let mut config_summary = BTreeMap::new();
    config_summary.insert("deployment".to_owned(), "single".to_owned());
    CrashContext {
        version: "1.4.2".to_owned(),
        platform: "linux-x86_64".to_owned(),
        build_id: "abc123".to_owned(),
        config_summary,
        deployment_model_digest: "digest-1".to_owned(),
    }
}

pub fn scrubber() -> Scrubber {
    Scrubber::new(&["/home/alice".to_owned()], &["alice".to_owned()]).expect("scrubber")
}

pub fn report(location: &str, version: &str, frames: &[&str]) -> CrashReport {
    CrashReport {
        format: 1,
        version: version.to_owned(),
        platform: "linux-x86_64".to_owned(),
        build_id: "abc123".to_owned(),
        panic_location: location.to_owned(),
        backtrace: frames.iter().map(|f| (*f).to_owned()).collect(),
        breadcrumbs: Vec::new(),
        config_summary: BTreeMap::new(),
        deployment_model_digest: "digest-1".to_owned(),
        occurred_at: 0,
    }
}

/// Takes the consent lock on another thread and holds it until the returned sender is dropped,
/// so an operation that needs consent meanwhile meets a busy lock rather than a free one.
pub fn hold_consent_lock(store: &Arc<ConsentStore>) -> (Sender<()>, std::thread::JoinHandle<()>) {
    let (locked_tx, locked_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    let store = store.clone();
    let handle = std::thread::spawn(move || {
        store.with_state(|_| {
            locked_tx.send(()).expect("report the lock is held");
            let _ = release_rx.recv();
        });
    });
    locked_rx.recv().expect("the other thread took the lock");
    (release_tx, handle)
}
