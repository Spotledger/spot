//! The Service Control Manager binding for [`RealWin`](super::RealWin).

use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    ENUM_SERVICE_STATUS_PROCESSW, ENUM_SERVICE_TYPE, EnumServicesStatusExW, OpenSCManagerW,
    OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceStatusEx, SC_ACTION,
    SC_ACTION_NONE, SC_ACTION_REBOOT, SC_ACTION_RESTART, SC_ENUM_PROCESS_INFO, SC_MANAGER_CONNECT,
    SC_MANAGER_ENUMERATE_SERVICE, SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_BOOT_START, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_DISABLED, SERVICE_ERROR,
    SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_START_TYPE, SERVICE_STATE_ALL, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_SYSTEM_START, StartServiceW,
};
use windows::core::PCWSTR;

use super::{RealWin, api, name_to_string, pcwstr, pwstr_to_string, wide};
use crate::api::{RecoveryAction, RecoveryConfig, ServiceInfo, ServiceState, StartType, WinApi};
use crate::error::WindowsError;

impl WinApi for RealWin {
    fn list_services(&self) -> Result<Vec<ServiceInfo>, WindowsError> {
        // SAFETY: the SCM handle is closed before returning on every path; the enumeration
        // buffer is a plain byte slice the API fills.
        unsafe {
            let scm = api(OpenSCManagerW(
                PCWSTR::null(),
                PCWSTR::null(),
                SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
            ))?;
            let mut needed = 0u32;
            let mut returned = 0u32;
            let mut resume = 0u32;
            let _ = EnumServicesStatusExW(
                scm,
                SC_ENUM_PROCESS_INFO,
                ENUM_SERVICE_TYPE(0x30), // WIN32 + DRIVER
                SERVICE_STATE_ALL,
                None,
                &mut needed,
                &mut returned,
                Some(&mut resume),
                PCWSTR::null(),
            );
            let mut buffer = vec![0u8; needed as usize];
            let status = EnumServicesStatusExW(
                scm,
                SC_ENUM_PROCESS_INFO,
                ENUM_SERVICE_TYPE(0x30),
                SERVICE_STATE_ALL,
                Some(&mut buffer),
                &mut needed,
                &mut returned,
                Some(&mut resume),
                PCWSTR::null(),
            );
            let _ = CloseServiceHandle(scm);
            api(status)?;
            let mut services = Vec::with_capacity(returned as usize);
            let entry = std::mem::size_of::<ENUM_SERVICE_STATUS_PROCESSW>();
            for index in 0..returned as usize {
                let start = index * entry;
                let record: ENUM_SERVICE_STATUS_PROCESSW =
                    std::ptr::read_unaligned(buffer.as_ptr().add(start).cast());
                services.push(ServiceInfo {
                    name: pwstr_to_string(record.lpServiceName),
                    display_name: pwstr_to_string(record.lpDisplayName),
                    state: service_state(record.ServiceStatusProcess.dwCurrentState),
                    start_type: StartType::Manual,
                });
            }
            Ok(services)
        }
    }

