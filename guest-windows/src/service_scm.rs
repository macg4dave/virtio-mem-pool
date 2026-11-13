use crate::native::to_wide;

use winapi::ctypes::c_void;
use winapi::shared::minwindef::{DWORD, FALSE, TRUE};
use winapi::um::winnt::SERVICE_ERROR_NORMAL;
use winapi::um::winsvc::{
    ChangeServiceConfig2W, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    StartServiceW, SC_ACTION, SC_ACTION_RESTART, SC_MANAGER_CREATE_SERVICE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_FAILURE_ACTIONSW,
    SERVICE_FAILURE_ACTIONS_FLAG, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOP, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW,
};

const SERVICE_ALL_ACCESS: DWORD = 0xF01FF;
const SERVICE_AUTO_START: DWORD = 0x00000002;
const SERVICE_WIN32_OWN_PROCESS: DWORD = 0x00000010;
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;
const SERVICE_DELETE: DWORD = 0x00010000;
const SERVICE_START: DWORD = 0x00000010;
const MAX_SERVICE_NAME_UTF16_UNITS: usize = 256;

use crate::config::ServiceConfig;
use crate::event_log::{
    ServiceEvent, ServiceEventId, ServiceEventLevel, ServiceEventSink, WindowsEventLog,
};
use crate::runtime::native_worker;
use crate::service_host::{ServiceHost, ServiceState, StopSignal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScmServiceStatus {
    pub current_state: DWORD,
    pub controls_accepted: DWORD,
    pub exit_code: DWORD,
}

impl ScmServiceStatus {
    pub fn from_state(state: ServiceState) -> Self {
        let (current_state, controls_accepted, exit_code) = match state {
            ServiceState::Created | ServiceState::StartPending => (SERVICE_START_PENDING, 0, 0),
            ServiceState::Running => (
                SERVICE_RUNNING,
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
                0,
            ),
            ServiceState::StopPending => (SERVICE_STOP_PENDING, 0, 0),
            ServiceState::Stopped => (SERVICE_STOPPED, 0, 0),
            ServiceState::Failed => (SERVICE_STOPPED, 0, 1),
        };

        Self {
            current_state,
            controls_accepted,
            exit_code,
        }
    }
}

// SCM may have entered a handler when the hosting thread reports Stopped.
// Keep its sole cancellation context valid for the process lifetime; callbacks
// never retain a status handle or publish status. This is an own-process service.
static SCM_STOP: std::sync::OnceLock<StopSignal> = std::sync::OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsServiceRegistration {
    pub service_name: String,
    pub display_name: String,
    pub description: String,
    pub executable_path: String,
    pub service_account: String,
    pub startup_type: DWORD,
    pub error_control: DWORD,
}

impl WindowsServiceRegistration {
    /// `config` comes from `ServiceConfig::load_default`, which validated it.
    pub fn from_config(config: &ServiceConfig) -> Result<Self, String> {
        let executable_path = std::env::current_exe()
            .and_then(|path| {
                path.into_os_string()
                    .into_string()
                    .map_err(|_| std::io::Error::other("executable path is not Unicode"))
            })
            .map_err(|error| format!("resolve executable path: {error}"))?;

        Ok(Self {
            service_name: config.service_name.clone(),
            display_name: config.display_name.clone(),
            description: config.description.clone(),
            executable_path,
            service_account: config.service_account.clone(),
            startup_type: SERVICE_AUTO_START,
            error_control: SERVICE_ERROR_NORMAL,
        })
    }
}

struct ServiceHandle(winapi::um::winsvc::SC_HANDLE);
impl ServiceHandle {
    fn checked(raw: winapi::um::winsvc::SC_HANDLE, operation: &str) -> Result<Self, String> {
        if raw.is_null() {
            Err(format!("{operation}: {}", std::io::Error::last_os_error()))
        } else {
            Ok(Self(raw))
        }
    }
}
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::winsvc::CloseServiceHandle(self.0);
        }
    }
}

