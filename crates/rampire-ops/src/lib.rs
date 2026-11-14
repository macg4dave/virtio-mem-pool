//! RAMpire's operator views, shared by the `rampire` command and by
//! `cargo xtask` during development: the pool of every VM and one VM's
//! live state. Nothing here needs a source checkout;
//! commands run from the working directory the caller passes.

pub mod applied;
pub mod check;
pub mod dashboard;
pub mod pool;
pub mod process;
pub mod units;
pub mod watch;
