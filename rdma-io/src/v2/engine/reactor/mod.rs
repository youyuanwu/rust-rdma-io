//! Incremental command boundary for the driver-owned reactor migration.
//!
//! This module deliberately does not own session lifecycle state yet. It
//! admits bounded frontend commands and lets the explicit engine driver
//! transfer them into the existing authoritative session backend.

pub(super) mod command;
pub(super) mod completion;

use std::sync::Arc;

use super::EngineShared;

pub(super) use command::CommandIngress;

/// Synchronous terminal adapter used when the sole progress owner disappears.
///
/// Driver drop cannot enqueue work because no later poll is guaranteed.
pub(super) struct DriverTermination;

impl DriverTermination {
    pub(super) fn terminate(shared: &Arc<EngineShared>) {
        shared.handle_driver_drop();
    }
}
