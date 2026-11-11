//! Crash reporting: consent, redaction before write, caps, the spool and the upload rules.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::sync::Mutex;

use spot_unattended_support::consent::{Consent, ConsentKind, ConsentState, ConsentStore};
use spot_unattended_support::crash::{
    CrashCollector, MAX_BREADCRUMBS, MAX_REPORT_BYTES, parse_frames, upload_spooled,
};
use spot_unattended_support::spool::Spool;
use spot_unattended_support::telemetry::Uploader;

const CANARIES: [&str; 4] = [
    "/home/alice",
    "bob@example.com",
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
    "alice",
];

struct Recording(Mutex<Vec<(String, Vec<u8>)>>);

impl Uploader for Recording {
    fn upload(&self, kind: &str, body: &[u8]) -> Result<(), String> {
        self.0
            .lock()
            .expect("lock")
            .push((kind.to_owned(), body.to_vec()));
        Ok(())
    }
}

fn on() -> ConsentState {
    let mut state = ConsentState::first_run();
    state.set(ConsentKind::Crash, Consent::On, 1);
    state
}

fn collector() -> CrashCollector {
    CrashCollector::new(common::context(), common::scrubber())
}

fn frames() -> Vec<String> {
    vec![
        "app::run at /home/alice/spot/src/main.rs:10".to_owned(),
        "core::panicking::panic".to_owned(),
    ]
}

#[test]
fn nothing_is_enabled_by_default() {
    let state = ConsentState::first_run();
    assert_eq!(state.get(ConsentKind::Crash), Consent::Ask);
    assert_eq!(state.get(ConsentKind::Telemetry), Consent::Ask);
    assert!(!state.may_upload_crash());
    assert!(!state.may_send_telemetry());
    assert!(state.must_ask_per_report());
    let missing = ConsentState::from_settings(None, None).expect("defaults");
    assert_eq!(missing, state);
}

#[test]
fn the_consent_state_machine_records_changes_and_refuses_bad_values() {
    let mut state = ConsentState::first_run();
    assert!(state.set(ConsentKind::Crash, Consent::Ask, 5).is_none());
    let change = state
        .set(ConsentKind::Crash, Consent::On, 6)
        .expect("change");
    assert_eq!(
        (change.from, change.to, change.at),
        (Consent::Ask, Consent::On, 6)
    );
    // answer() only resolves an ask.
    assert!(state.answer(ConsentKind::Crash, false, 7).is_none());
    assert_eq!(
        state
            .answer(ConsentKind::Telemetry, true, 8)
            .expect("answered")
            .to,
        Consent::On
    );
    state.set(ConsentKind::Crash, Consent::Off, 9);
    assert!(!state.may_spool_crash());
    assert_eq!(state.history().len(), 3);
    assert_eq!(Consent::parse(" ON ").expect("on"), Consent::On);
    assert_eq!(
        Consent::parse("yes").expect_err("refused").code(),
        "unattended_support.consent_invalid"
    );
    assert!(ConsentState::from_settings(Some("maybe"), None).is_err());
    let loaded = ConsentState::from_settings(Some("on"), Some("off")).expect("settings");
    assert!(loaded.may_upload_crash() && !loaded.may_send_telemetry());
}

#[test]
fn the_store_keeps_a_lock_free_copy_of_the_crash_switch() {
    // Initialised from the stored value at start-up: the copy is right before the first panic,
    // and a lock held elsewhere cannot move it.
    let loaded = ConsentState::from_settings(Some("off"), Some("ask")).expect("settings");
    let store = Arc::new(ConsentStore::new(loaded));
    assert!(!store.may_spool_crash_now());
    let (release, holder) = common::hold_consent_lock(&store);
    assert!(
        !store.may_spool_crash_now(),
        "a busy lock must not read as the first-run default"
    );
    assert_eq!(store.last_known().get(ConsentKind::Crash), Consent::Off);
    assert_eq!(store.last_known().get(ConsentKind::Telemetry), Consent::Ask);
    drop(release);
    holder.join().expect("the holding thread");

    // A change through the store updates the copy at once, and so does an answer.
    assert!(store.set(ConsentKind::Crash, Consent::On, 5).is_some());
    assert!(store.may_spool_crash_now());
    assert!(store.answer(ConsentKind::Telemetry, false, 6).is_some());
    assert!(store.may_spool_crash_now());
    assert_eq!(store.last_known().get(ConsentKind::Crash), Consent::On);

    // The state behind the lock still holds both switches and the history.
    let state = store.state();
    assert_eq!(state.get(ConsentKind::Crash), Consent::On);
    assert_eq!(state.get(ConsentKind::Telemetry), Consent::Off);
    assert_eq!(state.history().len(), 2);

    store.set(ConsentKind::Crash, Consent::Off, 7);
    assert!(!store.may_spool_crash_now());
}

