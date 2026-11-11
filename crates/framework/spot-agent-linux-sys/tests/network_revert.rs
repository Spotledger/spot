//! The persisted network revert guard: the ticket and its backup survive an agent restart,
//! a restart inside the window re-arms, one after the deadline reverts, a confirm clears,
//! and damaged state is reverted from the backup rather than ignored. Time is a manual
//! clock and the machine is a recording runner and an in-memory filesystem.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use spot_agent_linux_sys::error::{
    NETWORK_REVERT_BUSY, NETWORK_REVERT_CORRUPT, NETWORK_REVERT_EXPIRED, NETWORK_REVERT_REQUIRED,
    NETWORK_REVERT_SCHEDULE, SysError, SysResult,
};
use spot_agent_linux_sys::revert::{BACKUP_FILE, SEQUENCE_FILE, TICKET_FILE};
use spot_agent_linux_sys::testing::{FakeFs, FakeRunner, FakeSleeper, ok_output};
use spot_agent_linux_sys::{
    Addressing, FileSystem, NetworkChange, NetworkModule, Recovery, RevertScheduler, Sleeper,
    SystemdRunScheduler,
};
use spot_core::ManualClock;
use time::OffsetDateTime;

const STATE: &str = "/state";
const WINDOW: u64 = 30;

fn ticket_path() -> String {
    format!("{STATE}/{TICKET_FILE}")
}

fn backup_path() -> String {
    format!("{STATE}/{BACKUP_FILE}")
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ))
}

const NM_MODIFY_STATIC: &str = "nmcli connection modify eth0 ipv4.method manual ipv4.addresses 10.0.0.5/24 ipv4.gateway 10.0.0.1 ipv4.dns 10.0.0.1";
const NM_MODIFY_DHCP: &str =
    "nmcli connection modify eth0 ipv4.method auto ipv4.addresses  ipv4.gateway  ipv4.dns ";

/// NetworkManager, eth0 on DHCP; setting static and reverting to DHCP both succeed.
fn nm_runner() -> FakeRunner {
    FakeRunner::new()
        .with_response(
            "ip -j address show",
            ok_output(r#"[{"ifname":"eth0","operstate":"UP","addr_info":[]}]"#),
        )
        .with_response("systemctl is-active NetworkManager", ok_output("active\n"))
        .with_response(NM_MODIFY_STATIC, ok_output(""))
        .with_response(NM_MODIFY_DHCP, ok_output(""))
        .with_response("nmcli connection up eth0", ok_output(""))
}

fn static_change() -> NetworkChange {
    NetworkChange::new(
        "eth0",
        Addressing::Static {
            address: "10.0.0.5".to_owned(),
            prefix: 24,
            gateway: Some("10.0.0.1".to_owned()),
            dns: vec!["10.0.0.1".to_owned()],
        },
    )
    .unwrap()
}

fn reverts(runner: &FakeRunner) -> usize {
    runner
        .call_keys()
        .iter()
        .filter(|key| key.as_str() == NM_MODIFY_DHCP)
        .count()
}

/// A scheduler that records what it was asked and can be told to fail or "crash".
#[derive(Default)]
struct FakeScheduler {
    events: Mutex<Vec<String>>,
    fail: Mutex<bool>,
    crash: Mutex<bool>,
}

impl FakeScheduler {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl RevertScheduler for FakeScheduler {
    fn schedule(&self, id: u64, delay_secs: u64) -> SysResult<()> {
        assert!(!*self.crash.lock().unwrap(), "the agent crashed");
        if *self.fail.lock().unwrap() {
            return Err(SysError::new(NETWORK_REVERT_SCHEDULE, "no systemd-run"));
        }
        self.events
            .lock()
            .unwrap()
            .push(format!("schedule {id} {delay_secs}"));
        Ok(())
    }

    fn cancel(&self, id: u64) -> SysResult<()> {
        self.events.lock().unwrap().push(format!("cancel {id}"));
        Ok(())
    }
}

fn module<'a>(
    runner: &'a FakeRunner,
    fs: &'a dyn FileSystem,
    clock: &Arc<ManualClock>,
    scheduler: &'a FakeScheduler,
) -> NetworkModule<'a> {
    NetworkModule::new(runner, fs, clock.clone())
        .with_state_dir(STATE)
        .with_scheduler(scheduler)
}

