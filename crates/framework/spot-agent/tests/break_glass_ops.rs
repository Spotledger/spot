//! WP-ADM-11.T3, agent side: `breakglass.open` and `breakglass.close`. A fake account provider
//! and a manual clock stand in for the host: the grant expires on the agent without any control
//! plane, revocation closes, a closed session cannot be reopened (also after a restart), the
//! recording chain verifies and a changed byte is detected, a typed password is masked, and every
//! refusal is audited.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_agent::handlers::break_glass_ops::{
    BgError, BreakGlassAgent, CLOSE, MAX_SECONDS, NAMESPACE, OPEN, RefusingAccounts,
    SessionAccounts,
};
use spot_break_glass::{FileRecorder, FrameKind, replay, verify_recording};
use spot_core::ManualClock;
use time::OffsetDateTime;

#[derive(Default)]
struct Accounts {
    created: Mutex<Vec<(String, Vec<String>)>>,
    removed: Mutex<Vec<String>>,
    fail_remove: Mutex<bool>,
}

impl SessionAccounts for Accounts {
    fn create(&self, session: &str, scope: &[String], _valid_secs: i64) -> Result<String, BgError> {
        self.created
            .lock()
            .unwrap()
            .push((session.to_owned(), scope.to_vec()));
        Ok(format!("bg-{session}"))
    }
    fn remove(&self, session: &str, _account: &str) -> Result<(), BgError> {
        if *self.fail_remove.lock().unwrap() {
            return Err(BgError::Account);
        }
        self.removed.lock().unwrap().push(session.to_owned());
        Ok(())
    }
}

