// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live migration protocol and implementation.
//!
//! One driver, over any `Read + Write` stream: a Unix socket inside a
//! bhyve zone, where `/dev/poll` is unavailable, or a TCP socket. Two
//! drivers for two transports drifted apart once already, and the
//! async twin shipped no device state at all.

pub mod clock;
pub mod codec;
pub mod connect;
pub mod cpu;
pub mod destination;
pub mod gate;
pub mod limits;
pub mod pages;
pub mod protocol;
pub mod source;
pub mod stall;
pub mod state;
pub mod wait;
pub mod wire;
pub mod xsave;

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

/// Why a migration a stop or `migrate-cancel` cancelled failed.
pub const CANCELLED: &str = "the migration was cancelled";

/// The error a cancelled migration ends with.
pub fn cancelled() -> codec::MigrateError {
    codec::MigrateError::Phase(CANCELLED.to_string())
}

/// Whether `error` is this end's own cancel, which the peer has not
/// heard about yet.
pub fn is_cancel(error: &codec::MigrateError) -> bool {
    matches!(error, codec::MigrateError::Phase(why) if why == CANCELLED)
}

/// How the owner lets a stop cancel a migration that has not committed.
pub trait StopGate: Send + Sync {
    /// Whether a stop has cancelled the migration. Read often, to give
    /// up early; [`StopGate::commit`] is the check that counts.
    fn cancelled(&self) -> bool;

    /// Claim the step this end cannot take back. Refused once a stop
    /// has cancelled the migration; a stop after it no longer cancels.
    /// Claimed before the send, because a send that fails may still
    /// have reached the peer.
    fn commit(&self) -> Result<(), codec::MigrateError>;

    /// Run `shut` when a stop frees this migration's blocked I/O, or at
    /// once if one already has. It shuts a socket the migration opened.
    fn on_stop(&self, shut: Box<dyn FnOnce() + Send>);
}

/// The gate for a VM with nothing that can stop it mid-migration.
pub struct NoStop;

impl StopGate for NoStop {
    fn cancelled(&self) -> bool {
        false
    }

    fn commit(&self) -> Result<(), codec::MigrateError> {
        Ok(())
    }

    fn on_stop(&self, _shut: Box<dyn FnOnce() + Send>) {}
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

/// Where a source is at its cutover gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GateState {
    /// No gate, or not reached yet.
    #[default]
    None,
    Waiting,
    /// The orchestrator said go. The source never resumes by itself.
    Go,
    Abort,
}

impl std::fmt::Display for GateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Waiting => "waiting",
            Self::Go => "go",
            Self::Abort => "abort",
        })
    }
}

/// Migration progress information.
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub phase: MigrationPhase,
    pub bytes_transferred: u64,
    pub pages_transferred: u64,
    pub dirty_pages_remaining: u64,
    pub gate: GateState,
    /// On a destination: how far this host's wall clock is ahead of the
    /// source's, in ms. See [`clock`].
    pub clock_skew_ms: Option<i64>,
    /// Why the migration failed. Set with [`MigrationPhase::Error`].
    pub error: Option<String>,
}

impl Default for MigrationStatus {
    fn default() -> Self {
        Self {
            phase: MigrationPhase::Sync,
            bytes_transferred: 0,
            pages_transferred: 0,
            dirty_pages_remaining: 0,
            gate: GateState::None,
            clock_skew_ms: None,
            error: None,
        }
    }
}

/// Show a failure in the status. A committed source keeps its phase:
/// the orchestrator reads `committed` to know the guest may have moved.
pub fn record_failure(
    status: &std::sync::Mutex<MigrationStatus>,
    error: &impl std::fmt::Display,
) {
    if let Ok(mut s) = status.lock() {
        if s.phase != MigrationPhase::Committed {
            s.phase = MigrationPhase::Error;
        }
        s.error = Some(error.to_string());
    }
}