#[test]
fn apply_persists_the_ticket_and_backup_and_schedules_the_independent_timer() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let applied = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap();
    assert!(fs.has_file(&ticket_path()));
    assert!(fs.has_file(&backup_path()));
    let ticket = fs.file(&ticket_path()).unwrap();
    assert!(ticket.contains(&format!("\"deadline_unix\":{}", 1_800_000_000 + WINDOW)));
    assert_eq!(
        scheduler.events(),
        [format!("schedule {} {WINDOW}", applied.ticket.id)]
    );
}

#[test]
fn the_state_is_saved_before_the_change_is_made() {
    // The scheduler runs after the save and before the change: when it "crashes" the
    // ticket and backup are already on disk and nothing was configured.
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    *scheduler.crash.lock().unwrap() = true;
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        module(&runner, &fs, &clock, &scheduler).apply(&static_change(), WINDOW)
    }));
    assert!(crashed.is_err());
    assert!(fs.has_file(&ticket_path()) && fs.has_file(&backup_path()));
    assert!(
        !runner
            .call_keys()
            .iter()
            .any(|key| key.starts_with("nmcli connection modify")),
        "nothing was configured before the crash"
    );

    // After the restart, inside the window: re-armed. Past it: reverted (a harmless no-op
    // restore of what was never changed).
    let scheduler = FakeScheduler::default();
    clock.advance(Duration::from_secs(5));
    let recovery = module(&runner, &fs, &clock, &scheduler).recover().unwrap();
    assert!(matches!(
        recovery,
        Recovery::Rearmed {
            remaining_secs: 25,
            ..
        }
    ));
    clock.advance(Duration::from_secs(26));
    let recovery = module(&runner, &fs, &clock, &scheduler).recover().unwrap();
    assert!(matches!(recovery, Recovery::Reverted(_)));
    assert_eq!(reverts(&runner), 1);
    assert!(!fs.has_file(&ticket_path()) && !fs.has_file(&backup_path()));
}

#[test]
fn a_restart_inside_the_window_rearms_the_timer_and_the_change_can_still_be_confirmed() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let id = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap()
        .ticket
        .id;

    // The agent restarts 10 seconds in.
    clock.advance(Duration::from_secs(10));
    let after = FakeScheduler::default();
    let restarted = module(&runner, &fs, &clock, &after);
    match restarted.recover().unwrap() {
        Recovery::Rearmed {
            ticket,
            remaining_secs,
        } => {
            assert_eq!(ticket.id, id);
            assert_eq!(remaining_secs, 20);
        }
        other => panic!("expected a re-arm, got {other:?}"),
    }
    assert_eq!(
        after.events(),
        [format!("cancel {id}"), format!("schedule {id} 20")]
    );
    assert_eq!(reverts(&runner), 0, "nothing reverted inside the window");
    assert!(restarted.sweep().unwrap().is_none());

    // A fresh module (the handler builds one per operation) can still confirm.
    let confirmer = module(&runner, &fs, &clock, &after);
    confirmer.confirm(id).unwrap();
    assert!(!fs.has_file(&ticket_path()) && !fs.has_file(&backup_path()));
    assert_eq!(after.events().last().unwrap(), &format!("cancel {id}"));
    assert_eq!(restarted.recover().unwrap(), Recovery::Nothing);
    clock.advance(Duration::from_secs(60));
    assert!(
        restarted.sweep().unwrap().is_none(),
        "a confirmed change never reverts"
    );
}

