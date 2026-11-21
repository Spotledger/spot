//! Pid-file edge cases exercised through the real supervisor path: an empty file is
//! stale, a genuinely dead sibling pid is cleaned, and a live pid is refused.

mod common;

use common::{TestEnv, must};
use spot_pg_supervisor::Supervisor;
use spot_pg_supervisor::error::SupervisorError;
use spot_pg_supervisor::events::{EventStream, SupervisorEvent};
use std::time::Duration;

/// How many short-lived children the dead-pid test may try before it gives up.
const PID_ATTEMPTS: u32 = 5;

/// An empty `postmaster.pid` is stale: cleaned, startup proceeds.
#[tokio::test]
async fn empty_pid_file_is_stale() {
    let env = TestEnv::new();
    let supervisor = Supervisor::new(env.layout(), env.settings(), env.config());
    must(std::fs::create_dir_all(&env.data_dir));
    must(std::fs::write(env.data_dir.join("postmaster.pid"), b""));
    let handle = must(supervisor.start().await);
    must(handle.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env);
}

/// Spawns a short-lived child, reaps it and returns its pid once nothing holds it any
/// more. `None` when the kernel already handed the pid to another process (reuse).
async fn dead_pid() -> Option<u32> {
    let mut dying = must(tokio::process::Command::new("true").spawn());
    let pid = dying.id()?;
    let _ = dying.wait().await;
    if pid_still_taken(pid) {
        return None;
    }
    Some(pid)
}

/// `true` while some process holds this pid. The supervisor probes liveness with
/// `kill(pid, 0)`, which finds exactly the processes that have a `/proc` entry, so a pid
/// without one is one the supervisor must treat as stale. Windows has no `/proc` and its
/// probe is deliberately conservative (every pid looks alive); report "taken" there so
/// the attempt loop ends with its own clear message instead of blaming the supervisor.
fn pid_still_taken(pid: u32) -> bool {
    if cfg!(windows) {
        return true;
    }
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Collects the events published so far exactly once. A lagged receiver is not the end
/// of the stream, so `Lagged` continues; only `Empty` or `Closed` stops the drain.
fn drain(events: &mut EventStream) -> Vec<SupervisorEvent> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut collected = Vec::new();
    loop {
        match events.try_recv() {
            Ok(event) => collected.push(event),
            Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => return collected,
        }
    }
}

/// A pid file naming a genuinely dead sibling process is cleaned. We spawn our own
/// short-lived child to obtain a truly dead pid (reaped, so no zombie remains). Other
/// tests in this binary start real processes in parallel, so a pid the kernel handed out
/// again is detected and another child is tried (up to [`PID_ATTEMPTS`]); a refusal of a
/// pid nothing holds is a supervisor bug and fails the test at once.
#[tokio::test]
async fn dead_sibling_pid_cleaned() {
    for attempt in 1..=PID_ATTEMPTS {
        let Some(pid) = dead_pid().await else {
            continue;
        };
        let env = TestEnv::new();
        let supervisor = Supervisor::new(env.layout(), env.settings(), env.config());
        must(std::fs::create_dir_all(&env.data_dir));
        must(std::fs::write(
            env.data_dir.join("postmaster.pid"),
            format!("{pid}\n/data\n1\n"),
        ));
        // Subscribe before start: the supervisor publishes the cleanup event before
        // start returns, so the single drain below cannot miss it and needs no waiting.
        let mut events = supervisor.events();
        let started = supervisor.start().await;
        let collected = drain(&mut events);
        let handle = match started {
            Ok(handle) => handle,
            // The kernel handed the pid to another process between our check and the
            // supervisor's probe: refusing is correct, so try another child.
            Err(SupervisorError::LivePostmaster { .. }) if pid_still_taken(pid) => continue,
            Err(error) => panic!(
                "attempt {attempt}/{PID_ATTEMPTS}: start failed for pid {pid}, which no \
                 process holds (so the file is stale): {error:?}"
            ),
        };
        assert!(
            !collected
                .iter()
                .any(|event| matches!(event, SupervisorEvent::LiveLockRefused { .. })),
            "a stale lock must not be reported as a live-lock refusal: {collected:?}"
        );
        assert!(
            collected.iter().any(|event| matches!(
                event,
                SupervisorEvent::StaleLockCleaned { pid: cleaned, .. } if *cleaned == pid
            )),
            "no StaleLockCleaned for pid {pid} in {collected:?}"
        );
        must(handle.stop(Duration::from_secs(3)).await);
        common::assert_no_orphans(&env);
        return;
    }
    panic!("could not obtain a dead, unreused pid in {PID_ATTEMPTS} attempts");
}

/// A pid file naming a live process (this test's own pid) is refused with a typed error
/// and a `LiveLockRefused` event, and the file is left exactly as it was.
#[tokio::test]
async fn live_pid_is_refused() {
    let env = TestEnv::new();
    let supervisor = Supervisor::new(env.layout(), env.settings(), env.config());
    let pid = std::process::id();
    must(std::fs::create_dir_all(&env.data_dir));
    let pid_path = env.data_dir.join("postmaster.pid");
    must(std::fs::write(&pid_path, format!("{pid}\n/data\n1\n")));
    let mut events = supervisor.events();
    let started = supervisor.start().await;
    let collected = drain(&mut events);
    assert!(
        matches!(
            started,
            Err(SupervisorError::LivePostmaster { pid: named, .. }) if named == pid
        ),
        "expected LivePostmaster for pid {pid}"
    );
    assert!(
        collected.iter().any(|event| matches!(
            event,
            SupervisorEvent::LiveLockRefused { pid: refused, .. } if *refused == pid
        )),
        "no LiveLockRefused for pid {pid} in {collected:?}"
    );
    let after = must(std::fs::read_to_string(&pid_path));
    let named_after = must(
        after
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<u32>(),
    );
    assert_eq!(
        named_after, pid,
        "the refusal must leave the pid file exactly as it was: {after:?}"
    );
    common::assert_no_orphans(&env);
}