pub fn install_service(config: &ServiceConfig) -> Result<(), String> {
    let registration = WindowsServiceRegistration::from_config(config)?;
    provision_program_data_acl(config)?;
    let manager_name = to_wide("")?;
    let manager = unsafe {
        OpenSCManagerW(
            manager_name.as_ptr(),
            std::ptr::null(),
            SC_MANAGER_CREATE_SERVICE,
        )
    };
    let manager = ServiceHandle::checked(manager, "OpenSCManagerW")?;

    let service_name = to_wide(&registration.service_name)?;
    let display_name = to_wide(&registration.display_name)?;
    let executable = to_wide(&format!("\"{}\"", registration.executable_path))?;
    let account = to_wide(&registration.service_account)?;
    let service = unsafe {
        CreateServiceW(
            manager.0,
            service_name.as_ptr(),
            display_name.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            registration.startup_type,
            registration.error_control,
            executable.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            account.as_ptr(),
            std::ptr::null(),
        )
    };

    let service = ServiceHandle::checked(service, "CreateServiceW")?;
    let setup = (|| -> Result<(), String> {
        unsafe {
            let description = to_wide(&registration.description)?;
            let mut description_config = SERVICE_DESCRIPTIONW {
                lpDescription: description.as_ptr() as *mut u16,
            };
            if ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_DESCRIPTION,
                (&mut description_config as *mut SERVICE_DESCRIPTIONW).cast(),
            ) == FALSE
            {
                let error = std::io::Error::last_os_error();
                return Err(format!("ChangeServiceConfig2W description failed: {error}"));
            }

            let mut actions = [
                SC_ACTION {
                    Type: SC_ACTION_RESTART,
                    Delay: 5_000,
                },
                SC_ACTION {
                    Type: SC_ACTION_RESTART,
                    Delay: 30_000,
                },
                SC_ACTION {
                    Type: SC_ACTION_RESTART,
                    Delay: 60_000,
                },
            ];
            let mut failure_actions = SERVICE_FAILURE_ACTIONSW {
                dwResetPeriod: 86_400,
                lpRebootMsg: std::ptr::null_mut(),
                lpCommand: std::ptr::null_mut(),
                cActions: actions.len() as DWORD,
                lpsaActions: actions.as_mut_ptr(),
            };
            if ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_FAILURE_ACTIONS,
                (&mut failure_actions as *mut SERVICE_FAILURE_ACTIONSW).cast(),
            ) == FALSE
            {
                let error = std::io::Error::last_os_error();
                return Err(format!("ChangeServiceConfig2W recovery failed: {error}"));
            }

            let mut failure_flag = SERVICE_FAILURE_ACTIONS_FLAG {
                fFailureActionsOnNonCrashFailures: TRUE,
            };
            if ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
                (&mut failure_flag as *mut SERVICE_FAILURE_ACTIONS_FLAG).cast(),
            ) == FALSE
            {
                let error = std::io::Error::last_os_error();
                return Err(format!(
                    "ChangeServiceConfig2W failure flag failed: {error}"
                ));
            }
        }

        Ok(())
    })();
    if let Err(error) = setup {
        if unsafe { DeleteService(service.0) } == FALSE {
            return Err(format!("{error}; cleanup DeleteService failed: {}; remove the partially created service before retrying installation", std::io::Error::last_os_error()));
        }
        return Err(error);
    }
    Ok(())
}