#[test]
fn a_restart_after_the_deadline_reverts_at_once_and_only_once() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let id = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap()
        .ticket
        .id;
    clock.advance(Duration::from_secs(WINDOW + 1));

    let after = FakeScheduler::default();
    let restarted = module(&runner, &fs, &clock, &after);
    let Recovery::Reverted(outcome) = restarted.recover().unwrap() else {
        panic!("expected a revert");
    };
    assert!(outcome.changed);
    assert_eq!(reverts(&runner), 1);
    assert!(!fs.has_file(&ticket_path()) && !fs.has_file(&backup_path()));

    // Idempotent: everything that can run again is a no-op.
    assert_eq!(restarted.recover().unwrap(), Recovery::Nothing);
    assert!(restarted.sweep().unwrap().is_none());
    assert!(restarted.revert_ticket(id).unwrap().is_none());
    assert_eq!(reverts(&runner), 1, "the revert ran exactly once");
}

#[test]
fn a_confirm_after_the_deadline_is_refused_and_the_sweep_reverts() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let m = module(&runner, &fs, &clock, &scheduler);
    let id = m.apply(&static_change(), WINDOW).unwrap().ticket.id;
    clock.advance(Duration::from_secs(WINDOW));
    assert_eq!(m.confirm(id).unwrap_err().code, NETWORK_REVERT_EXPIRED);
    assert!(m.sweep().unwrap().is_some());
}

#[test]
fn the_independent_timer_reverts_its_own_ticket_even_inside_the_window_and_only_that_one() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let m = module(&runner, &fs, &clock, &scheduler);
    let id = m.apply(&static_change(), WINDOW).unwrap().ticket.id;
    // A timer left over from an earlier ticket must not touch this change.
    assert!(m.revert_ticket(id + 1000).unwrap().is_none());
    assert_eq!(reverts(&runner), 0);
    // The timer for this ticket reverts it, with the agent not involved at all.
    assert!(m.revert_ticket(id).unwrap().is_some());
    assert_eq!(reverts(&runner), 1);
    assert!(m.revert_ticket(id).unwrap().is_none());
}

#[test]
fn a_corrupt_ticket_is_reverted_from_the_backup_and_reported() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap();
    fs.set_file(&ticket_path(), "{ this is not a ticket");

    let Recovery::RevertedFromBackup(outcome) =
        module(&runner, &fs, &clock, &scheduler).recover().unwrap()
    else {
        panic!("a corrupt ticket must be reverted from the backup");
    };
    assert!(outcome.detail.contains("corrupt"), "{}", outcome.detail);
    assert_eq!(reverts(&runner), 1);
    assert!(!fs.has_file(&ticket_path()) && !fs.has_file(&backup_path()));
}

#[test]
fn a_corrupt_ticket_blocks_confirmation_and_is_reverted_by_the_sweep() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let m = module(&runner, &fs, &clock, &scheduler);
    let id = m.apply(&static_change(), WINDOW).unwrap().ticket.id;
    fs.set_file(&ticket_path(), "");
    assert!(m.confirm(id).is_err(), "a damaged ticket cannot confirm");
    assert!(m.sweep().unwrap().is_some());
    assert_eq!(reverts(&runner), 1);
}

#[test]
fn an_unreadable_ticket_and_backup_is_reported_not_ignored() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let m = module(&runner, &fs, &clock, &scheduler);
    m.apply(&static_change(), WINDOW).unwrap();
    fs.set_file(&ticket_path(), "garbage");
    fs.set_file(&backup_path(), "garbage");
    assert_eq!(m.recover().unwrap_err().code, NETWORK_REVERT_CORRUPT);
    assert_eq!(m.sweep().unwrap_err().code, NETWORK_REVERT_CORRUPT);
    // And a new change is refused rather than stacked on an unknown state.
    assert_eq!(
        m.apply(&static_change(), WINDOW).unwrap_err().code,
        NETWORK_REVERT_CORRUPT
    );
}

#[test]
fn a_backup_with_no_ticket_is_a_change_that_never_started() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    fs.set_file(&backup_path(), "{}");
    assert_eq!(
        module(&runner, &fs, &clock, &scheduler).recover().unwrap(),
        Recovery::Nothing
    );
    assert!(!fs.has_file(&backup_path()));
    assert_eq!(reverts(&runner), 0);
}

/// A filesystem that refuses to write one path, to cut `apply` short between the backup and
/// the ticket.
struct FailOn {
    inner: FakeFs,
    path: String,
}

