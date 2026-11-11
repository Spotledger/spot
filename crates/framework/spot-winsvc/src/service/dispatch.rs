//! The SCM dispatcher: the thin glue between `StartServiceCtrlDispatcherW` and
//! [`crate::orchestrator::Orchestrator`]. This module owns no business logic — it only
//! translates [`crate::orchestrator::Readiness`] into `SERVICE_STATUS` and runs the
//! orchestrator on a Tokio runtime for the lifetime of the service process.

use crate::error::WinsvcError;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, WIN32_ERROR};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::PWSTR;

/// The status handle the control handler reports through, stored as the raw pointer
/// value rather than [`SERVICE_STATUS_HANDLE`] itself: that type wraps a raw
/// `*mut c_void` and so is neither `Send` nor `Sync`, which a `static` requires.
/// [`report_full`] reconstructs the real handle from this `isize` for every call; no
/// thread ever dereferences the pointer itself, only passes it back into
/// `SetServiceStatus`, so round-tripping through `isize` loses nothing a Win32 call
/// needs. Set exactly once, before `StartServiceCtrlDispatcherW` can call back.
static STATUS_HANDLE: OnceLock<isize> = OnceLock::new();

/// The config path `run` was given, read by the service-main callback (the
/// `SERVICE_TABLE_ENTRYW` callback signature takes no user data slot on this binding).
static CONFIG_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// A monotonically increasing checkpoint sent with every `SERVICE_STOP_PENDING` report
/// while a stop is in progress (brief review: "send STOP_PENDING checkpoints to the SCM
/// during the long stop"). The SCM treats an unchanging checkpoint as a hung service and
/// kills the process once `dwWaitHint` elapses; incrementing it on every tick tells the
/// SCM the stop is still making progress and buys another `dwWaitHint` window.
static STOP_CHECKPOINT: AtomicU32 = AtomicU32::new(0);

/// How often the background task in [`service_main`] sends a fresh `SERVICE_STOP_PENDING`
/// checkpoint while a stop is in progress.
const STOP_CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// The `dwWaitHint` sent with every stop checkpoint: must comfortably exceed
/// [`STOP_CHECKPOINT_INTERVAL`] so the SCM never times the service out between two ticks.
const STOP_CHECKPOINT_WAIT_HINT_MS: u32 = 15_000;

/// Reports `state` to the SCM with a `dwWin32ExitCode` of 0 (the ordinary case: pending
/// states, and a clean `SERVICE_STOPPED`). Never panics on a failed report — a status
/// update is best-effort diagnostics, not a condition the service should die over.
fn report(state: u32, wait_hint_ms: u32) {
    report_full(state, wait_hint_ms, 0, 0);
}

/// Reports `state` to the SCM with an explicit `dwWin32ExitCode` and `dwCheckPoint`.
/// [`report`] is the common case (exit code 0, checkpoint 0); this is used for the two
/// cases that need more: a failed `run_foreground` (brief review point 4 — a clean
/// `SERVICE_STOPPED` with exit code 0 never triggers the SCM's recovery actions, even
/// though [`crate::service::install::configure_recovery`] enables them for non-crash
/// failures too) and the stop-progress checkpoints (review point 5).
fn report_full(state: u32, wait_hint_ms: u32, exit_code: u32, checkpoint: u32) {
    let Some(&raw_handle) = STATUS_HANDLE.get() else {
        return;
    };
    let handle = SERVICE_STATUS_HANDLE(raw_handle as *mut core::ffi::c_void);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STATUS_CURRENT_STATE(state),
        dwControlsAccepted: if state == SERVICE_RUNNING.0 {
            SERVICE_ACCEPT_STOP
        } else {
            0
        },
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint_ms,
    };
    // SAFETY: `handle` was returned by `RegisterServiceCtrlHandlerExW` earlier in this
    // same process and `status` is a correctly-populated, stack-local structure.
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

/// The control handler the SCM calls on `stop`/`shutdown`. Runs on the SCM's own thread;
/// it only requests a stop and returns immediately (the brief's grace period runs on the
/// orchestrator's own task, not on this callback).
unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP {
        report(SERVICE_STOP_PENDING.0, 30_000);
        crate::service::dispatch::request_stop();
        0
    } else {
        ERROR_CALL_NOT_IMPLEMENTED.0
    }
}

