// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live migration protocol and implementation.
//!
//! One driver, over any `Read + Write` stream: a Unix socket inside a
//! bhyve zone, where `/dev/poll` is unavailable, or a TCP socket. Two
//! drivers for two transports drifted apart once already, and the
//! async twin shipped no device state at all.

pub mod codec;
pub mod cpu;
pub mod destination;
pub mod limits;
pub mod pages;
pub mod protocol;
pub mod source;
pub mod state;
pub mod wire;

/// Pauses or resumes the vCPUs.
///
/// A hook and not a direct `VM_PAUSE`, because the kernel keeps one
/// pause flag per instance and the owner may already hold it, or share
/// it with an operator pause and a hot-unplug.
pub type VmPauseFn = Box<dyn Fn() -> std::io::Result<()> + Send>;

/// The pair a VM with nothing else pausing it uses.
pub fn hdl_pause_hooks(
    hdl: &std::sync::Arc<vmm_core::hdl::VmmHdl>,
) -> (VmPauseFn, VmPauseFn) {
    let for_pause = std::sync::Arc::clone(hdl);
    let for_resume = std::sync::Arc::clone(hdl);
    (
        Box::new(move || for_pause.pause()),
        Box::new(move || for_resume.resume()),
    )
}

/// Migration phases, executed in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPhase {
    /// Protocol negotiation and instance spec comparison.
    Sync,
    /// Pre-pause iterative dirty page transfer.
    RamPushPrePause,
    /// Pause all vCPUs and devices.
    Pause,
    /// Transfer pages dirtied since pre-pause.
    RamPushPostPause,
    /// TSC, boot hrtime synchronization.
    TimeData,
    /// Export/import all device state.
    DeviceState,
    /// Destination requests any missing pages.
    RamPull,
    /// The destination says it holds everything.
    Finish,
    /// The source has handed the guest over and cannot take it back.
    ///
    /// Past this point a rollback would run a second copy of the same
    /// guest against the same disk, so the source stays paused however
    /// the rest of the exchange ends.
    Committed,
    /// Migration failed.
    Error,
}

impl std::fmt::Display for MigrationPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sync => write!(f, "sync"),
            Self::RamPushPrePause => write!(f, "ram-push-pre-pause"),
            Self::Pause => write!(f, "pause"),
            Self::RamPushPostPause => write!(f, "ram-push-post-pause"),
            Self::TimeData => write!(f, "time-data"),
            Self::DeviceState => write!(f, "device-state"),
            Self::RamPull => write!(f, "ram-pull"),
            Self::Finish => write!(f, "finish"),
            Self::Committed => write!(f, "committed"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Migration progress information.
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub phase: MigrationPhase,
    pub bytes_transferred: u64,
    pub pages_transferred: u64,
    pub dirty_pages_remaining: u64,
}

impl Default for MigrationStatus {
    fn default() -> Self {
        Self {
            phase: MigrationPhase::Sync,
            bytes_transferred: 0,
            pages_transferred: 0,
            dirty_pages_remaining: 0,
        }
    }
}
