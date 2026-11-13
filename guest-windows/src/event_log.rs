//! Bounded Windows Application Event Log records for SCM lifecycle outcomes.

use crate::native::to_wide;

use winapi::shared::minwindef::WORD;
use winapi::um::winbase::{DeregisterEventSource, RegisterEventSourceW, ReportEventW};
use winapi::um::winnt::{
    EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE, HANDLE,
};

const MAX_EVENT_MESSAGE_CHARS: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// Stable event identifiers used by operational queries and alerts.
pub enum ServiceEventId {
    StartPending = 1_000,
    Running = 1_001,
    StopRequested = 1_002,
    Stopped = 1_003,
    ConfigurationFailed = 2_000,
    WorkerFailed = 2_001,
    UnexpectedExit = 2_002,
    StatusPublishFailed = 2_003,
    WakeUnavailable = 2_004,
    WakeConnected = 1_004,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Windows Event Log severity for a service event.
pub enum ServiceEventLevel {
    Information,
    Warning,
    Error,
}

impl ServiceEventLevel {
    fn event_type(self) -> WORD {
        match self {
            Self::Information => EVENTLOG_INFORMATION_TYPE,
            Self::Warning => EVENTLOG_WARNING_TYPE,
            Self::Error => EVENTLOG_ERROR_TYPE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A sanitized, bounded lifecycle event ready for publication.
pub struct ServiceEvent {
    pub id: ServiceEventId,
    pub level: ServiceEventLevel,
    pub message: String,
}

impl ServiceEvent {
    /// Creates an event with control characters replaced and a bounded
    /// insertion string.
    pub fn new(id: ServiceEventId, level: ServiceEventLevel, message: impl AsRef<str>) -> Self {
        Self {
            id,
            level,
            message: sanitize_message(message.as_ref()),
        }
    }
}

/// Injected publication boundary for service lifecycle events.
pub trait ServiceEventSink {
    fn emit(&self, event: &ServiceEvent) -> Result<(), String>;
}

/// Native publisher for the local Windows Application Event Log.
pub struct WindowsEventLog {
    handle: HANDLE,
}

impl WindowsEventLog {
    /// Opens an event source on the local Windows computer.
    pub fn open(source_name: &str) -> Result<Self, String> {
        if source_name.trim().is_empty() {
            return Err("Windows Event Log source name must be non-empty".to_owned());
        }
        let source_name = to_wide(source_name)?;
        // SAFETY: both pointers reference valid, NUL-terminated UTF-16 for the
        // duration of the call; a null server name selects the local computer.
        let handle = unsafe { RegisterEventSourceW(std::ptr::null(), source_name.as_ptr()) };
        if handle.is_null() {
            return Err(format!(
                "RegisterEventSourceW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { handle })
    }
}

impl ServiceEventSink for WindowsEventLog {
    fn emit(&self, event: &ServiceEvent) -> Result<(), String> {
        let message = to_wide(&event.message)?;
        let mut strings = [message.as_ptr()];
        // SAFETY: the registered handle remains owned by `self`; `strings`
        // contains one valid NUL-terminated UTF-16 pointer and no raw data is
        // supplied. ReportEventW copies the insertion string during the call.
        let reported = unsafe {
            ReportEventW(
                self.handle,
                event.level.event_type(),
                0,
                event.id as u32,
                std::ptr::null_mut(),
                1,
                0,
                strings.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if reported == 0 {
            Err(format!(
                "ReportEventW failed: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for WindowsEventLog {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: this non-null handle was returned by
            // RegisterEventSourceW and is released exactly once here.
            unsafe {
                DeregisterEventSource(self.handle);
            }
        }
    }
}

fn sanitize_message(message: &str) -> String {
    let mut sanitized = String::with_capacity(message.len().min(MAX_EVENT_MESSAGE_CHARS));
    for character in message.chars().take(MAX_EVENT_MESSAGE_CHARS) {
        sanitized.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_messages_are_single_line_and_bounded() {
        let message = format!("failure\r\n{}", "x".repeat(MAX_EVENT_MESSAGE_CHARS + 20));
        let event = ServiceEvent::new(
            ServiceEventId::WorkerFailed,
            ServiceEventLevel::Error,
            message,
        );

        assert!(!event.message.contains('\r'));
        assert!(!event.message.contains('\n'));
        assert_eq!(event.message.chars().count(), MAX_EVENT_MESSAGE_CHARS);
    }

    #[test]
    fn event_ids_are_stable() {
        assert_eq!(ServiceEventId::StartPending as u32, 1_000);
        assert_eq!(ServiceEventId::Stopped as u32, 1_003);
        assert_eq!(ServiceEventId::ConfigurationFailed as u32, 2_000);
        assert_eq!(ServiceEventId::StatusPublishFailed as u32, 2_003);
    }
}
