//! Windows Event Log reporting (`cfg(windows)` only; **unverified on this Linux CI
//! host** — reviewed by reading only, same caveat as every other module under
//! `crate::service`). The brief's title names "event-log and file logging" alongside
//! [`crate::logging`]'s rotating redacted files; this module is the event-log half.
//!
//! Every call registers an event source under the service's own name
//! (`NT SERVICE\SpotledgerServer` has no registered `EventMessageFile`, so Event Viewer
//! shows the raw string with its usual "the description for Event ID ... cannot be
//! found" preamble — acceptable for a lifecycle log with no dedicated message-table
//! resource DLL), reports one event, then deregisters immediately: these are rare,
//! lifecycle-level events (service start/stop/failure), not a hot path, so there is no
//! need to keep a handle open for the life of the service.
//!
//! Every message is redacted through [`spot_redact::Redactor`] before it reaches
//! `ReportEventW`, exactly like [`crate::logging::RedactingFileLogger`] — the Event Log
//! is as much a durable record as the rotating file, and the brief's "never put a
//! secret ... in a log line" applies here too.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
    REPORT_EVENT_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows::core::PCWSTR;

/// The event ID the service reports on reaching `SERVICE_RUNNING`.
pub const EVENT_ID_STARTED: u32 = 1;
/// The event ID the service reports on a clean `SERVICE_STOPPED`.
pub const EVENT_ID_STOPPED: u32 = 2;
/// The event ID the service reports when `run_foreground` returns an error (brief
/// review point 4: the same failure that makes [`crate::service::dispatch`] report a
/// non-zero `dwWin32ExitCode` to trigger SCM recovery).
pub const EVENT_ID_FAILED: u32 = 3;
/// The event ID the service reports when the bundled PostgreSQL is restarted after a
/// crash (brief: "a PostgreSQL crash restarts it with backoff").
pub const EVENT_ID_PG_RESTARTED: u32 = 4;

/// A UTF-16, NUL-terminated string for a Win32 wide-string argument (same convention as
/// [`crate::service::win_acl::wide`], duplicated here rather than shared across the two
/// small Windows-only modules to keep each one's `unsafe` self-contained).
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Redacts `message`, then reports it to the `SpotledgerServer` event source under
/// `event_id`/`event_type`. Registers and deregisters the source around the single
/// call: see the module docs for why that is fine for this call volume.
///
/// Never panics or propagates a Win32 failure as fatal: like
/// [`crate::service::dispatch::report`], an event-log write is best-effort diagnostics,
/// never a condition the service should fail over (a locked-down `ProgramData` or a
/// full event log must not stop the service from starting or stopping).
fn report_event(event_type: REPORT_EVENT_TYPE, event_id: u32, message: &str) {
    let Ok(redactor) = spot_redact::Redactor::from_policy(&spot_redact::RedactionPolicy::default())
    else {
        return;
    };
    let redacted = redactor.redact_str(message);
    let source_name = wide(crate::service::SERVICE_NAME);
    // SAFETY: `source_name` outlives this call; `PCWSTR::null()` for the server name
    // asks for the local machine's event log, the documented way to register locally.
    let handle: HANDLE =
        match unsafe { RegisterEventSourceW(PCWSTR::null(), PCWSTR(source_name.as_ptr())) } {
            Ok(handle) => handle,
            Err(_) => return,
        };
    let message_wide = wide(&redacted);
    let strings = [PCWSTR(message_wide.as_ptr())];
    // SAFETY: `handle` was just registered above; `strings` and the buffer it points
    // into outlive this call; no user SID or raw data is attached.
    unsafe {
        let _ = ReportEventW(
            handle,
            event_type,
            0,
            event_id,
            None,
            0,
            Some(&strings),
            None,
        );
    }
    // SAFETY: `handle` was returned by `RegisterEventSourceW` above and is deregistered
    // exactly once, immediately after its only use.
    unsafe {
        let _ = DeregisterEventSource(handle);
    }
}

/// Reports an informational event (service started, service stopped).
pub fn report_info(event_id: u32, message: &str) {
    report_event(EVENTLOG_INFORMATION_TYPE, event_id, message);
}

/// Reports a warning event (a degraded-but-recovering condition, e.g. a PostgreSQL
/// restart after a crash).
pub fn report_warning(event_id: u32, message: &str) {
    report_event(EVENTLOG_WARNING_TYPE, event_id, message);
}

/// Reports an error event (a failed start, an unrecoverable stop).
pub fn report_error(event_id: u32, message: &str) {
    report_event(EVENTLOG_ERROR_TYPE, event_id, message);
}
