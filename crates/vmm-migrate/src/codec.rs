// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration codec for WebSocket-based data transfer.
//!
//! Message types for the migration protocol, using RON serialization
//! for structured data and raw binary for memory pages.

use serde::{Deserialize, Serialize};

/// Message types in the migration protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Acknowledgement / success.
    Okay = 0,
    /// Error with description.
    Error = 1,
    /// RON-serialized structured data.
    Serialized = 2,
    /// Raw binary blob.
    Blob = 3,
    /// Memory page data.
    Page = 4,
    /// Memory region query.
    MemQuery = 5,
    /// Memory region offer (dirty bitmap).
    MemOffer = 6,
    /// Request specific pages.
    MemFetch = 7,
    /// Page data transfer.
    MemXfer = 8,
    /// End of memory transfer phase.
    MemEnd = 9,
    /// Memory transfer complete acknowledgement.
    MemDone = 10,
}

/// Protocol negotiation offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolOffer {
    /// List of supported protocol strings (e.g., "vmm-migrate-ron/0").
    pub protocols: Vec<String>,
}

/// Protocol negotiation selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolSelect {
    /// Selected protocol string.
    pub protocol: String,
}

/// Memory region descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemRegionDesc {
    /// Guest physical address.
    pub gpa: u64,
    /// Length in bytes.
    pub len: u64,
}

/// Preamble exchanged during the Sync phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPreamble {
    /// VM configuration for compatibility check.
    pub num_cpus: u32,
    pub mem_size: u64,
    /// Device list for compatibility.
    pub devices: Vec<String>,
}