fn provision_program_data_acl(config: &ServiceConfig) -> Result<(), String> {
    if !config
        .service_account
        .eq_ignore_ascii_case(r"NT AUTHORITY\LocalService")
    {
        return Err(
            "automatic ACL provisioning supports only NT AUTHORITY\\LocalService".to_owned(),
        );
    }
    let report = std::path::Path::new(&config.demand_report_path);
    let config_path = std::path::Path::new(&config.config_path);
    let root = config_path
        .parent()
        .ok_or("configuration path has no parent")?;
    let report_parent = report.parent().ok_or("telemetry path has no parent")?;
    if virtio_mem_core::windows_service_config::normalize_windows_path(
        root.to_str()
            .ok_or("configuration directory is not Unicode")?,
    )? != virtio_mem_core::windows_service_config::normalize_windows_path(
        report_parent
            .to_str()
            .ok_or("telemetry directory is not Unicode")?,
    )? {
        return Err("configuration and telemetry files must share one ACL root".to_owned());
    }
    std::fs::create_dir_all(root)
        .map_err(|error| format!("create service data directory: {error}"))?;
    // The directory allows creation/traversal but neither DELETE nor
    // FILE_DELETE_CHILD. New publication files (telemetry, backups, request
    // state and the host status QGA writes as LocalSystem) inherit modify
    // rights; the protected configuration DACL grants LocalService only read.
    apply_acl(
        root,
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1201bf;;;LS)(A;OIIO;0x1301bf;;;LS)",
    )?;
    apply_acl(config_path, "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;LS)")?;
    // SetFileSecurity does not update existing children. Set their protected
    // DACLs explicitly: every file named by the publication path or derived
    // from it as `<path>.<suffix>`, including unpublished handoff files.
    let prefix = report
        .file_name()
        .ok_or("telemetry path has no filename")?
        .to_string_lossy()
        .to_lowercase();
    for entry in std::fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name == prefix || name.starts_with(&format!("{prefix}.")) {
            crate::config::validate_file_identity(
                &config.config_path,
                entry
                    .path()
                    .to_str()
                    .ok_or("publication path is not Unicode")?,
            )
            .map_err(|error| error.to_string())?;
            apply_acl(
                &entry.path(),
                "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1301bf;;;LS)",
            )?;
        }
    }
    Ok(())
}

