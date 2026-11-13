//! Platform-neutral virtio-mem state, policy, and QEMU Guest Agent parsing.

pub mod compatibility;
pub mod controller_status;
pub mod demand;
pub mod error;
pub mod guest_policy;
pub mod memory_request;
pub mod pool;
pub mod settings;
pub mod target_controller;
pub mod units;
pub mod virtio_mem;
pub mod virtio_mem_xml;
pub mod windows_service_config;

pub use compatibility::{
    CompatibilityEvidence, VirtioMemCompatibility, VirtioMemCompatibilityError,
};
pub use controller_status::{
    parse_controller_status, CommandDirection, CommandOwnership, ControlHealth,
    ControllerCommandStatus, ControllerStatusError, ControllerStatusSnapshot, GuestInputStatus,
    RecoveryState, CONTROLLER_STATUS_VERSION, MAX_CONTROLLER_STATUS_BYTES,
};
pub use demand::{
    DemandError, MemoryResourceNotificationState, MemoryTelemetrySnapshot, RawTelemetryEnvelope,
    RAW_TELEMETRY_VERSION,
};
pub use error::VirtioMemError;
pub use guest_policy::{
    plan_request, GuestDecision, GuestMemoryPolicy, GuestRequestState, GuestRequester,
};
pub use memory_request::{
    bounded_reason, host_status_path, request_state_path, GuestSettings, HostMemoryStatus,
    MemoryOperation,
    MemoryOutcome, MemoryRequest, MemoryRequestJournal, MemoryResult, RequestAdmission,
    RequestIdentity, MAX_HOST_STATUS_BYTES, MAX_REQUEST_LIFETIME_MILLIS, MEMORY_REQUEST_VERSION,
    MEMORY_WAKE_CHANNEL_NAME,
};
pub use pool::{allocate_pool, PoolAllocation, PoolError, PoolMember};
pub use target_controller::{
    TargetEstimate, TargetEstimator, TargetEstimatorError, TargetGeometry, TargetPolicyConfig,
    TargetSample,
};
pub use units::{bytes_to_kibibytes, kibibytes_to_bytes, parse_byte_size, BYTES_PER_KIB};
pub use virtio_mem::{VirtioMemState, MIN_BLOCK_SIZE_BYTES};
pub use virtio_mem_xml::{
    parse_virtio_mem_xml, parse_virtio_mem_xml_for_alias, VirtioMemXmlError, VirtioMemXmlState,
};
pub use windows_service_config::{WindowsServiceConfigFile, WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION};
