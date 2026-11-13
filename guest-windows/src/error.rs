use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ServiceHostError {
    #[error("service host is already running")]
    AlreadyRunning,
    #[error("service host has already stopped")]
    AlreadyStopped,
    #[error("service worker initialization failed: {0}")]
    Startup(String),
    #[error("service worker failed: {0}")]
    Worker(String),
    #[error("service host shutdown timeout must be greater than zero")]
    InvalidShutdownTimeout,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigurationError {
    #[error("service configuration file is required: {0}")]
    MissingFile(String),
    #[error("configuration file I/O failed: {0}")]
    FileIo(String),
    #[error("configuration file is invalid: {0}")]
    InvalidFile(String),
    #[error("unsupported configuration schema version: {0}")]
    UnsupportedSchemaVersion(u32),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeWiringError {
    #[error("native telemetry worker construction failed: {0}")]
    WorkerConstruction(String),
    #[error("service host execution failed: {0}")]
    HostExecution(String),
}
