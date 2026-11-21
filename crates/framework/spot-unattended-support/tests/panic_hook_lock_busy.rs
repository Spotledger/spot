//! The panic hook honours the consent switch while the consent lock is busy: a lock
//! that cannot be taken must not read as the first-run default `ask` and spool a report the
//! user turned off. This file has one test, and its own binary, because the hook is
//! process-wide.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use spot_unattended_support::consent::{Consent, ConsentKind, ConsentStore};
use spot_unattended_support::crash::{CrashCollector, install_panic_hook};
use spot_unattended_support::spool::Spool;

#[test]
fn a_busy_consent_lock_keeps_the_switch_the_user_set() {
    let dir = common::temp_dir("hook-busy");
    let spool = Arc::new(Spool::new(&dir));
    let consent = Arc::new(ConsentStore::first_run());
    consent.set(ConsentKind::Crash, Consent::Off, 1);
    let collector = Arc::new(CrashCollector::new(common::context(), common::scrubber()));
    install_panic_hook(collector, consent.clone(), spool.clone(), || 1_234);

    // Off, with the lock busy: nothing is spooled.
    let (release, holder) = common::hold_consent_lock(&consent);
    let raised = std::panic::catch_unwind(|| panic!("off, while the lock is held"));
    assert!(raised.is_err(), "the panic did not reach the test");
    assert!(
        spool.list().expect("list").is_empty(),
        "a report was spooled although the user turned crash reporting off"
    );
    drop(release);
    holder.join().expect("the holding thread");

    // On, with the lock busy: the report is spooled, unapproved (consent `on` needs no
    // per-report approval).
    consent.set(ConsentKind::Crash, Consent::On, 2);
    let (release, holder) = common::hold_consent_lock(&consent);
    let raised = std::panic::catch_unwind(|| panic!("on, while the lock is held"));
    assert!(raised.is_err(), "the panic did not reach the test");
    let listed = spool.list().expect("list");
    assert_eq!(
        listed.len(),
        1,
        "no report was spooled although consent is on"
    );
    assert!(!listed[0].approved, "consent `on` needs no approval");
    drop(release);
    holder.join().expect("the holding thread");
}