/// Set by the control handler; the service-main loop polls it instead of blocking the
/// SCM callback on the real shutdown sequence.
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Requests the running service to stop. Called from the control handler; also usable by
/// tests that drive the loop condition directly (there is no SCM on Linux).
pub fn request_stop() {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// `true` once [`request_stop`] has been called.
#[must_use]
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// The SCM calls this once `StartServiceCtrlDispatcherW` matches the service name.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = crate::service::SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: `name` outlives this call; `control_handler` is a valid `extern "system"`
    // callback with the signature `RegisterServiceCtrlHandlerExW` requires.
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(
            windows::core::PCWSTR(name.as_ptr()),
            Some(control_handler),
            None,
        )
    };
    let Ok(handle) = handle else {
        return;
    };
    let _ = STATUS_HANDLE.set(handle.0 as isize);
    report(SERVICE_START_PENDING.0, 60_000);

    let Some(config_path) = CONFIG_PATH.get().cloned() else {
        report(SERVICE_STOPPED.0, 0);
        return;
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => {
            report(SERVICE_STOPPED.0, 0);
            return;
        }
    };
    runtime.block_on(async move {
        let stopping_done = std::sync::Arc::new(AtomicBool::new(false));
        let checkpoint_task = {
            let stopping_done = stopping_done.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(STOP_CHECKPOINT_INTERVAL).await;
                    if stopping_done.load(Ordering::SeqCst) {
                        break;
                    }
                    if stop_requested() {
                        let checkpoint = STOP_CHECKPOINT.fetch_add(1, Ordering::SeqCst) + 1;
                        report_full(
                            SERVICE_STOP_PENDING.0,
                            STOP_CHECKPOINT_WAIT_HINT_MS,
                            0,
                            checkpoint,
                        );
                    }
                }
            })
        };
        let result = crate::run::run_foreground(&config_path, report_running, stop_requested).await;
        stopping_done.store(true, Ordering::SeqCst);
        checkpoint_task.abort();
        match result {
            Ok(()) => {
                report(SERVICE_STOPPED.0, 0);
                crate::service::eventlog::report_info(
                    crate::service::eventlog::EVENT_ID_STOPPED,
                    "SpotledgerServer stopped cleanly.",
                );
            }
            Err(err) => {
                // A non-zero `dwWin32ExitCode` (brief review point 4): a clean
                // `SERVICE_STOPPED` with exit code 0 never triggers the SCM's recovery
                // ladder, no matter what. With
                // `crate::service::install::configure_recovery`'s
                // `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG` set, a non-zero exit code here is
                // exactly what makes the SCM treat this "clean" stop as a failure worth
                // recovering from (its usual recovery-action trigger, an actual process
                // crash, never applies to a service that exits on its own).
                let exit_code = u32::try_from(err.exit_code()).unwrap_or(1).max(1);
                report_full(SERVICE_STOPPED.0, 0, exit_code, 0);
                crate::service::eventlog::report_error(
                    crate::service::eventlog::EVENT_ID_FAILED,
                    &format!("SpotledgerServer stopped on error: {err}"),
                );
            }
        }
    });
}

/// Reports `SERVICE_RUNNING` (brief: "report `SERVICE_RUNNING` only when
/// `/api/health/ready` answers"); called by [`crate::run::run_foreground`] once the
/// orchestrator first reaches [`crate::orchestrator::Readiness::Ready`]. Also records
/// the transition to the Windows Event Log (brief title: "event-log and file logging").
fn report_running() {
    report(SERVICE_RUNNING.0, 0);
    crate::service::eventlog::report_info(
        crate::service::eventlog::EVENT_ID_STARTED,
        "SpotledgerServer reached SERVICE_RUNNING: spot serve is ready.",
    );
}

/// Runs the SCM dispatcher loop. Blocks until the service process should exit.
///
/// # Errors
/// [`WinsvcError::Failed`] when `StartServiceCtrlDispatcherW` itself fails (for example,
/// run outside a service context — the SCM never started this process).
pub fn run(config_path: std::path::PathBuf) -> Result<(), WinsvcError> {
    let _ = CONFIG_PATH.set(config_path);
    let name = crate::service::SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: windows::core::PWSTR(name.as_ptr().cast_mut()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: windows::core::PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: `table` is a correctly NUL-terminated `SERVICE_TABLE_ENTRYW` array whose
    // backing `name` buffer outlives the (blocking) call.
    let result = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    result.map_err(|e| WinsvcError::failed(format!("Service Control Manager dispatch: {e}")))
}

#[allow(dead_code)]
fn silence_unused_warning(_: WIN32_ERROR) {}
