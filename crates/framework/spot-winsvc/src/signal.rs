//! Sending the `spot serve` child a graceful-stop signal (brief: "on stop: send the stop
//! signal, wait for the shutdown grace of the host ... then stop PostgreSQL").
//!
//! On Unix, `SIGTERM` is the portable, well-understood graceful-stop signal and `spot`'s
//! own host already drains on it.
//!
//! On Windows there is no `SIGTERM`, but `spot serve` is not silent about what it does
//! listen for: `spot-cli/src/main.rs`'s `shutdown()` future calls
//! `tokio::signal::ctrl_c().await` unconditionally (its `#[cfg(unix)]` branch additionally
//! races a `SIGTERM` listener, but the `#[cfg(not(unix))]` branch — the one that compiles
//! on Windows — is exactly `tokio::signal::ctrl_c().await`). On Windows, `tokio`
//! implements that by registering a console control handler and resolving on
//! `CTRL_C_EVENT` specifically (see `tokio::signal::windows::ctrl_c`), not
//! `CTRL_BREAK_EVENT`. So the real, already-documented shutdown protocol for a Windows
//! `spot serve` child is: deliver it a `CTRL_C_EVENT`.
//!
//! The Win32 catch: `GenerateConsoleCtrlEvent(CTRL_C_EVENT, ...)` cannot be scoped to one
//! process group (`dwProcessGroupId` must be `0` for `CTRL_C_EVENT`; that is documented
//! as unsupported for `CTRL_C_EVENT`, and only `CTRL_BREAK_EVENT` accepts a non-zero
//! group). A `0` group broadcasts to *every* process attached to the caller's current
//! console — dangerous if that console is shared with anything else. [`crate::app`]
//! avoids that by giving the child its **own** console (`CREATE_NEW_CONSOLE`, see
//! `crate::app::RealApp::start`) rather than inheriting this service's (which has none —
//! Windows services never have a console). This module then uses the standard
//! attach/detach technique to reach only that console:
//! 1. `AttachConsole` this (console-less) process to the child's console by PID.
//! 2. `SetConsoleCtrlHandler(None, true)` so *this* process ignores the event it is about
//!    to broadcast (otherwise the service host would also receive its own
//!    `CTRL_C_EVENT` and fall over on exactly the mechanism meant for the child).
//! 3. `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)`: broadcasts to every process on the
//!    now-shared console, which (thanks to step 2 and the dedicated console from step 0)
//!    is only the child.
//! 4. Undo step 2, then `FreeConsole` to detach again.
//!
//! This is the standard, documented pattern for a console-less parent to deliver Ctrl+C
//! to one child (there is no Win32 API to target `CTRL_C_EVENT` more directly). It is
//! best-effort: every step's failure is swallowed and falls through to
//! [`crate::app::RealApp::stop`]'s existing grace-then-kill, so a console API that
//! misbehaves on some Windows configuration can only ever lose the "graceful" half of the
//! stop, never block it.
//!
//! **Caveat, stated plainly because it cannot be exercised on this Linux CI host:** this
//! whole mechanism is read-reviewed and type-checked only, never run against a real
//! `spot serve` process on a real Windows console. The attach/detach dance is a
//! well-established technique (used by, for example, service-wrapper tools that need to
//! Ctrl+C a console child), but this crate has no Windows runner to prove it against the
//! real `spot-cli` binary. If a Windows run shows it does not reliably reach `spot
//! serve`'s `ctrl_c()` listener, the fallback is unchanged and safe: the existing
//! grace-period-then-kill in [`crate::app::RealApp::stop`]. A Windows run proving (or
//! disproving) this is exactly the kind of evidence this task still owes — see the
//! crate-level `## Open questions`.

#[cfg(unix)]
/// Sends the child `SIGTERM` (the portable graceful-stop signal, Unix only).
pub fn request_graceful_stop(child: &tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };
    // SAFETY: `pid` is this process's own child, read a moment ago from `tokio`; sending
    // it `SIGTERM` is the documented, always-valid use of `kill(2)`.
    unsafe {
        let _ = libc::kill(i32::try_from(pid).unwrap_or(i32::MAX), libc::SIGTERM);
    }
}

#[cfg(windows)]
/// Delivers a real `CTRL_C_EVENT` to the `spot serve` child (see the module docs for why
/// this, and only this, reaches `spot-cli`'s own `tokio::signal::ctrl_c()` shutdown
/// listener). Every failure is silent and best-effort: [`crate::app::RealApp::stop`]'s
/// grace period and eventual `kill` are what guarantee the child never outlives the
/// service either way.
pub fn request_graceful_stop(child: &tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };
    if let Err(step) = send_ctrl_c(pid) {
        tracing::warn!(
            pid,
            step,
            "the graceful-stop console signal was not delivered"
        );
    }
}

