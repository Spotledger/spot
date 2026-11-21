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
//! - `services`: the Service Control Manager;
//! - `updates`: the Windows Update Agent, through its `Microsoft.Update.Session` COM
//!   object by late-bound `IDispatch` (no invented type library);
//! - `firewall`: the `HNetCfg.FwPolicy2` COM object's `INetFwPolicy2` interface;
//! - `users`: NetAPI.
//!
//! # Verification
//!
//! This module does not compile on Linux. It is type-checked and linted for
//! `x86_64-pc-windows-gnu` from Linux (see the pipeline notes), which proves the
//! bindings match the `windows` crate but not that the calls behave as intended; that
//! needs a Windows runner. The trait it implements is fully exercised against
//! [`crate::fake::FakeWin`] on Linux.

#![allow(unsafe_code)]

use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{PCWSTR, PWSTR};

use crate::api::{
    FirewallRule, LocalGroup, LocalUser, Profile, RecoveryConfig, ServiceInfo, StartType,
    UpdateInfo, WinApi,
};
use crate::error::WindowsError;
use crate::secrets::Secret;

mod firewall;
mod services;
mod updates;
mod users;
mod variant;

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

/// A buffer the NetAPI allocated for the caller, freed with `NetApiBufferFree` on drop.
struct NetBuffer(*mut u8);

impl NetBuffer {
    fn new() -> Self {
        Self(std::ptr::null_mut())
    }
}

impl Drop for NetBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer was returned by a NetAPI call that allocates for the
            // caller and is freed exactly once, here.
            unsafe {
                let _ = NetApiBufferFree(Some(self.0.cast_const().cast()));
            }
        }
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
    // SAFETY: the ProgID is a static, NUL-terminated wide string.
    unsafe { api(windows::Win32::System::Com::CLSIDFromProgID(prog)) }
}

impl WinApi for RealWin {
    fn list_services(&self) -> Result<Vec<ServiceInfo>, WindowsError> {
        RealWin::list_services(self)
    }

    fn service_status(&self, name: &str) -> Result<ServiceInfo, WindowsError> {
        RealWin::service_status(self, name)
    }

    fn start_service(&self, name: &str, timeout_seconds: u64) -> Result<(), WindowsError> {
        RealWin::start_service(self, name, timeout_seconds)
    }

    fn stop_service(&self, name: &str, timeout_seconds: u64) -> Result<(), WindowsError> {
        RealWin::stop_service(self, name, timeout_seconds)
    }

    fn set_service_start_type(
        &self,
        name: &str,
        start_type: StartType,
    ) -> Result<(), WindowsError> {
        RealWin::set_service_start_type(self, name, start_type)
    }

    fn service_recovery(&self, name: &str) -> Result<RecoveryConfig, WindowsError> {
        RealWin::service_recovery(self, name)
    }

    fn set_service_recovery(
        &self,
        name: &str,
        recovery: &RecoveryConfig,
    ) -> Result<(), WindowsError> {
        RealWin::set_service_recovery(self, name, recovery)
    }

    fn search_updates(&self, include_hidden: bool) -> Result<Vec<UpdateInfo>, WindowsError> {
        RealWin::search_updates(self, include_hidden)
    }

    fn download_update(&self, id: &str) -> Result<(), WindowsError> {
        RealWin::download_update(self, id)
    }

    fn install_update(&self, id: &str) -> Result<(), WindowsError> {
        RealWin::install_update(self, id)
    }

    fn reboot_required(&self) -> Result<bool, WindowsError> {
        RealWin::reboot_required(self)
    }

    fn schedule_reboot(&self, at_unix_seconds: i64) -> Result<(), WindowsError> {
        RealWin::schedule_reboot(self, at_unix_seconds)
    }

    fn scheduled_reboot(&self) -> Result<Option<i64>, WindowsError> {
        RealWin::scheduled_reboot(self)
    }

    fn list_firewall_rules(&self) -> Result<Vec<FirewallRule>, WindowsError> {
        RealWin::list_firewall_rules(self)
    }

    fn add_firewall_rule(&self, rule: &FirewallRule) -> Result<(), WindowsError> {
        RealWin::add_firewall_rule(self, rule)
    }

    fn remove_firewall_rule(&self, name: &str) -> Result<(), WindowsError> {
        RealWin::remove_firewall_rule(self, name)
    }

    fn set_profile_enabled(&self, profile: Profile, enabled: bool) -> Result<(), WindowsError> {
        RealWin::set_profile_enabled(self, profile, enabled)
    }

    fn profile_enabled(&self, profile: Profile) -> Result<bool, WindowsError> {
        RealWin::profile_enabled(self, profile)
    }

    fn list_local_users(&self) -> Result<Vec<LocalUser>, WindowsError> {
        RealWin::list_local_users(self)
    }

    fn local_user(&self, name: &str) -> Result<LocalUser, WindowsError> {
        RealWin::local_user(self, name)
    }

    fn create_local_user(
        &self,
        name: &str,
        password: &Secret,
        full_name: &str,
        comment: &str,
    ) -> Result<(), WindowsError> {
        RealWin::create_local_user(self, name, password, full_name, comment)
    }

    fn set_user_enabled(&self, name: &str, enabled: bool) -> Result<(), WindowsError> {
        RealWin::set_user_enabled(self, name, enabled)
    }

    fn set_user_password(&self, name: &str, password: &Secret) -> Result<(), WindowsError> {
        RealWin::set_user_password(self, name, password)
    }

    fn set_user_groups(&self, name: &str, groups: &[String]) -> Result<(), WindowsError> {
        RealWin::set_user_groups(self, name, groups)
    }

    fn list_local_groups(&self) -> Result<Vec<LocalGroup>, WindowsError> {
        RealWin::list_local_groups(self)
    }

    fn add_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError> {
        RealWin::add_group_member(self, group, user)
    }

    fn remove_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError> {
        RealWin::remove_group_member(self, group, user)
    }
}

/// Keeps the `WinApi` import meaningful on every path of the crate.
#[allow(dead_code)]
type SharedApi = std::sync::Arc<dyn WinApi>;