#[test]
fn nothing_is_written_when_consent_is_off() {
    let dir = common::temp_dir("off");
    let spool = Spool::new(&dir);
    let mut state = ConsentState::first_run();
    state.set(ConsentKind::Crash, Consent::Off, 1);
    let id = collector()
        .record(&state, &spool, "src/a.rs:1", &frames(), 10)
        .expect("ok");
    assert!(id.is_none());
    assert!(spool.list().expect("list").is_empty());
}

#[test]
fn canaries_never_reach_the_spooled_report() {
    let dir = common::temp_dir("canary");
    let spool = Spool::new(&dir);
    let c = collector();
    c.breadcrumb("user bob@example.com logged in from /home/alice/work");
    c.breadcrumb(&format!("token {}", CANARIES[2]));
    c.breadcrumb("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789");
    let mut frames = frames();
    frames.push(format!("worker alice {}", CANARIES[1]));
    let id = c
        .record(&on(), &spool, "/home/alice/spot/src/lib.rs:42", &frames, 99)
        .expect("recorded")
        .expect("spooled");
    let text = spool.view(&id).expect("view");
    for canary in CANARIES {
        assert!(
            !text.contains(canary),
            "{canary} leaked into the spooled report: {text}"
        );
    }
    assert!(!text.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
    // The report is still useful: the shape of the location survives.
    assert!(text.contains("src/lib.rs:42"));
    assert!(text.contains("1.4.2") && text.contains("abc123") && text.contains("digest-1"));
    let report = spool.read(&id).expect("read");
    assert_eq!(report.occurred_at, 99);
    assert!(
        report.panic_location.starts_with('~'),
        "{}",
        report.panic_location
    );
}

#[test]
fn generic_home_paths_are_scrubbed_even_for_unknown_users() {
    let scrubber = spot_unattended_support::scrub::Scrubber::new(&[], &[]).expect("scrubber");
    assert_eq!(
        scrubber.scrub("/home/zoe/app/x.rs"),
        "/home/<user>/app/x.rs"
    );
    assert_eq!(scrubber.scrub("/Users/zoe/app"), "/Users/<user>/app");
    assert_eq!(
        scrubber.scrub("C:\\Users\\zoe\\app"),
        "C:\\Users\\<user>\\app"
    );
    assert_eq!(scrubber.scrub("/home/<user>/x"), "/home/<user>/x");
}

#[test]
fn reports_are_capped() {
    let c = collector();
    for i in 0..(MAX_BREADCRUMBS * 3) {
        c.breadcrumb(&format!("line {i} {}", "x".repeat(2000)));
    }
    let many: Vec<String> = (0..500)
        .map(|i| format!("frame {i} {}", "y".repeat(900)))
        .collect();
    let report = c.build("src/a.rs:1", &many, 1);
    assert!(report.breadcrumbs.len() <= MAX_BREADCRUMBS);
    assert!(report.backtrace.len() <= 64);
    assert!(report.to_bytes().expect("fits").len() <= MAX_REPORT_BYTES);
    assert!(report.breadcrumbs.iter().all(|b| b.len() <= 520));
}

#[test]
fn the_user_can_view_approve_and_delete_the_spool() {
    let dir = common::temp_dir("spool");
    let spool = Spool::new(&dir);
    assert!(spool.list().expect("missing folder is empty").is_empty());
    let c = collector();
    let a = c
        .record(&on(), &spool, "src/a.rs:1", &frames(), 1)
        .expect("ok")
        .expect("id");
    let b = c
        .record(&on(), &spool, "src/b.rs:2", &frames(), 2)
        .expect("ok")
        .expect("id");
    assert_ne!(a, b);
    let listed = spool.list().expect("list");
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|e| !e.approved && e.bytes > 0));
    spool.approve(&a).expect("approve");
    assert!(
        spool
            .list()
            .expect("list")
            .iter()
            .any(|e| e.id == a && e.approved)
    );
    spool.delete(&a).expect("delete");
    spool.delete(&a).expect("deleting twice is fine");
    assert_eq!(spool.list().expect("list").len(), 1);
    assert_eq!(spool.clear().expect("clear"), 1);
    assert!(spool.list().expect("list").is_empty());
    // An id that is not a spool id never touches the file system.
    assert!(spool.view("../../etc/passwd").is_err());
    assert!(spool.delete("not-an-id").is_err());
    assert!(spool.approve("0123456789abcdef").is_err());
}

