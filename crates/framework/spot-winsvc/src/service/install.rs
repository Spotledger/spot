//! Install, uninstall, start and stop through the Service Control Manager (SCM). Every
//! function opens the SCM, does one thing, and closes every handle on every path
//! (including the error paths) — Windows handles are not value types the Rust
//! borrow-checker tracks, so this is this module's own discipline, not the compiler's.

use crate::error::WinsvcError;
use std::path::Path;
use std::time::Duration;
use windows::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE,
    SC_MANAGER_ALL_ACCESS, SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, SERVICE_CONTROL_STOP,
    SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONS_FLAG, SERVICE_FAILURE_ACTIONSW, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_PROCESS, SERVICE_STOPPED,
    SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

/// How long [`start`]/[`stop`] poll the SCM for the service to actually reach
/// `SERVICE_RUNNING`/`SERVICE_STOPPED` before giving up (brief: the host, not the SCM
/// call itself, decides when the operation is "done" — `StartServiceW`/`ControlService`
/// only ask the SCM to begin the transition and return as soon as it is *accepted*).
const WAIT_FOR_STATE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often [`wait_for_state`] re-queries the SCM while polling.
const WAIT_FOR_STATE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A UTF-16, NUL-terminated string for a Win32 wide-string argument.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens the SCM with `desired_access`.
fn open_scm(desired_access: u32) -> Result<SC_HANDLE, WinsvcError> {
    // SAFETY: `OpenSCManagerW` is a plain FFI call; `None` machine/database names mean
    // "local machine, services database", a documented, always-valid combination.
    let handle = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), desired_access) }
        .map_err(|e| WinsvcError::failed(format!("opening the Service Control Manager: {e}")))?;
    Ok(handle)
}

/// Closes an SC handle, ignoring the result — this module never has a meaningful
/// recovery for a failed close, and every call site already reports its own error.
fn close(handle: SC_HANDLE) {
    // SAFETY: `handle` was returned by a prior `OpenSCManagerW`/`OpenServiceW`/
    // `CreateServiceW` call on this path and is closed exactly once.
    unsafe {
        let _ = CloseServiceHandle(handle);
    }
}

/// Registers the `SpotledgerServer` service to run `binary_path --config config_path run`,
/// automatic (delayed) start, no dependencies, under the virtual service account, with
/// the recovery ladder and description the brief fixes. Also creates and ACL-protects
/// the two product directories (brief "Directories": `%ProgramFiles%\Spotledger`,
/// `%ProgramData%\Spotledger` — see [`crate::service::win_acl`]), since install time is
/// the one point this host is guaranteed to run with Administrator rights.
///
/// Idempotent: an already-registered service is left exactly as it is (its config is
/// never re-applied here). This is a deliberate choice, not an oversight — re-running
/// `install` is how the updater and a repair-install both detect "already present", and
/// a silent reconfiguration on every such call would make an administrator's manual
/// `sc config` change disappear the next time setup runs. A genuine config change (a new
/// recovery ladder, a new description) needs its own `reconfigure`/`upgrade`-style entry
/// point, which this task does not add — flagged under the task's `## Open questions`
/// for the lead to confirm or to ask for one.
///
/// # Errors
/// [`WinsvcError::Failed`] on any SCM call failure.
pub fn install(binary_path: &Path, config_path: &Path) -> Result<(), WinsvcError> {
    crate::service::win_acl::secure_program_files_dir(&crate::dirs::program_files_dir())?;
    crate::service::win_acl::secure_program_data_dir(&crate::dirs::program_data_dir())?;

    let scm = open_scm(SC_MANAGER_ALL_ACCESS)?;
    let command_line = format!(
        "\"{}\" run --config \"{}\"",
        binary_path.display(),
        config_path.display()
    );
    let name = wide(crate::service::SERVICE_NAME);
    let display_name = wide(crate::service::SERVICE_NAME);
    let bin_path = wide(&command_line);
    let account = wide(crate::service::SERVICE_ACCOUNT);

    // SAFETY: every pointer argument is a `wide()`-built, NUL-terminated buffer kept
    // alive for the duration of this call; `CreateServiceW` does not retain them.
    let result = unsafe {
        CreateServiceW(
            scm,
            PCWSTR(name.as_ptr()),
            PCWSTR(display_name.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(bin_path.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(), // no dependencies (brief: "dependency none")
            PCWSTR(account.as_ptr()),
            PCWSTR::null(), // the virtual account needs no password
        )
    };

    let service = match result {
        Ok(handle) => handle,
        Err(e) if e.code() == ERROR_SERVICE_EXISTS.to_hresult() => {
            close(scm);
            return Ok(());
        }
        Err(e) => {
            close(scm);
            return Err(WinsvcError::failed(format!("registering the service: {e}")));
        }
    };

    let configure_result = configure_delayed_start(service)
        .and_then(|()| configure_recovery(service))
        .and_then(|()| configure_description(service));

    close(service);
    close(scm);
    configure_result
}

/// Sets "automatic (delayed) start" (brief: "automatic (delayed) start").
fn configure_delayed_start(service: SC_HANDLE) -> Result<(), WinsvcError> {
    let mut info = SERVICE_DELAYED_AUTO_START_INFO {
        fDelayedAutostart: BOOL(1),
    };
    // SAFETY: `info` is a valid, correctly-sized structure for
    // `SERVICE_CONFIG_DELAYED_AUTO_START_INFO`, as the Win32 API requires.
    let ok = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
            Some(std::ptr::addr_of_mut!(info).cast()),
        )
    };
    ok.map_err(|e| WinsvcError::failed(format!("setting delayed auto-start: {e}")))
}

