// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live migration protocol and implementation.
//!
//! Provides the migration protocol (WebSocket-based), codec, memory
//! transfer with dirty page tracking, and device state serialization.

pub mod codec;
pub mod destination;
pub mod limits;
pub mod pages;
pub mod protocol;
pub mod source;
pub mod state;
pub mod sync_destination;
pub mod sync_source;

/// A stream that can be either a TCP connection or a Unix socket.
/// Implements AsyncRead + AsyncWrite so it can be used with the
/// generic migration protocol.
pub enum MigrateStream {
    Tcp(tokio::net::TcpStream),
    Unix(tokio::net::UnixStream),
}

impl tokio::io::AsyncRead for MigrateStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            MigrateStream::Tcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            MigrateStream::Unix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for MigrateStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            MigrateStream::Tcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            MigrateStream::Unix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            MigrateStream::Tcp(s) => std::pin::Pin::new(s).poll_flush(cx),
            MigrateStream::Unix(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            MigrateStream::Tcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            MigrateStream::Unix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
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
    /// Handoff, destination resumes.
    Finish,
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