/// The attach, ignore, broadcast, detach sequence; the error names the failing step.
#[cfg(windows)]
fn send_ctrl_c(pid: u32) -> Result<(), String> {
    use windows::Win32::System::Console::{
        AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };
    // SAFETY: a process can be attached to only one console, and `AttachConsole` fails
    // with access-denied if it already has one (a service has none, but a process run
    // from a terminal does). Detaching first is a no-op for a service and makes the
    // attach below work everywhere.
    unsafe {
        let _ = FreeConsole();
    }
    // SAFETY: `pid` is this process's own child (spawned by `crate::app::RealApp::start`
    // with `CREATE_NEW_CONSOLE`, so it owns a console distinct from this service's
    // none); attaching to it is the documented way for a console-less process to reach
    // a specific child's console. If the child's console is already gone (it exited, or
    // never got one), this fails and the whole function returns without side effects.
    unsafe { AttachConsole(pid) }.map_err(|e| format!("AttachConsole: {e}"))?;
    // SAFETY: `None` as the handler routine, `true` to add: the documented way to make
    // *this* process ignore `CTRL_C_EVENT` so the broadcast in the next step reaches only
    // the child now sharing this console, not the service host itself.
    let ignored = unsafe { SetConsoleCtrlHandler(None, true) }.is_ok();
    // SAFETY: `CTRL_C_EVENT` with a process-group id of `0` is the only form Win32
    // supports for this event (`CTRL_C_EVENT` cannot target a specific non-zero group);
    // it broadcasts to every process on the console this call just attached to, which is
    // only the child (this process ignores it, from the call just above).
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }
        .map_err(|e| format!("GenerateConsoleCtrlEvent: {e}"));
    // The event is delivered asynchronously, on a new thread in each attached process.
    // Restoring normal Ctrl+C handling before that thread runs would let the event kill
    // this very process (a Windows run showed exactly that: STATUS_CONTROL_C_EXIT), so
    // keep ignoring it for a moment first. This runs on the stop path, where 0.5 s is
    // noise next to the grace period.
    std::thread::sleep(std::time::Duration::from_millis(500));
    if ignored {
        // SAFETY: undoes the `SetConsoleCtrlHandler(None, true)` above, restoring this
        // process's normal Ctrl+C handling before it detaches from the console.
        unsafe {
            let _ = SetConsoleCtrlHandler(None, false);
        }
    }
    // SAFETY: detaches this process from the child's console, leaving it exactly as
    // console-less as it was before this function ran (a Windows service has none).
    unsafe {
        let _ = FreeConsole();
    }
    sent
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn sigterm_reaches_a_real_child_process() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("spawning sleep");
        request_graceful_stop(&child);
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .expect("sleep exits promptly on SIGTERM")
            .expect("wait succeeds");
        assert!(!status.success());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The child half of [`ctrl_c_reaches_a_child_in_its_own_console`]: waits on exactly
    /// the future `spot serve` shuts down on (`tokio::signal::ctrl_c`). Only meant to be
    /// run by the parent test, so it is ignored in a normal run.
    #[tokio::test]
    #[ignore = "spawned by ctrl_c_reaches_a_child_in_its_own_console"]
    async fn ctrl_c_helper() {
        // CI runners start their steps in a process group with Ctrl+C disabled, and the
        // flag is inherited by every descendant; a process started by the Service Control
        // Manager does not have it. Clear it so the helper behaves like `spot serve`
        // does under the service.
        // SAFETY: `None`, `false` removes the "ignore Ctrl+C" attribute from this process.
        unsafe {
            let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(None, false);
        }
        tokio::select! {
            received = tokio::signal::ctrl_c() => assert!(received.is_ok()),
            () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                panic!("no CTRL_C_EVENT within 30 s");
            }
        }
    }

    /// Spawns this test binary as the helper above, in its own console exactly like
    /// `spot serve`, and checks the graceful-stop signal makes it exit successfully.
    #[tokio::test]
    async fn ctrl_c_reaches_a_child_in_its_own_console() {
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        let mut child = tokio::process::Command::new(std::env::current_exe().expect("exe"))
            .args(["--exact", "--ignored", "--test-threads=1"])
            .arg("signal::windows_tests::ctrl_c_helper")
            .creation_flags(CREATE_NEW_CONSOLE)
            .kill_on_drop(true)
            .spawn()
            .expect("spawning the helper");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let pid = child.id().expect("child pid");
        send_ctrl_c(pid).expect("the console signal is delivered");
        let exited = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
        let status = exited
            .expect("the child must exit promptly on CTRL_C_EVENT")
            .expect("wait succeeds");
        assert!(status.success(), "the helper saw the event: {status:?}");
    }
}