/// Sets the recovery ladder (brief: "restart after 5 s, 30 s, 60 s with reset period 1
/// day"). Also sets `fFailureActionsOnNonCrashFailures` (`SERVICE_CONFIG_FAILURE_ACTIONS_FLAG`):
/// without it, Windows only runs the recovery actions when the process is *killed* by the
/// SCM/the system, never when it exits on its own with a non-zero code — exactly the case
/// [`crate::service::dispatch`] reports when `run_foreground` returns `Err` (see
/// `WinsvcError::exit_code`). Without this flag the recovery ladder the brief asks for
/// would silently never trigger on an ordinary reported failure.
fn configure_recovery(service: SC_HANDLE) -> Result<(), WinsvcError> {
    let mut actions: Vec<SC_ACTION> = crate::service::RESTART_DELAYS_MS
        .iter()
        .map(|delay_ms| SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: *delay_ms,
        })
        .collect();
    let mut failure_actions = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: crate::service::RESET_PERIOD_SECS,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: u32::try_from(actions.len()).unwrap_or(0),
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: `actions` outlives this call and `failure_actions` points only at it.
    let ok = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(std::ptr::addr_of_mut!(failure_actions).cast()),
        )
    };
    ok.map_err(|e| WinsvcError::failed(format!("setting the recovery ladder: {e}")))?;

    let mut flag = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: BOOL(1),
    };
    // SAFETY: `flag` is a valid, correctly-sized structure for
    // `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG`, as the Win32 API requires.
    let ok = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            Some(std::ptr::addr_of_mut!(flag).cast()),
        )
    };
    ok.map_err(|e| {
        WinsvcError::failed(format!(
            "enabling recovery actions on non-crash failures: {e}"
        ))
    })
}

/// Sets the service description (brief: "description set").
fn configure_description(service: SC_HANDLE) -> Result<(), WinsvcError> {
    let mut text = wide(crate::service::SERVICE_DESCRIPTION);
    let mut info = SERVICE_DESCRIPTIONW {
        lpDescription: PWSTR(text.as_mut_ptr()),
    };
    // SAFETY: `text` outlives this call and `info` points only at it.
    let ok = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(std::ptr::addr_of_mut!(info).cast()),
        )
    };
    ok.map_err(|e| WinsvcError::failed(format!("setting the service description: {e}")))
}

/// Unregisters the service. Idempotent: an already-absent service is not an error.
///
/// # Errors
/// [`WinsvcError::Failed`] on any SCM call failure other than "does not exist".
pub fn uninstall() -> Result<(), WinsvcError> {
    let scm = open_scm(SC_MANAGER_ALL_ACCESS)?;
    let name = wide(crate::service::SERVICE_NAME);
    // SAFETY: `name` is a valid NUL-terminated wide string kept alive for this call.
    let open_result = unsafe { OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) };
    let service = match open_result {
        Ok(handle) => handle,
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            close(scm);
            return Ok(());
        }
        Err(e) => {
            close(scm);
            return Err(WinsvcError::failed(format!("opening the service: {e}")));
        }
    };
    // SAFETY: `service` is the handle just opened above.
    let result = unsafe { DeleteService(service) };
    close(service);
    close(scm);
    result.map_err(|e| WinsvcError::failed(format!("removing the service: {e}")))
}