impl FileSystem for FailOn {
    fn read_to_string(&self, path: &Path) -> SysResult<String> {
        self.inner.read_to_string(path)
    }
    fn exists(&self, path: &Path) -> SysResult<bool> {
        self.inner.exists(path)
    }
    fn is_dir(&self, path: &Path) -> SysResult<bool> {
        self.inner.is_dir(path)
    }
    fn list_dir(&self, path: &Path) -> SysResult<Vec<String>> {
        self.inner.list_dir(path)
    }
    fn write_atomic(&self, path: &Path, contents: &str) -> SysResult<()> {
        if path == Path::new(&self.path) {
            return Err(SysError::io("disk full"));
        }
        self.inner.write_atomic(path, contents)
    }
    fn remove_file(&self, path: &Path) -> SysResult<()> {
        self.inner.remove_file(path)
    }
    fn create_dir_all(&self, path: &Path) -> SysResult<()> {
        self.inner.create_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> SysResult<()> {
        self.inner.rename(from, to)
    }
    fn metadata_len(&self, path: &Path) -> SysResult<u64> {
        self.inner.metadata_len(path)
    }
    fn read_link(&self, path: &Path) -> SysResult<std::path::PathBuf> {
        self.inner.read_link(path)
    }
    fn try_lock_exclusive(&self, path: &Path) -> SysResult<Option<spot_agent_linux_sys::FileLock>> {
        self.inner.try_lock_exclusive(path)
    }
}

#[test]
fn a_failure_between_the_backup_and_the_ticket_changes_nothing() {
    let (runner, clock, scheduler) = (nm_runner(), clock(), FakeScheduler::default());
    let fs = FailOn {
        inner: FakeFs::new(),
        path: ticket_path(),
    };
    let error = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap_err();
    assert_eq!(error.code, "agent_linux_sys.io");
    assert!(
        !runner
            .call_keys()
            .iter()
            .any(|key| key.starts_with("nmcli connection modify")),
        "the change was not applied without a saved ticket"
    );
    assert!(scheduler.events().is_empty());
    // The orphan backup is discarded at the next start.
    assert!(fs.inner.has_file(&backup_path()));
    assert_eq!(
        module(&runner, &fs, &clock, &scheduler).recover().unwrap(),
        Recovery::Nothing
    );
    assert!(!fs.inner.has_file(&backup_path()));
}

#[test]
fn if_the_timer_cannot_be_scheduled_the_change_is_not_applied() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    *scheduler.fail.lock().unwrap() = true;
    let error = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap_err();
    assert_eq!(error.code, NETWORK_REVERT_SCHEDULE);
    assert!(
        !runner
            .call_keys()
            .iter()
            .any(|key| key.starts_with("nmcli connection modify"))
    );
    assert!(!fs.has_file(&ticket_path()) && !fs.has_file(&backup_path()));
}

#[test]
fn a_change_that_fails_part_way_is_restored_and_leaves_no_ticket() {
    let runner = nm_runner();
    runner.set_failure(
        "nmcli connection up eth0",
        1,
        "connection activation failed",
    );
    let (fs, clock, scheduler) = (FakeFs::new(), clock(), FakeScheduler::default());
    let error = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap_err();
    assert_eq!(error.code, "agent_linux_sys.command_failed");
    assert_eq!(
        reverts(&runner),
        1,
        "the previous configuration was put back"
    );
    assert!(!fs.has_file(&ticket_path()) && !fs.has_file(&backup_path()));
}

#[test]
fn a_second_change_is_refused_after_a_restart_too() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap();
    let error = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap_err();
    assert_eq!(error.code, NETWORK_REVERT_REQUIRED);
}

#[test]
fn ticket_ids_do_not_repeat_across_restarts() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    let first = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap()
        .ticket
        .id;
    module(&runner, &fs, &clock, &scheduler)
        .confirm(first)
        .unwrap();
    let second = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap()
        .ticket
        .id;
    assert!(second > first);
    assert!(fs.has_file(&format!("{STATE}/{SEQUENCE_FILE}")));
}

