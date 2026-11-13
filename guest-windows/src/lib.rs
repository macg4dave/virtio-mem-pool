pub mod config;
pub mod demand;
mod diagnostics;
pub mod error;
pub mod event_log;
mod native;
pub mod requests;
pub mod runtime;
pub mod service_host;
pub mod service_scm;
mod wake;

/// Build identity stamped by `xtask windows build` from the synchronised
/// source (`<commit>` or `<commit>-dirty.<source digest>`); local builds
/// without the stamp report `unversioned`.
pub const BUILD_ID: &str = match option_env!("VIRTIO_MEM_BUILD_ID") {
    Some(id) => id,
    None => "unversioned",
};

pub use config::ServiceConfig;
#[cfg(windows)]
pub use demand::{native_memory_resource_notifications, WindowsMemoryResourceNotificationApi};
pub use demand::{
    process_session_id, AtomicRawTelemetryPublisher, DemandError, MemoryResourceNotificationApi,
    MemoryResourceNotificationCollector, MemoryResourceNotificationKind,
    MemoryResourceNotificationState, MemoryResourceNotificationTelemetry, MemoryTelemetry,
    MemoryTelemetrySnapshot, NativeMemoryTelemetry, RawTelemetryEnvelope, RawTelemetryPublisher,
    SystemTelemetryClock, TelemetryClock, UnavailableMemoryResourceNotifications,
};
pub use error::{ConfigurationError, RuntimeWiringError, ServiceHostError};
pub use event_log::{
    ServiceEvent, ServiceEventId, ServiceEventLevel, ServiceEventSink, WindowsEventLog,
};
pub use requests::GuestRequests;
pub use runtime::GuestMemoryWorker;
#[cfg(windows)]
pub use runtime::{native_worker, NativeWorker};
pub use service_host::StopSignal;
pub use service_host::{ServiceHost, ServiceState, ServiceWorker};
pub use service_scm::{
    install_service, remove_service, run_as_service, start_service, stop_service, ScmServiceStatus,
    WindowsServiceRegistration,
};
