//! The real Windows implementation, on the `windows` crate (Windows only).
//!
//! This module is compiled **only on Windows** (`#[cfg(windows)]`); the Linux CI host
//! never builds it, which is why the `windows` dependency is target-specific. It is the
//! one place in the crate that calls `unsafe`: every Win32 and COM function in the
//! `windows` crate is `unsafe`, so the crate's `unsafe_code` lint is `deny` everywhere
//! except here, where it is allowed with this note — the same sanction
//! `spot-pg-supervisor` uses for its two platform blocks. Every block below carries a
//! `SAFETY` note and the handles it opens are closed on every path.
//!
//! # What each submodule binds to
//!
//! - [`services`]: the Service Control Manager;
//! - [`updates`]: the Windows Update Agent, through its `Microsoft.Update.Session` COM
//!   object by late-bound `IDispatch` (no invented type library);
//! - [`firewall`]: the `HNetCfg.FwPolicy2` COM object's `INetFwPolicy2` interface;
//! - [`users`]: NetAPI.
//!
//! # Not compile-checked on the Linux host
//!
//! The acceptance for this task runs on Linux, where this module is not compiled; see the
//! PR for the exact list of calls that were written but not compiled here. The trait it
//! implements is fully exercised against [`crate::fake::FakeWin`] on Linux.

#![allow(unsafe_code)]

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{PCWSTR, PWSTR};

use crate::api::WinApi;
use crate::error::WindowsError;

mod firewall;
mod services;
mod updates;
mod users;

/// The real [`WinApi`] over the `windows` crate.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealWin;

impl RealWin {
    /// A real seam.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Encodes text as a NUL-terminated UTF-16 buffer.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A PCWSTR over a NUL-terminated buffer.
fn pcwstr(buffer: &[u16]) -> PCWSTR {
    PCWSTR(buffer.as_ptr())
}

/// Maps a `windows` result into [`WindowsError::Api`], reporting only the HRESULT code so
/// no OS text (which could carry a path) reaches a log.
fn api<T>(result: windows::core::Result<T>) -> Result<T, WindowsError> {
    result.map_err(|error| WindowsError::Api {
        detail: format!("HRESULT {:#010x}", error.code().0 as u32),
    })
}

/// Maps a NetAPI status code into [`WindowsError::Api`].
fn net(status: u32, what: &str) -> Result<(), WindowsError> {
    if status == windows::Win32::NetworkManagement::NetManagement::NERR_Success {
        Ok(())
    } else {
        Err(WindowsError::Api {
            detail: format!("{what} failed with status {status}"),
        })
    }
}

/// Runs `body` with COM initialised on this thread.
fn with_com<T>(body: impl FnOnce() -> Result<T, WindowsError>) -> Result<T, WindowsError> {
    // SAFETY: CoInitializeEx/CoUninitialize bracket the closure; the return value is only
    // an error code and the closure performs every COM call between them on this thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let result = body();
        CoUninitialize();
        result
    }
}

/// Turns a wide, NUL-terminated pointer into a `String` (lossy, empty on null).
fn pwstr_to_string(pointer: PWSTR) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: the pointer is a NUL-terminated wide string owned by the caller's buffer.
    unsafe { pointer.to_string().unwrap_or_default() }
}

/// The text of a wide buffer without its NUL.
fn name_to_string(buffer: &[u16]) -> String {
    String::from_utf16_lossy(buffer.strip_suffix(&[0]).unwrap_or(buffer))
}

/// Resolves a ProgID to a CLSID, so no COM identity is hard-coded in this crate.
fn prog_id(prog: windows::core::PCWSTR) -> Result<windows::core::GUID, WindowsError> {
    let mut clsid = windows::core::GUID::zeroed();
    // SAFETY: the ProgID is a static wide string and `clsid` is a valid out-pointer.
    unsafe {
        api(windows::Win32::System::Com::CLSIDFromProgID(
            prog, &mut clsid,
        ))?
    };
    Ok(clsid)
}

/// Keeps the `WinApi` import meaningful on every path of the crate.
#[allow(dead_code)]
type SharedApi = std::sync::Arc<dyn WinApi>;