struct Rig {
    dir: PathBuf,
    clock: Arc<ManualClock>,
    accounts: Arc<Accounts>,
    agent: BreakGlassAgent,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(tag: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!(
        "spot_bg_agent_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ));
    let accounts = Arc::new(Accounts::default());
    let agent = BreakGlassAgent::new(Arc::clone(&clock) as _, Arc::clone(&accounts) as _, &dir);
    Rig {
        dir,
        clock,
        accounts,
        agent,
    }
}

fn scope() -> Vec<String> {
    vec!["invoices".to_owned()]
}

fn audit_lines(rig: &Rig) -> Vec<String> {
    std::fs::read_to_string(rig.dir.join("audit_log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_namespace_and_ids_are_the_ones_the_catalogue_names() {
    assert_eq!(NAMESPACE, "breakglass");
    assert!(OPEN.starts_with(NAMESPACE) && CLOSE.starts_with(NAMESPACE));
}

#[test]
fn the_grant_expires_on_the_agent_without_the_control_plane() {
    let r = rig("expiry");
    let account = r.agent.open("s1", "acme", &scope(), 600).unwrap();
    assert_eq!(account, "bg-s1");
    assert_eq!(r.accounts.created.lock().unwrap()[0].1, scope());
    r.clock.advance(Duration::from_secs(599));
    assert!(r.agent.tick().is_empty() && r.agent.is_open("s1"));
    r.clock.advance(Duration::from_secs(1));
    assert_eq!(r.agent.tick(), ["s1"]);
    assert!(!r.agent.is_open("s1"));
    assert_eq!(r.accounts.removed.lock().unwrap().as_slice(), ["s1"]);
    assert!(audit_lines(&r).iter().any(|l| l.contains(" expired s1 ")));
}

#[test]
fn a_closed_or_expired_session_cannot_be_reopened_even_after_a_restart() {
    let r = rig("reopen");
    r.agent.open("s1", "acme", &scope(), 600).unwrap();
    r.agent.close("s1", "closed").unwrap();
    assert_eq!(
        r.agent.open("s1", "acme", &scope(), 600).unwrap_err(),
        BgError::Reused
    );
    let restarted = BreakGlassAgent::new(
        Arc::clone(&r.clock) as _,
        Arc::clone(&r.accounts) as _,
        &r.dir,
    );
    assert_eq!(
        restarted.open("s1", "acme", &scope(), 600).unwrap_err(),
        BgError::Reused
    );
    assert!(
        audit_lines(&r)
            .iter()
            .any(|l| l.contains(" open_refused s1 "))
    );
}

#[test]
fn a_revocation_closes_the_session_and_removes_the_account_first() {
    let r = rig("revoke");
    r.agent.open("s1", "acme", &scope(), 600).unwrap();
    *r.accounts.fail_remove.lock().unwrap() = true;
    assert_eq!(r.agent.close("s1", "closed").unwrap_err(), BgError::Account);
    assert!(
        r.agent.is_open("s1"),
        "still tracked, so the next tick retries"
    );
    *r.accounts.fail_remove.lock().unwrap() = false;
    assert_eq!(r.agent.tick(), ["s1"]);
    assert_eq!(r.agent.close("s1", "closed").unwrap_err(), BgError::NotOpen);
}

#[test]
fn the_recording_chain_verifies_a_typed_password_is_masked_and_a_change_is_detected() {
    let r = rig("recording");
    r.agent.open("s1", "acme", &scope(), 600).unwrap();
    r.agent
        .record("s1", FrameKind::Input, b"ls\n", true)
        .unwrap();
    r.agent
        .record("s1", FrameKind::Output, b"invoices\n", true)
        .unwrap();
    r.agent
        .record("s1", FrameKind::Input, b"hunter2\n", false)
        .unwrap();
    let digest = r.agent.close("s1", "closed").unwrap();
    let dir = r.dir.join("recordings");
    let recording = FileRecorder::recording_path(&dir, "s1");
    let manifest = FileRecorder::manifest_path(&dir, "s1");
    let checked = verify_recording(&recording, &manifest).unwrap();
    assert_eq!(spot_break_glass::hex(&checked), digest);
    assert_eq!(
        std::fs::read_to_string(r.dir.join("s1.digest")).unwrap(),
        digest
    );
    let bytes = std::fs::read(&recording).unwrap();
    assert!(
        !bytes.windows(7).any(|w| w == b"hunter2"),
        "the password never reached the file"
    );
    let mut tampered = bytes;
    let at = tampered.len() - 5;
    tampered[at] ^= 0xff;
    std::fs::write(&recording, tampered).unwrap();
    assert!(!replay(&recording).verified());
    assert!(verify_recording(&recording, &manifest).is_err());
}

#[test]
fn nothing_is_recorded_for_a_closed_session_and_bad_arguments_are_refused() {
    let r = rig("args");
    assert_eq!(
        r.agent
            .record("nope", FrameKind::Input, b"x", true)
            .unwrap_err(),
        BgError::NotOpen
    );
    assert_eq!(
        r.agent.open("../x", "acme", &scope(), 60).unwrap_err(),
        BgError::Argument
    );
    assert_eq!(
        r.agent.open("s2", "acme", &[], 60).unwrap_err(),
        BgError::Argument
    );
    assert_eq!(
        r.agent.open("s2", "acme", &scope(), 0).unwrap_err(),
        BgError::Duration
    );
    assert_eq!(
        r.agent
            .open("s2", "acme", &scope(), MAX_SECONDS + 1)
            .unwrap_err(),
        BgError::Duration
    );
    assert!(
        r.accounts.created.lock().unwrap().is_empty(),
        "nothing created on a refusal"
    );
    assert_eq!(
        audit_lines(&r)
            .iter()
            .filter(|l| l.contains(" open_refused "))
            .count(),
        4
    );
}

#[test]
fn without_an_account_provider_every_open_is_refused() {
    let dir = std::env::temp_dir().join(format!("spot_bg_agent_none_{}", std::process::id()));
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ));
    let agent = BreakGlassAgent::new(clock as _, Arc::new(RefusingAccounts), &dir);
    assert_eq!(
        agent.open("s1", "acme", &scope(), 60).unwrap_err(),
        BgError::Account
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_audit_log_is_hash_chained_and_holds_no_terminal_bytes() {
    let r = rig("audit");
    r.agent.open("s1", "acme", &scope(), 600).unwrap();
    r.agent
        .record("s1", FrameKind::Input, b"secret-command\n", true)
        .unwrap();
    r.agent.close("s1", "closed").unwrap();
    let lines = audit_lines(&r);
    assert!(lines.len() >= 2);
    assert!(lines.iter().all(|l| !l.contains("secret-command")));
    // Each line names the hash of everything before it: replaying the chain reaches the head.
    let mut prev = [0_u8; 32];
    for (n, line) in lines.iter().enumerate() {
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields[0], (n + 1).to_string());
        assert_eq!(fields[1], spot_break_glass::hex(&prev));
        use sha2::Digest as _;
        prev = sha2::Sha256::digest([prev.as_slice(), line.as_bytes()].concat()).into();
    }
}