#[test]
fn a_crash_loop_cannot_fill_the_spool() {
    let dir = common::temp_dir("bounded");
    let spool = Spool::with_limits(&dir, 3, 1024 * 1024);
    let c = collector();
    let mut last = String::new();
    for i in 0..10 {
        last = c
            .record(&on(), &spool, &format!("src/a.rs:{i}"), &frames(), i)
            .expect("ok")
            .expect("id");
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let listed = spool.list().expect("list");
    assert_eq!(listed.len(), 3, "the count bound holds");
    assert!(
        listed.iter().any(|e| e.id == last),
        "the newest report is kept"
    );
    // The byte bound too: a bound below one report still keeps the report just written.
    let dir = common::temp_dir("bytes");
    let spool = Spool::with_limits(&dir, 50, 10);
    for i in 0..3 {
        c.record(&on(), &spool, &format!("src/b.rs:{i}"), &frames(), i)
            .expect("ok");
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    assert_eq!(spool.list().expect("list").len(), 1);
}

#[cfg(unix)]
#[test]
fn spool_files_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let dir = common::temp_dir("perm").join("spool");
    let spool = Spool::new(&dir);
    let id = collector()
        .record(&on(), &spool, "src/a.rs:1", &frames(), 1)
        .expect("ok")
        .expect("id");
    spool.approve(&id).expect("approve");
    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(format!("{id}.json"))), 0o600);
    assert_eq!(mode(&dir.join(format!("{id}.ok"))), 0o600);
}

#[test]
fn upload_follows_the_consent_matrix() {
    let dir = common::temp_dir("upload");
    let spool = Spool::new(&dir);
    let c = collector();
    let mut ask = ConsentState::first_run();
    let first = c
        .record(&ask, &spool, "src/a.rs:1", &frames(), 1)
        .expect("ok")
        .expect("spooled under ask");
    c.record(&ask, &spool, "src/b.rs:2", &frames(), 2)
        .expect("ok");
    let up = Recording(Mutex::new(Vec::new()));
    // Ask: nothing leaves until the user approves a report, and then only that one.
    assert_eq!(upload_spooled(&ask, &spool, &up).expect("ok"), 0);
    spool.approve(&first).expect("approve");
    assert_eq!(upload_spooled(&ask, &spool, &up).expect("ok"), 1);
    let sent = up.0.lock().expect("lock").clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "crash");
    assert_eq!(
        spool.list().expect("list").len(),
        1,
        "an uploaded report leaves the spool"
    );
    // Off: nothing, even for what is spooled.
    ask.set(ConsentKind::Crash, Consent::Off, 3);
    assert_eq!(upload_spooled(&ask, &spool, &up).expect("ok"), 0);
    // On: everything that is left.
    ask.set(ConsentKind::Crash, Consent::On, 4);
    assert_eq!(upload_spooled(&ask, &spool, &up).expect("ok"), 1);
    assert!(spool.list().expect("list").is_empty());
}

#[test]
fn a_refused_upload_keeps_the_report() {
    struct Refuse;
    impl Uploader for Refuse {
        fn upload(&self, _: &str, _: &[u8]) -> Result<(), String> {
            Err("token=sekrit-value blocked by policy".to_owned())
        }
    }
    let dir = common::temp_dir("refuse");
    let spool = Spool::new(&dir);
    collector()
        .record(&on(), &spool, "src/a.rs:1", &frames(), 1)
        .expect("ok");
    let err = upload_spooled(&on(), &spool, &Refuse).expect_err("refused");
    assert_eq!(err.code(), "unattended_support.upload_failed");
    assert!(
        !err.to_string().contains("sekrit-value"),
        "uploader text must not be echoed"
    );
    assert_eq!(spool.list().expect("list").len(), 1);
}

#[test]
fn backtrace_text_is_split_into_frames() {
    let trace = "   0: app::run\n             at ./src/main.rs:10:5\n   1: std::rt::lang_start\n";
    assert_eq!(
        parse_frames(trace),
        ["app::run at ./src/main.rs:10:5", "std::rt::lang_start"]
    );
}