/// Starts the registered service via the SCM (as an administrator would from
/// `services.msc`; distinct from [`crate::service::dispatch`], which is how the running
/// service process itself enters). Polls until the service actually reports
/// `SERVICE_RUNNING` (brief: an upgrade's health check must observe the *started*
/// service, not merely that the SCM accepted the start request) — `StartServiceW`
/// returns as soon as the request is accepted, long before `service_main` has even run.
///
/// # Errors
/// [`WinsvcError::prerequisite`] when the service is not registered,
/// [`WinsvcError::Failed`] on any other SCM call failure, or when the service never
/// reaches `SERVICE_RUNNING` within [`WAIT_FOR_STATE_TIMEOUT`].
pub fn start() -> Result<(), WinsvcError> {
    let scm = open_scm(SC_MANAGER_ALL_ACCESS)?;
    let name = wide(crate::service::SERVICE_NAME);
    // SAFETY: `name` is a valid NUL-terminated wide string kept alive for this call.
    let open_result = unsafe { OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) };
    let service = match open_result {
        Ok(handle) => handle,
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            close(scm);
            return Err(WinsvcError::prerequisite(crate::service::SERVICE_NAME));
        }
        Err(e) => {
            close(scm);
            return Err(WinsvcError::failed(format!("opening the service: {e}")));
        }
    };
    // SAFETY: no extra arguments are passed; `None` is the documented "no arguments".
    let result = unsafe { StartServiceW(service, None) };
    if let Err(e) = result {
        close(service);
        close(scm);
        return Err(WinsvcError::failed(format!("starting the service: {e}")));
    }
    let waited = wait_for_state(service, SERVICE_RUNNING, WAIT_FOR_STATE_TIMEOUT);
    close(service);
    close(scm);
    waited
}

/// Stops the running service via the SCM. Polls until the service actually reports
/// `SERVICE_STOPPED` (see [`start`]'s docs for why a single `ControlService` call is not
/// enough: it only asks the SCM to begin the stop, which can take up to the host's own
/// shutdown grace period while `run_foreground`'s stop order runs).
///
/// # Errors
/// [`WinsvcError::prerequisite`] when the service is not registered,
/// [`WinsvcError::Failed`] on any other SCM call failure, or when the service never
/// reaches `SERVICE_STOPPED` within [`WAIT_FOR_STATE_TIMEOUT`].
pub fn stop() -> Result<(), WinsvcError> {
    let scm = open_scm(SC_MANAGER_ALL_ACCESS)?;
    let name = wide(crate::service::SERVICE_NAME);
    // SAFETY: `name` is a valid NUL-terminated wide string kept alive for this call.
    let open_result = unsafe { OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) };
    let service = match open_result {
        Ok(handle) => handle,
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            close(scm);
            return Err(WinsvcError::prerequisite(crate::service::SERVICE_NAME));
        }
        Err(e) => {
            close(scm);
            return Err(WinsvcError::failed(format!("opening the service: {e}")));
        }
    };
    let mut status = SERVICE_STATUS::default();
    // SAFETY: `status` is a valid, correctly-sized out-buffer, as the API requires.
    let result = unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) };
    if let Err(e) = result {
        close(service);
        close(scm);
        return Err(WinsvcError::failed(format!("stopping the service: {e}")));
    }
    let waited = wait_for_state(service, SERVICE_STOPPED, WAIT_FOR_STATE_TIMEOUT);
    close(service);
    close(scm);
    waited
}

/// Polls `service` via `QueryServiceStatusEx` until `dwCurrentState` equals `target` or
/// `timeout` elapses. This function reads elapsed time from `std::time::Instant` purely
/// to bound the poll loop's own duration (not the wall clock the brief means by "time is
/// an argument" — there is no business decision here, only "give up eventually").
fn wait_for_state(
    service: SC_HANDLE,
    target: SERVICE_STATUS_CURRENT_STATE,
    timeout: Duration,
) -> Result<(), WinsvcError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut status = SERVICE_STATUS_PROCESS::default();
        let mut bytes_needed = 0u32;
        // SAFETY: `status` is a correctly-sized out-buffer for `SC_STATUS_PROCESS_INFO`,
        // as the API requires; `bytes_needed` is a valid out-parameter.
        let queried = unsafe {
            QueryServiceStatusEx(
                service,
                SC_STATUS_PROCESS_INFO,
                Some(std::slice::from_raw_parts_mut(
                    std::ptr::addr_of_mut!(status).cast::<u8>(),
                    std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
                )),
                &mut bytes_needed,
            )
        };
        if let Err(e) = queried {
            return Err(WinsvcError::failed(format!("querying service status: {e}")));
        }
        if status.dwCurrentState == target {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(WinsvcError::failed(format!(
                "the service did not reach state {} within {}s",
                target.0,
                timeout.as_secs()
            )));
        }
        std::thread::sleep(WAIT_FOR_STATE_POLL_INTERVAL);
    }
}
