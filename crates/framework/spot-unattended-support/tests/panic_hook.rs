//! The panic hook spools a scrubbed report without the panic message. This file has one test
//! because the hook is process-wide.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use spot_unattended_support::consent::{Consent, ConsentKind, ConsentStore};
use spot_unattended_support::crash::{CrashCollector, install_panic_hook};
use spot_unattended_support::spool::Spool;

#[test]
fn a_panic_is_spooled_when_consent_allows_and_never_carries_its_message() {
    let dir = common::temp_dir("hook");
    let spool = Arc::new(Spool::new(&dir));
    let consent = Arc::new(ConsentStore::first_run());
    let collector = Arc::new(CrashCollector::new(common::context(), common::scrubber()));
    collector.breadcrumb("started");
    install_panic_hook(collector, consent.clone(), spool.clone(), || 1_234);

    // Ask: the report is spooled for review, with the time the host supplied.
    let _ = std::panic::catch_unwind(|| panic!("customer bob@example.com owes 100"));
    let listed = spool.list().expect("list");
    assert_eq!(listed.len(), 1);
    let text = spool.view(&listed[0].id).expect("view");
    assert!(
        !text.contains("bob@example.com") && !text.contains("owes 100"),
        "{text}"
    );
    assert!(
        text.contains("panic_hook.rs"),
        "the location is kept: {text}"
    );
    assert_eq!(spool.read(&listed[0].id).expect("read").occurred_at, 1_234);

    // A panic while another thread holds the consent lock must not wait for it: the hook reads
    // the last known value (`ask` here), so the report is spooled for review, nothing uploaded.
    {
        let (release, holder) = common::hold_consent_lock(&consent);
        let _ = std::panic::catch_unwind(|| panic!("while the lock is held"));
        drop(release);
        holder.join().expect("the holding thread");
    }
    assert_eq!(spool.list().expect("list").len(), 2);
    assert!(spool.list().expect("list").iter().all(|e| !e.approved));

    // Off: the next panic writes nothing.
    consent.set(ConsentKind::Crash, Consent::Off, 2);
    let _ = std::panic::catch_unwind(|| panic!("again"));
    assert_eq!(spool.list().expect("list").len(), 2);
}
