//! The guest side of the memory request protocol, for every guest OS: the
//! durable request store, the worker that samples, decides and publishes,
//! the transports to the host (control channel or control files), and the
//! service loop that takes its settings from the host. A guest OS supplies
//! its memory counters, its pressure signal, its clock and how it opens the
//! control channel.

pub mod files;
pub mod requests;
pub mod serve;
pub mod transport;
pub mod worker;