#[test]
fn a_networkd_revert_restores_the_previous_file_after_a_restart() {
    let networkd_file = "/etc/systemd/network/10-spotledger-eth0.network";
    let previous = "[Match]\nName=eth0\n\n[Network]\nDHCP=yes\n";
    let runner = FakeRunner::new()
        .with_response(
            "ip -j address show",
            ok_output(r#"[{"ifname":"eth0","operstate":"UP","addr_info":[]}]"#),
        )
        .with_response(
            "systemctl is-active NetworkManager",
            ok_output("inactive\n"),
        )
        .with_response(
            "systemctl is-active systemd-networkd",
            ok_output("active\n"),
        )
        .with_response("networkctl reload", ok_output(""))
        .with_response("networkctl reconfigure eth0", ok_output(""));
    let fs = FakeFs::new().with_file(networkd_file, previous);
    let (clock, scheduler) = (clock(), FakeScheduler::default());
    module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap();
    assert!(
        fs.file(networkd_file)
            .unwrap()
            .contains("Address=10.0.0.5/24")
    );
    assert!(
        fs.file(&backup_path())
            .unwrap()
            .contains("10-spotledger-eth0.network")
    );

    clock.advance(Duration::from_secs(WINDOW + 5));
    let recovery = module(&runner, &fs, &clock, &scheduler).recover().unwrap();
    assert!(matches!(recovery, Recovery::Reverted(_)));
    assert_eq!(fs.file(networkd_file).as_deref(), Some(previous));
}

#[test]
fn a_networkd_revert_removes_a_file_that_did_not_exist_before() {
    let networkd_file = "/etc/systemd/network/10-spotledger-eth0.network";
    let runner = FakeRunner::new()
        .with_response(
            "ip -j address show",
            ok_output(r#"[{"ifname":"eth0","operstate":"UP","addr_info":[]}]"#),
        )
        .with_response(
            "systemctl is-active NetworkManager",
            ok_output("inactive\n"),
        )
        .with_response(
            "systemctl is-active systemd-networkd",
            ok_output("active\n"),
        )
        .with_response("networkctl reload", ok_output(""))
        .with_response("networkctl reconfigure eth0", ok_output(""));
    let (fs, clock, scheduler) = (FakeFs::new(), clock(), FakeScheduler::default());
    module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap();
    assert!(fs.has_file(networkd_file));
    clock.advance(Duration::from_secs(WINDOW + 1));
    module(&runner, &fs, &clock, &scheduler).recover().unwrap();
    assert!(!fs.has_file(networkd_file));
}

#[test]
fn the_systemd_timer_is_built_as_an_argv_vector_and_refuses_a_shell() {
    let runner = FakeRunner::new()
        .with_response(
            "systemd-run --unit=spot-net-revert-7 --on-active=30s --timer-property=AccuracySec=1s -- /usr/libexec/spot-agent network-revert 7",
            ok_output(""),
        )
        .with_response("systemctl stop spot-net-revert-7.timer", ok_output(""));
    let scheduler = SystemdRunScheduler::new(
        &runner,
        vec![
            "/usr/libexec/spot-agent".to_owned(),
            "network-revert".to_owned(),
        ],
    )
    .unwrap();
    scheduler.schedule(7, 30).unwrap();
    scheduler.cancel(7).unwrap();

    // A timer that is already gone is not an error.
    runner.set_failure(
        "systemctl stop spot-net-revert-8.timer",
        5,
        "Unit spot-net-revert-8.timer not loaded.",
    );
    scheduler.cancel(8).unwrap();

    // A scheduling failure carries the schedule code.
    runner.set_failure(
        "systemd-run --unit=spot-net-revert-9 --on-active=1s --timer-property=AccuracySec=1s -- /usr/libexec/spot-agent network-revert 9",
        1,
        "Failed to connect to bus",
    );
    assert_eq!(
        scheduler.schedule(9, 0).unwrap_err().code,
        NETWORK_REVERT_SCHEDULE
    );

    assert!(SystemdRunScheduler::new(&runner, vec![]).is_err());
    assert!(
        SystemdRunScheduler::new(&runner, vec!["/bin/sh".to_owned(), "-c".to_owned()]).is_err()
    );
}

#[test]
fn a_module_with_no_state_directory_keeps_the_in_memory_behaviour() {
    let (runner, fs, clock) = (nm_runner(), FakeFs::new(), clock());
    let m = NetworkModule::new(&runner, &fs, clock.clone());
    let id = m.apply(&static_change(), WINDOW).unwrap().ticket.id;
    assert!(!fs.has_file(&ticket_path()));
    assert!(m.sweep().unwrap().is_none());
    clock.advance(Duration::from_secs(WINDOW + 1));
    assert!(m.sweep().unwrap().is_some());
    assert_eq!(id, 1);
}

// ---------------------------------------------------------------------------------------
// WP-ADM-04.T7: the deadline survives a clock step; a revert cannot race a confirm.
// ---------------------------------------------------------------------------------------

const BOOT_A: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\n";
const BOOT_B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n";
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
const UPTIME: &str = "/proc/uptime";

fn set_boot(fs: &FakeFs, boot: &str, uptime_secs: u64) {
    fs.set_file(BOOT_ID, boot);
    // The real file has two decimals; the guard reads whole seconds as text.
    fs.set_file(UPTIME, &format!("{uptime_secs}.47 99999.00\n"));
}

fn step_wall(clock: &ManualClock, seconds: i64) {
    let now = spot_core::Clock::now_utc(clock);
    clock.set_wall(now + time::Duration::seconds(seconds));
}

/// A ticket applied at uptime 1000 of boot A, with the wall clock then stepped by `step`
/// seconds and the uptime moved on by `elapsed`.
fn applied_then(
    step: i64,
    elapsed: u64,
) -> (FakeRunner, FakeFs, Arc<ManualClock>, FakeScheduler, u64) {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    set_boot(&fs, BOOT_A, 1000);
    let id = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap()
        .ticket
        .id;
    let ticket = fs.file(&ticket_path()).unwrap();
    assert!(ticket.contains("\"boot_id\":\"0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\""));
    assert!(ticket.contains("\"applied_uptime_secs\":1000"));
    step_wall(&clock, step);
    set_boot(&fs, BOOT_A, 1000 + elapsed);
    (runner, fs, clock, scheduler, id)
}

#[test]
fn a_clock_stepped_back_an_hour_does_not_extend_the_window_within_a_boot() {
    // The window is over by uptime, but the wall clock now says 59 minutes remain.
    let (runner, fs, clock, scheduler, id) = applied_then(-3600, WINDOW + 1);
    let m = module(&runner, &fs, &clock, &scheduler);
    assert_eq!(m.confirm(id).unwrap_err().code, NETWORK_REVERT_EXPIRED);
    assert!(m.sweep().unwrap().is_some(), "the sweep reverts at once");
    assert_eq!(reverts(&runner), 1);
}

#[test]
fn a_clock_stepped_forward_does_not_revert_early_within_a_boot() {
    // The wall clock jumped past the unix deadline, but only 5 seconds of uptime passed.
    let (runner, fs, clock, scheduler, id) = applied_then(3600, 5);
    let m = module(&runner, &fs, &clock, &scheduler);
    assert!(m.sweep().unwrap().is_none());
    assert_eq!(reverts(&runner), 0);
    m.confirm(id).unwrap();
    assert!(!fs.has_file(&ticket_path()));
}

#[test]
fn the_rearm_delay_after_a_step_is_the_uptime_remaining() {
    let (runner, fs, clock, scheduler, id) = applied_then(-3600, 10);
    let after = FakeScheduler::default();
    let Recovery::Rearmed { remaining_secs, .. } =
        module(&runner, &fs, &clock, &after).recover().unwrap()
    else {
        panic!("expected a re-arm");
    };
    assert_eq!(remaining_secs, WINDOW - 10);
    assert_eq!(
        after.events(),
        [
            format!("cancel {id}"),
            format!("schedule {id} {}", WINDOW - 10)
        ]
    );
    drop(scheduler);
}

#[test]
fn across_a_reboot_the_unix_deadline_decides() {
    // Boot B, with a smaller uptime than at apply time: the uptime means nothing now.
    let (runner, fs, clock, scheduler, id) = applied_then(0, 0);
    set_boot(&fs, BOOT_B, 3);
    clock.advance(Duration::from_secs(WINDOW + 1));
    let m = module(&runner, &fs, &clock, &scheduler);
    assert_eq!(m.confirm(id).unwrap_err().code, NETWORK_REVERT_EXPIRED);
    assert!(m.sweep().unwrap().is_some());

    // And inside the unix window after a reboot, nothing is due.
    let (runner, fs, clock, scheduler, id) = applied_then(0, 0);
    set_boot(&fs, BOOT_B, 3);
    clock.advance(Duration::from_secs(5));
    let m = module(&runner, &fs, &clock, &scheduler);
    assert!(m.sweep().unwrap().is_none());
    m.confirm(id).unwrap();
}

#[test]
fn a_ticket_written_before_t7_loads_and_uses_the_unix_deadline() {
    let (runner, fs, clock, scheduler) = (
        nm_runner(),
        FakeFs::new(),
        clock(),
        FakeScheduler::default(),
    );
    // No /proc files: a T5-era apply stores no boot fields.
    let id = module(&runner, &fs, &clock, &scheduler)
        .apply(&static_change(), WINDOW)
        .unwrap()
        .ticket
        .id;
    let ticket = fs.file(&ticket_path()).unwrap();
    assert!(!ticket.contains("boot_id") && !ticket.contains("applied_uptime_secs"));
    // The agent now runs on a machine that can tell its boot: the old ticket still goes by
    // the unix deadline.
    set_boot(&fs, BOOT_A, 5000);
    let m = module(&runner, &fs, &clock, &scheduler);
    assert!(m.sweep().unwrap().is_none());
    clock.advance(Duration::from_secs(WINDOW));
    assert!(m.sweep().unwrap().is_some());
    assert_eq!(reverts(&runner), 1);
    let _ = id;
}

#[test]
fn an_unreadable_uptime_falls_back_to_the_unix_deadline() {
    let (runner, fs, clock, scheduler, _id) = applied_then(0, 0);
    fs.set_file(UPTIME, "not a number\n");
    clock.advance(Duration::from_secs(WINDOW));
    assert!(
        module(&runner, &fs, &clock, &scheduler)
            .sweep()
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_held_lock_times_out_with_a_retryable_busy_error_after_a_bounded_wait() {
    let (runner, fs, clock, scheduler, id) = applied_then(0, 0);
    let lock_path = format!("{STATE}/network-revert.lock");
    let other_process = fs.try_lock_exclusive(Path::new(&lock_path)).unwrap();
    assert!(other_process.is_some());
    let sleeper = Arc::new(FakeSleeper::new());
    let m = module(&runner, &fs, &clock, &scheduler).with_sleeper(sleeper.clone());
    for error in [
        m.confirm(id).unwrap_err(),
        m.revert_ticket(id).unwrap_err(),
        m.sweep().unwrap_err(),
        m.recover().unwrap_err(),
        m.apply(&static_change(), WINDOW).unwrap_err(),
    ] {
        assert_eq!(error.code, NETWORK_REVERT_BUSY);
    }
    // 50 pauses of 100 ms per operation: five seconds at most, never a wall-clock read.
    assert_eq!(sleeper.count(), 5 * 50);
    assert!(
        fs.has_file(&ticket_path()),
        "a busy operation changed nothing"
    );
    assert_eq!(reverts(&runner), 0);

    // Once the other holder lets go, the same call goes through.
    drop(other_process);
    m.confirm(id).unwrap();
}

/// Sleeps for real, briefly, so a waiting thread lets the lock holder finish.
struct ShortSleeper;

impl Sleeper for ShortSleeper {
    fn sleep(&self, _duration: Duration) {
        std::thread::sleep(Duration::from_millis(4));
    }
}

/// A filesystem that stalls the calling thread at the `stall_at`-th call (counted across all
/// threads), so a test can cut the two racing operations at every point.
struct Stall<'a> {
    inner: &'a FakeFs,
    calls: AtomicUsize,
    stall_at: usize,
}

impl Stall<'_> {
    fn tick(&self) {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.stall_at {
            std::thread::sleep(Duration::from_millis(3));
        }
    }
}

impl FileSystem for Stall<'_> {
    fn read_to_string(&self, path: &Path) -> SysResult<String> {
        self.tick();
        self.inner.read_to_string(path)
    }
    fn exists(&self, path: &Path) -> SysResult<bool> {
        self.tick();
        self.inner.exists(path)
    }
    fn is_dir(&self, path: &Path) -> SysResult<bool> {
        self.tick();
        self.inner.is_dir(path)
    }
    fn list_dir(&self, path: &Path) -> SysResult<Vec<String>> {
        self.tick();
        self.inner.list_dir(path)
    }
    fn write_atomic(&self, path: &Path, contents: &str) -> SysResult<()> {
        self.tick();
        self.inner.write_atomic(path, contents)
    }
    fn remove_file(&self, path: &Path) -> SysResult<()> {
        self.tick();
        self.inner.remove_file(path)
    }
    fn create_dir_all(&self, path: &Path) -> SysResult<()> {
        self.tick();
        self.inner.create_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> SysResult<()> {
        self.tick();
        self.inner.rename(from, to)
    }
    fn metadata_len(&self, path: &Path) -> SysResult<u64> {
        self.tick();
        self.inner.metadata_len(path)
    }
    fn read_link(&self, path: &Path) -> SysResult<std::path::PathBuf> {
        self.tick();
        self.inner.read_link(path)
    }
    fn try_lock_exclusive(&self, path: &Path) -> SysResult<Option<spot_agent_linux_sys::FileLock>> {
        self.tick();
        self.inner.try_lock_exclusive(path)
    }
}

#[test]
fn a_confirm_and_a_timer_revert_interleaved_at_every_point_have_exactly_one_winner() {
    use spot_agent_linux_sys::RevertRun;

    let mut confirm_wins = 0;
    let mut revert_wins = 0;
    // The fake file system is the shared "disk"; each thread builds its own module, as the
    // agent and the timer's process each do. Stall one call, at every position, per round.
    for stall_at in 0..40 {
        for revert_first in [false, true] {
            let (runner, inner, clock, scheduler, id) = applied_then(0, 0);
            let fs = Stall {
                inner: &inner,
                calls: AtomicUsize::new(0),
                stall_at,
            };
            let barrier = Barrier::new(2);
            let (confirmed, reverted) = std::thread::scope(|scope| {
                let confirm = scope.spawn(|| {
                    barrier.wait();
                    if revert_first {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    module(&runner, &fs, &clock, &scheduler)
                        .with_sleeper(Arc::new(ShortSleeper))
                        .confirm(id)
                });
                let revert = scope.spawn(|| {
                    barrier.wait();
                    if !revert_first {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    module(&runner, &fs, &clock, &scheduler)
                        .with_sleeper(Arc::new(ShortSleeper))
                        .revert_ticket_checked(id)
                });
                (confirm.join().unwrap(), revert.join().unwrap())
            });
            let reverted = reverted.unwrap();
            match (&confirmed, &reverted) {
                (Ok(_), RevertRun::NothingPending) => confirm_wins += 1,
                (Err(error), RevertRun::Reverted(_)) => {
                    assert_eq!(error.code, "agent_linux_sys.refused", "{error}");
                    assert!(error.message.contains("no network change"), "{error}");
                    revert_wins += 1;
                }
                other => panic!("stall {stall_at}: not exactly one winner: {other:?}"),
            }
            // The final configuration is the winner's, and no state is left behind.
            let expected_reverts = usize::from(matches!(reverted, RevertRun::Reverted(_)));
            assert_eq!(reverts(&runner), expected_reverts, "stall {stall_at}");
            assert!(!inner.has_file(&ticket_path()) && !inner.has_file(&backup_path()));
        }
    }
    assert!(
        confirm_wins > 0 && revert_wins > 0,
        "{confirm_wins} {revert_wins}"
    );
}