fn apply_acl(path: &std::path::Path, sddl: &str) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    use winapi::shared::minwindef::HLOCAL;
    use winapi::shared::sddl::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use winapi::um::securitybaseapi::SetFileSecurityW;
    use winapi::um::winbase::LocalFree;
    use winapi::um::winnt::{
        DACL_SECURITY_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    if std::fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .file_attributes()
        & FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        return Err(format!(
            "refusing ACL provisioning on reparse point {}",
            path.display()
        ));
    }
    let sddl = to_wide(sddl)?;
    let path = to_wide(path.to_str().ok_or("ACL path is not Unicode")?)?;
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1.into(),
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == FALSE
    {
        return Err(format!(
            "convert security descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    let result = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    // Capture the API error before LocalFree can change the thread error code.
    let error = (result == FALSE).then(std::io::Error::last_os_error);
    unsafe {
        LocalFree(descriptor as HLOCAL);
    }
    match error {
        Some(error) => Err(format!("apply service ACL: {error}")),
        None => Ok(()),
    }
}

pub fn run_as_service() -> Result<bool, String> {
    // This is an own-process service, so SCM ignores the table name and passes
    // the installed identity to `service_main`. Keeping the table entry empty
    // avoids coupling dispatcher attachment to the configurable install name.
    let service_name = to_wide("")?;
    let mut table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: service_name.as_ptr() as *mut u16,
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];

    let started = unsafe { StartServiceCtrlDispatcherW(table.as_mut_ptr()) };
    if started != 0 {
        return Ok(true);
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) {
        Ok(false)
    } else {
        Err(format!("StartServiceCtrlDispatcherW failed: {error}"))
    }
}

unsafe extern "system" fn service_main(argc: DWORD, argv: *mut *mut u16) {
    let event_source = match service_name_from_main_args(argc, argv) {
        Ok(service_name) => service_name,
        Err(error) => {
            eprintln!("resolve SCM service identity failed: {error}");
            return;
        }
    };
    let callback_stop = SCM_STOP.get_or_init(StopSignal::new);
    let stop = callback_stop.clone();
    let Ok(service_name) = to_wide(&event_source) else {
        return;
    };
    let status_handle = RegisterServiceCtrlHandlerExW(
        service_name.as_ptr(),
        Some(service_control_handler),
        callback_stop as *const StopSignal as *mut _,
    );
    if status_handle.is_null() {
        emit_event(
            &event_source,
            ServiceEvent::new(
                ServiceEventId::StatusPublishFailed,
                ServiceEventLevel::Error,
                format!(
                    "RegisterServiceCtrlHandlerExW failed: {}",
                    std::io::Error::last_os_error()
                ),
            ),
        );
        return;
    }

    if let Err(error) = publish_status(
        status_handle,
        ScmServiceStatus::from_state(ServiceState::StartPending),
    ) {
        emit_event(
            &event_source,
            ServiceEvent::new(
                ServiceEventId::StatusPublishFailed,
                ServiceEventLevel::Error,
                error,
            ),
        );
        return;
    }
    emit_event(
        &event_source,
        ServiceEvent::new(
            ServiceEventId::StartPending,
            ServiceEventLevel::Information,
            format!("service initialization started (build {})", crate::BUILD_ID),
        ),
    );
    let config = match ServiceConfig::load_default() {
        Ok(config) => config,
        Err(error) => {
            emit_event(
                &event_source,
                ServiceEvent::new(
                    ServiceEventId::ConfigurationFailed,
                    ServiceEventLevel::Error,
                    format!("service configuration failed: {error}"),
                ),
            );
            let _ = publish_status(
                status_handle,
                ScmServiceStatus::from_state(ServiceState::Failed),
            );
            return;
        }
    };
    if let Err(error) = validate_configured_service_identity(&config.service_name, &event_source) {
        emit_event(
            &event_source,
            ServiceEvent::new(
                ServiceEventId::ConfigurationFailed,
                ServiceEventLevel::Error,
                error,
            ),
        );
        let _ = publish_status(
            status_handle,
            ScmServiceStatus::from_state(ServiceState::Failed),
        );
        return;
    }
    let worker = match native_worker(&config) {
        Ok(worker) => worker,
        Err(error) => {
            emit_event(
                &event_source,
                ServiceEvent::new(
                    ServiceEventId::WorkerFailed,
                    ServiceEventLevel::Error,
                    error.to_string(),
                ),
            );
            let _ = publish_status(
                status_handle,
                ScmServiceStatus::from_state(ServiceState::Failed),
            );
            return;
        }
    };
    let stop_observer = stop.clone();
    let mut host =
        ServiceHost::with_stop_and_shutdown_timeout(worker, stop, config.shutdown_timeout());
    let result = host
        .run_with_status(|state| {
            publish_status(status_handle, ScmServiceStatus::from_state(state))?;
            if state == ServiceState::Running {
                emit_event(
                    &event_source,
                    ServiceEvent::new(
                        ServiceEventId::Running,
                        ServiceEventLevel::Information,
                        "service worker is running",
                    ),
                );
            }
            Ok(())
        })
        .map_err(|error| format!("runtime wiring / service host execution: {error}"));
    let stop_requested = stop_observer.is_cancelled();
    if stop_requested {
        emit_event(
            &event_source,
            ServiceEvent::new(
                ServiceEventId::StopRequested,
                ServiceEventLevel::Information,
                "service cancellation was requested",
            ),
        );
    }
    let (final_state, event) = classify_service_exit(result, stop_requested);
    emit_event(&event_source, event);
    if let Err(error) = publish_status(status_handle, ScmServiceStatus::from_state(final_state)) {
        emit_event(
            &event_source,
            ServiceEvent::new(
                ServiceEventId::StatusPublishFailed,
                ServiceEventLevel::Error,
                error,
            ),
        );
    }
}

fn validate_configured_service_identity(configured: &str, scm: &str) -> Result<(), String> {
    if configured == scm {
        Ok(())
    } else {
        Err(format!(
            "configured service name {configured} does not match SCM identity {scm}"
        ))
    }
}

unsafe fn service_name_from_main_args(argc: DWORD, argv: *mut *mut u16) -> Result<String, String> {
    if argc == 0 || argv.is_null() {
        return Err("SCM did not provide the service name argument".to_owned());
    }
    let service_name = *argv;
    if service_name.is_null() {
        return Err("SCM provided a null service name argument".to_owned());
    }
    let mut length = 0;
    while length <= MAX_SERVICE_NAME_UTF16_UNITS {
        if *service_name.add(length) == 0 {
            if length == 0 {
                return Err("SCM provided an empty service name argument".to_owned());
            }
            return String::from_utf16(std::slice::from_raw_parts(service_name, length))
                .map_err(|error| format!("SCM service name is invalid UTF-16: {error}"));
        }
        length += 1;
    }
    Err(format!(
        "SCM service name exceeds {MAX_SERVICE_NAME_UTF16_UNITS} UTF-16 units"
    ))
}

unsafe extern "system" fn service_control_handler(
    control: DWORD,
    _event_type: DWORD,
    _event_data: *mut c_void,
    context: *mut c_void,
) -> DWORD {
    if context.is_null() {
        return 1;
    }
    let stop = &*(context as *const StopSignal);
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        stop.cancel();
        0
    } else {
        1
    }
}