    fn service_status(&self, name: &str) -> Result<ServiceInfo, WindowsError> {
        let name = wide(name);
        // SAFETY: both handles are closed before returning.
        unsafe {
            let scm = api(OpenSCManagerW(
                PCWSTR::null(),
                PCWSTR::null(),
                SC_MANAGER_CONNECT,
            ))?;
            let service = match OpenServiceW(scm, pcwstr(&name), SERVICE_QUERY_STATUS) {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = CloseServiceHandle(scm);
                    return Err(map_service_open(name_to_string(&name), error));
                }
            };
            let mut status = SERVICE_STATUS_PROCESS::default();
            let mut needed = 0u32;
            let result = QueryServiceStatusEx(
                service,
                SC_STATUS_PROCESS_INFO,
                Some(std::slice::from_raw_parts_mut(
                    (&raw mut status).cast::<u8>(),
                    std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
                )),
                &mut needed,
            );
            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
            api(result)?;
            let service_name = name_to_string(&name);
            let start_type = read_start_type(&service_name)?;
            Ok(ServiceInfo {
                name: service_name.clone(),
                display_name: service_name,
                state: service_state(status.dwCurrentState),
                start_type,
            })
        }
    }

    fn start_service(&self, name: &str, timeout_seconds: u64) -> Result<(), WindowsError> {
        self.control_service(name, SERVICE_START, SERVICE_RUNNING, timeout_seconds)
    }

    fn stop_service(&self, name: &str, timeout_seconds: u64) -> Result<(), WindowsError> {
        self.control_service(name, SERVICE_CONTROL_STOP, SERVICE_STOPPED, timeout_seconds)
    }

    fn set_service_start_type(
        &self,
        name: &str,
        start_type: StartType,
    ) -> Result<(), WindowsError> {
        let name = wide(name);
        let start = start_type_code(start_type);
        // SAFETY: both handles are closed before returning.
        unsafe {
            let scm = api(OpenSCManagerW(
                PCWSTR::null(),
                PCWSTR::null(),
                SC_MANAGER_CONNECT,
            ))?;
            let service = match OpenServiceW(scm, pcwstr(&name), SERVICE_CHANGE_CONFIG) {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = CloseServiceHandle(scm);
                    return Err(map_service_open(name_to_string(&name), error));
                }
            };
            let result = ChangeServiceConfigW(
                service,
                ENUM_SERVICE_TYPE(0xFFFFFFFF),
                start,
                SERVICE_ERROR(0xFFFFFFFF),
                PCWSTR::null(),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            );
            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
            api(result)
        }
    }

    fn service_recovery(&self, name: &str) -> Result<RecoveryConfig, WindowsError> {
        let name = wide(name);
        // SAFETY: both handles are closed before returning; the query buffer is owned here.
        unsafe {
            let scm = api(OpenSCManagerW(
                PCWSTR::null(),
                PCWSTR::null(),
                SC_MANAGER_CONNECT,
            ))?;
            let service = match OpenServiceW(scm, pcwstr(&name), SERVICE_QUERY_STATUS) {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = CloseServiceHandle(scm);
                    return Err(map_service_open(name_to_string(&name), error));
                }
            };
            let mut needed = 0u32;
            let _ =
                QueryServiceConfig2W(service, SERVICE_CONFIG_FAILURE_ACTIONS, None, &mut needed);
            let mut buffer = vec![0u8; needed as usize];
            let result = QueryServiceConfig2W(
                service,
                SERVICE_CONFIG_FAILURE_ACTIONS,
                Some(&mut buffer),
                &mut needed,
            );
            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
            api(result)?;
            let actions_struct: SERVICE_FAILURE_ACTIONSW =
                std::ptr::read_unaligned(buffer.as_ptr().cast());
            let mut actions = Vec::new();
            for index in 0..actions_struct.cActions as usize {
                let action = std::ptr::read_unaligned(actions_struct.lpsaActions.add(index));
                actions.push(recovery_action(action.Type));
            }
            while actions.len() < 3 {
                actions.push(RecoveryAction::None);
            }
            Ok(RecoveryConfig {
                actions,
                restart_delay_seconds: first_delay(actions_struct),
                reset_period_seconds: actions_struct.dwResetPeriod,
            })
        }
    }

    fn set_service_recovery(
        &self,
        name: &str,
        recovery: &RecoveryConfig,
    ) -> Result<(), WindowsError> {
        let name = wide(name);
        let mut actions: Vec<SC_ACTION> = recovery
            .actions
            .iter()
            .map(|action| SC_ACTION {
                Type: recovery_code(*action),
                Delay: recovery.restart_delay_seconds,
            })
            .collect();
        // SAFETY: both handles are closed; the failure-actions struct borrows `actions`
        // for the duration of the call only.
        unsafe {
            let scm = api(OpenSCManagerW(
                PCWSTR::null(),
                PCWSTR::null(),
                SC_MANAGER_CONNECT,
            ))?;
            let service = match OpenServiceW(scm, pcwstr(&name), SERVICE_CHANGE_CONFIG) {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = CloseServiceHandle(scm);
                    return Err(map_service_open(name_to_string(&name), error));
                }
            };
            let failure = SERVICE_FAILURE_ACTIONSW {
                dwResetPeriod: recovery.reset_period_seconds,
                lpRebootMsg: windows::core::PWSTR::null(),
                lpCommand: windows::core::PWSTR::null(),
                cActions: actions.len() as u32,
                lpsaActions: actions.as_mut_ptr(),
            };
            let result = ChangeServiceConfig2W(
                service,
                SERVICE_CONFIG_FAILURE_ACTIONS,
                Some((&raw const failure).cast()),
            );
            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
            api(result)
        }
    }
}

