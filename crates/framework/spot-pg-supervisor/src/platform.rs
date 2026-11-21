//! Platform glue: the only `cfg`-split and the only `unsafe` code in the crate.
//!
//! Three jobs, per the task brief:
//! 1. **Fast shutdown request.** Unix signals the postmaster directly with `SIGINT`
//!    (fast shutdown); Windows goes through `pg_ctl stop -m fast`, the supported
//!    asynchronous shutdown path there.
//! 2. **A child is never left running if the supervisor dies.** Unix sets
//!    `prctl(PR_SET_PDEATHSIG, SIGKILL)` in the child before it execs and verifies the
//!    parent is still there (the classic race); Windows puts the child in a job object
//!    with `KILL_ON_JOB_CLOSE`.
//! 3. **`CREATE_NEW_PROCESS_GROUP` on Windows** so a Ctrl-C to the service does not
//!    reach the child before the supervisor acts.
//!
//! Every `unsafe` block in the crate is in this file, each with a `SAFETY` note: the two
//! sanctioned blocks (`prctl` in the child, the job object) and two integer-only `kill`
//! calls (the fast-stop signal and the liveness probe). They use the pinned `libc` (Unix) and `windows-sys` (Windows) the brief
//! allows; the alternative was hand-rolling the same syscalls.

use tokio::process::Child;

/// Configures the `postgres` spawn: parent-death signal (with the re-parent race
/// check) on Unix, `CREATE_NEW_PROCESS_GROUP` on Windows.
pub(crate) fn configure_spawn(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // Sanctioned unsafe block 1 of 2 (Unix). This closure runs in the forked child
        // just before `exec(postgres)`: it asks the kernel to kill the child with
        // SIGKILL when the supervisor dies, and closes the classic prctl race (child
        // scheduled only after the parent already exited) by checking the parent pid
        // is still our supervisor process.
        let supervisor_pid = std::process::id();
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != i32::try_from(supervisor_pid).unwrap_or(-1) {
                    // The supervisor died between fork and prctl.
                    libc::exit(1);
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
    let _ = cmd;
}

/// Installs the process-death guarantees for the spawned child: nothing to do beyond
/// `pre_exec` on Unix, the kill-on-close job object on Windows.
pub(crate) fn after_spawn(child: &Child) {
    #[cfg(windows)]
    attach_job_object(child);
    #[cfg(not(windows))]
    let _ = child;
}

/// Windows: put the child in a job object with `KILL_ON_JOB_CLOSE`, so postgres and
/// its backends cannot outlive the supervisor process.
#[cfg(windows)]
fn attach_job_object(child: &Child) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    let Some(pid) = child.id() else {
        return; // already exited; the monitor reports the exit
    };
    // Sanctioned unsafe block 2 of 2 (Windows). Every handle here is either closed on
    // every path or deliberately leaked for the process lifetime (the job handle must
    // outlive every restart so KILL_ON_JOB_CLOSE covers the whole tree).
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            tracing::error!("could not create the PostgreSQL job object");
            return;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured == 0 {
            CloseHandle(job);
            tracing::error!("could not set KILL_ON_JOB_CLOSE on the PostgreSQL job object");
            return;
        }
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            CloseHandle(job);
            return; // the child exited between spawn and assignment
        }
        let assigned = AssignProcessToJobObject(job, process);
        CloseHandle(process);
        if assigned == 0 {
            CloseHandle(job);
            tracing::error!("could not assign postgres to the job object");
        }
        // `job` stays open on purpose: killing on close is the point.
    }
}

/// Asks the child for a **fast** shutdown. Fire-and-forget: the authoritative outcome
/// is the child exit the monitor waits for with the grace period.
pub(crate) fn request_fast_stop(
    child: &mut Child,
    pg_ctl: &std::path::Path,
    data_dir: &std::path::Path,
) {
    #[cfg(unix)]
    {
        let _ = (pg_ctl, data_dir);
        // Fast shutdown == SIGINT for a postmaster: stop accepting connections, let
        // sessions finish, checkpoint, exit. We own the child, so signal it directly.
        if let Some(pid) = child.id() {
            // A pid that does not fit `i32` must never reach `kill`: 0 would signal our
            // whole process group.
            let Ok(pid_i32) = i32::try_from(pid) else {
                tracing::warn!(pid, "pid out of range, no SIGINT sent");
                return;
            };
            // SAFETY: `kill` takes two integers and no pointers; `pid_i32` is a positive
            // pid of a child we own.
            let delivered = unsafe { libc::kill(pid_i32, libc::SIGINT) };
            if delivered != 0 {
                tracing::warn!(pid, "SIGINT could not be delivered (child already gone?)");
            }
        }
    }
    #[cfg(windows)]
    {
        // The Windows path stops the postmaster through `pg_ctl`, not through the child handle.
        let _ = &child;
        // The supported fast shutdown on Windows goes through `pg_ctl stop -m fast`,
        // which signals the postmaster through its console-control plumbing. It runs
        // detached; the monitor keeps waiting for the child with the grace period.
        let pg_ctl = pg_ctl.to_path_buf();
        let data_dir = data_dir.to_path_buf();
        tokio::spawn(async move {
            let mut cmd = tokio::process::Command::new(&pg_ctl);
            cmd.arg("stop")
                .arg("-m")
                .arg("fast")
                .arg("-D")
                .arg(&data_dir);
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
            match cmd.output().await {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    tracing::warn!(
                        status = %output.status,
                        "pg_ctl stop -m fast did not report success"
                    );
                }
                Err(error) => tracing::warn!(%error, "could not run pg_ctl stop -m fast"),
            }
        });
    }
}

/// Hard kill of the child (`start_kill` + reap; postmaster backends notice the
/// postmaster death on their own).
pub(crate) async fn kill_child(child: &mut Child) {
    if let Err(error) = child.start_kill() {
        if error.kind() == std::io::ErrorKind::InvalidInput {
            // The child was never spawned or already reaped; the wait below then
            // returns at once or the monitor has already reported the exit.
        } else {
            tracing::warn!(%error, "kill of the child failed");
        }
    }
    let _ = child.wait().await;
}

/// Blocks until the host asks the *supervisor* to shut down (SIGTERM/SIGINT on Unix,
/// Ctrl-C on Windows). The service wrapper decides whether to stop the child; this is
/// the exported entry point the wrapper (and only the wrapper) calls to wait for the
/// host request.
pub async fn host_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::SignalKind;
        let term = async {
            match tokio::signal::unix::signal(SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term => {}
        }
    }
    #[cfg(windows)]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::future::pending::<()>().await;
    }
}

/// `true` when a process with this pid exists on this machine (`kill -0` semantics). A
/// process we may not signal (`EPERM`) exists, so it counts as alive.
#[cfg(unix)]
pub(crate) fn process_exists(pid: u32) -> bool {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return false;
    };
    if pid_i32 <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 takes two integers and sends nothing; `pid_i32` is
    // positive, so it names one process and never a group.
    let rc = unsafe { libc::kill(pid_i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