fn classify_service_exit(
    result: Result<(), String>,
    stop_requested: bool,
) -> (ServiceState, ServiceEvent) {
    match (result, stop_requested) {
        (Ok(()), true) => (
            ServiceState::Stopped,
            ServiceEvent::new(
                ServiceEventId::Stopped,
                ServiceEventLevel::Information,
                "service stopped after cancellation",
            ),
        ),
        (Ok(()), false) => (
            ServiceState::Failed,
            ServiceEvent::new(
                ServiceEventId::UnexpectedExit,
                ServiceEventLevel::Error,
                "service worker exited without a stop request",
            ),
        ),
        (Err(error), _) => (
            ServiceState::Failed,
            ServiceEvent::new(
                ServiceEventId::WorkerFailed,
                ServiceEventLevel::Error,
                error,
            ),
        ),
    }
}

fn emit_event(source: &str, event: ServiceEvent) {
    let result = WindowsEventLog::open(source).and_then(|event_log| event_log.emit(&event));
    if let Err(error) = result {
        eprintln!(
            "Windows Event Log publication failed for event {}: {error}",
            event.id as u32
        );
    }
}

fn publish_status(handle: SERVICE_STATUS_HANDLE, status: ScmServiceStatus) -> Result<(), String> {
    let mut native = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: status.current_state,
        dwControlsAccepted: status.controls_accepted,
        dwWin32ExitCode: status.exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    let published = unsafe { SetServiceStatus(handle, &mut native) };
    if published == 0 {
        Err(format!(
            "SetServiceStatus failed: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

fn open_service(service_name: &str, access: DWORD) -> Result<ServiceHandle, String> {
    if service_name.is_empty() || service_name.contains(['/', '\\']) {
        return Err("invalid service name".to_owned());
    }
    let name = to_wide(service_name)?;
    let manager = ServiceHandle::checked(
        unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), 0) },
        "OpenSCManagerW",
    )?;
    ServiceHandle::checked(
        unsafe { OpenServiceW(manager.0, name.as_ptr(), access) },
        "OpenServiceW",
    )
}

pub fn stop_service(service_name: &str) -> Result<(), String> {
    let service = open_service(service_name, SERVICE_STOP)?;
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == FALSE {
        return Err(format!(
            "ControlService for {service_name}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

pub fn start_service(service_name: &str) -> Result<(), String> {
    let service = open_service(service_name, SERVICE_START)?;
    if unsafe { StartServiceW(service.0, 0, std::ptr::null_mut()) } == FALSE {
        return Err(format!(
            "StartServiceW for {service_name}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

pub fn remove_service(service_name: &str) -> Result<(), String> {
    let service = open_service(service_name, SERVICE_DELETE)?;
    if unsafe { DeleteService(service.0) } == FALSE {
        return Err(format!(
            "DeleteService for {service_name}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winapi::um::winsvc::{
        SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
        SERVICE_STOPPED, SERVICE_STOP_PENDING,
    };

    #[test]
    fn maps_service_state_to_windows_status_codes() {
        assert_eq!(
            ScmServiceStatus::from_state(ServiceState::StartPending).current_state,
            SERVICE_START_PENDING as DWORD
        );
        assert_eq!(
            ScmServiceStatus::from_state(ServiceState::Running).current_state,
            SERVICE_RUNNING as DWORD
        );
        assert_eq!(
            ScmServiceStatus::from_state(ServiceState::StopPending).current_state,
            SERVICE_STOP_PENDING as DWORD
        );
        assert_eq!(
            ScmServiceStatus::from_state(ServiceState::Stopped).current_state,
            SERVICE_STOPPED as DWORD
        );
    }

    #[test]
    fn registration_uses_normal_startup_error_control() {
        let registration = WindowsServiceRegistration::from_config(&ServiceConfig {
            config_path: r"C:\test\config.json".to_owned(),
            ..crate::config::test_config()
        })
        .expect("registration should be valid");

        assert_eq!(registration.error_control, 1);
    }

    #[test]
    fn service_main_uses_the_scm_supplied_service_identity() {
        let mut encoded = to_wide("ConfiguredVirtioMemService").unwrap();
        let mut argument = encoded.as_mut_ptr();

        let service_name = unsafe { service_name_from_main_args(1, &mut argument) };

        assert_eq!(service_name.as_deref(), Ok("ConfiguredVirtioMemService"));
        assert_eq!(
            validate_configured_service_identity(
                "ConfiguredVirtioMemService",
                service_name.as_deref().expect("service identity")
            ),
            Ok(())
        );
        assert!(validate_configured_service_identity(
            "AnotherService",
            service_name.as_deref().expect("service identity")
        )
        .is_err());
    }

    #[test]
    fn service_main_rejects_missing_or_unbounded_service_identity() {
        assert!(unsafe { service_name_from_main_args(0, std::ptr::null_mut()) }.is_err());

        let mut encoded = vec![u16::from(b'a'); MAX_SERVICE_NAME_UTF16_UNITS + 1];
        let mut argument = encoded.as_mut_ptr();
        assert!(unsafe { service_name_from_main_args(1, &mut argument) }.is_err());
    }

    #[test]
    fn running_service_accepts_stop_and_shutdown_controls() {
        assert_eq!(
            ScmServiceStatus::from_state(ServiceState::Running).controls_accepted,
            (SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN) as DWORD
        );
    }

    #[test]
    fn marks_failed_workers_with_non_zero_exit_code() {
        let status = ScmServiceStatus::from_state(ServiceState::Failed);

        assert_eq!(status.current_state, SERVICE_STOPPED as DWORD);
        assert_eq!(status.exit_code, 1);
    }

    #[test]
    fn intentional_stop_is_successful_and_unexpected_exit_is_recoverable() {
        let (state, event) = classify_service_exit(Ok(()), true);
        assert_eq!(state, ServiceState::Stopped);
        assert_eq!(event.id, ServiceEventId::Stopped);
        assert_eq!(event.level, ServiceEventLevel::Information);

        let (state, event) = classify_service_exit(Ok(()), false);
        assert_eq!(state, ServiceState::Failed);
        assert_eq!(event.id, ServiceEventId::UnexpectedExit);
        assert_eq!(event.level, ServiceEventLevel::Error);
        assert_eq!(ScmServiceStatus::from_state(state).exit_code, 1);
    }

    #[test]
    fn worker_failure_is_recoverable_even_after_stop_was_requested() {
        let (state, event) = classify_service_exit(Err("poll failed".to_owned()), true);

        assert_eq!(state, ServiceState::Failed);
        assert_eq!(event.id, ServiceEventId::WorkerFailed);
        assert_eq!(event.message, "poll failed");
        assert_eq!(ScmServiceStatus::from_state(state).exit_code, 1);
    }
}