impl RealWin {
    /// Starts, stops or otherwise controls a service and waits for the wanted state.
    fn control_service(
        &self,
        name: &str,
        control: SERVICE_CONTROL,
        wanted: SERVICE_STATUS_CURRENT_STATE,
        timeout_seconds: u64,
    ) -> Result<(), WindowsError> {
        let name_w = wide(name);
        let start = std::time::Instant::now();
        // SAFETY: the service handle is closed on every path.
        unsafe {
            let scm = api(OpenSCManagerW(
                PCWSTR::null(),
                PCWSTR::null(),
                SC_MANAGER_CONNECT,
            ))?;
            let access = if control == SERVICE_CONTROL_STOP {
                SERVICE_STOP
            } else {
                SERVICE_START
            };
            let service = match OpenServiceW(scm, pcwstr(&name_w), access) {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = CloseServiceHandle(scm);
                    return Err(map_service_open(name.to_owned(), error));
                }
            };
            let issued = if control == SERVICE_CONTROL_STOP {
                let mut status = SERVICE_STATUS::default();
                ControlService(service, control, &mut status)
            } else {
                StartServiceW(service, None)
            };
            let result = api(issued);
            if result.is_ok() {
                loop {
                    let mut status = SERVICE_STATUS_PROCESS::default();
                    let mut needed = 0u32;
                    let _ = QueryServiceStatusEx(
                        service,
                        SC_STATUS_PROCESS_INFO,
                        Some(std::slice::from_raw_parts_mut(
                            (&raw mut status).cast::<u8>(),
                            std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
                        )),
                        &mut needed,
                    );
                    if status.dwCurrentState == wanted {
                        break;
                    }
                    if start.elapsed().as_secs() >= timeout_seconds {
                        let _ = CloseServiceHandle(service);
                        let _ = CloseServiceHandle(scm);
                        return Err(WindowsError::ServiceTimeout {
                            name: name.to_owned(),
                            timeout_seconds,
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
            result
        }
    }
}

/// Reads the configured start type of a service.
fn read_start_type(name: &str) -> Result<StartType, WindowsError> {
    // SAFETY: both handles are closed before returning.
    unsafe {
        let name_w = wide(name);
        let scm = api(OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT,
        ))?;
        let service = match OpenServiceW(scm, pcwstr(&name_w), SERVICE_QUERY_STATUS) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = CloseServiceHandle(scm);
                return Err(map_service_open(name.to_owned(), error));
            }
        };
        let mut needed = 0u32;
        let _ =
            windows::Win32::System::Services::QueryServiceConfigW(service, None, 0, &mut needed);
        let mut buffer = vec![0u8; needed as usize];
        let result = windows::Win32::System::Services::QueryServiceConfigW(
            service,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        );
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
        api(result)?;
        let config: QUERY_SERVICE_CONFIGW = std::ptr::read_unaligned(buffer.as_ptr().cast());
        Ok(start_type_of(config.dwStartType))
    }
}

/// The [`StartType`] for a `SERVICE_START_TYPE` code.
fn start_type_of(code: SERVICE_START_TYPE) -> StartType {
    match code {
        SERVICE_BOOT_START => StartType::Boot,
        SERVICE_SYSTEM_START => StartType::System,
        SERVICE_AUTO_START => StartType::Automatic,
        SERVICE_DISABLED => StartType::Disabled,
        _ => StartType::Manual,
    }
}

/// The `SERVICE_START_TYPE` code for a [`StartType`].
fn start_type_code(start: StartType) -> SERVICE_START_TYPE {
    match start {
        StartType::Boot => SERVICE_BOOT_START,
        StartType::System => SERVICE_SYSTEM_START,
        StartType::Automatic | StartType::AutomaticDelayed => SERVICE_AUTO_START,
        StartType::Manual => SERVICE_DEMAND_START,
        StartType::Disabled => SERVICE_DISABLED,
    }
}

/// The [`ServiceState`] for a service status code.
fn service_state(code: SERVICE_STATUS_CURRENT_STATE) -> ServiceState {
    match code.0 {
        1 => ServiceState::Stopped,
        2 => ServiceState::StartPending,
        3 => ServiceState::StopPending,
        4 => ServiceState::Running,
        5 => ServiceState::ContinuePending,
        6 => ServiceState::PausePending,
        7 => ServiceState::Paused,
        _ => ServiceState::Unknown,
    }
}

/// The [`RecoveryAction`] for an `SC_ACTION_TYPE`.
fn recovery_action(code: windows::Win32::System::Services::SC_ACTION_TYPE) -> RecoveryAction {
    match code {
        SC_ACTION_RESTART => RecoveryAction::Restart,
        SC_ACTION_REBOOT => RecoveryAction::Reboot,
        SC_ACTION_NONE => RecoveryAction::None,
        _ => RecoveryAction::None,
    }
}

/// The `SC_ACTION_TYPE` for a [`RecoveryAction`].
fn recovery_code(action: RecoveryAction) -> windows::Win32::System::Services::SC_ACTION_TYPE {
    match action {
        RecoveryAction::Restart => SC_ACTION_RESTART,
        RecoveryAction::Reboot => SC_ACTION_REBOOT,
        RecoveryAction::None => SC_ACTION_NONE,
    }
}

/// The first action's delay from a failure-actions struct.
fn first_delay(actions: SERVICE_FAILURE_ACTIONSW) -> u32 {
    if actions.cActions == 0 {
        return 0;
    }
    // SAFETY: `lpsaActions` points at `cActions` entries, checked non-zero above.
    unsafe { std::ptr::read_unaligned(actions.lpsaActions).Delay }
}

/// Maps a failed `OpenServiceW` into [`WindowsError`]: a missing service is
/// [`WindowsError::UnknownService`], anything else an API failure.
fn map_service_open(name: String, error: windows::core::Error) -> WindowsError {
    // ERROR_SERVICE_DOES_NOT_EXIST is 1060.
    if error.code().0 as u32 == 0x8007_0424 {
        WindowsError::UnknownService { name }
    } else {
        WindowsError::Api {
            detail: format!(
                "OpenService failed with HRESULT {:#010x}",
                error.code().0 as u32
            ),
        }
    }
}

/// Keeps the unused-import warning away from `SERVICE_ALL_ACCESS`, which documents the
/// full access mask this binding would request if it needed one.
#[allow(dead_code)]
const _FULL_ACCESS: u32 = SERVICE_ALL_ACCESS;
