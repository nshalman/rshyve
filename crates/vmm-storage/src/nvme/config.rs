// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a controller shows the guest, chosen per device.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LbaSize {
    /// LBADS 9. What the file backend has always shown.
    #[default]
    B512,
    /// LBADS 12.
    B4096,
}

impl LbaSize {
    pub fn bytes(self) -> u32 {
        match self {
            Self::B512 => 512,
            Self::B4096 => 4096,
        }
    }
}

/// How many MSIs a batch of completions raises.
///
/// A batch is every completion written under one hold of the state
/// lock: one doorbell, one call to the completion sink.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MsiPolicy {
    /// One per completion queue that got completions.
    #[default]
    PerBatch,
    /// One per completion, to measure what batching saves.
    PerCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvmeConfig {
    pub lba_size: LbaSize,
    /// Whether Identify reports a volatile write cache.
    ///
    /// Without one the guest sends no flush and takes every completed
    /// write as durable, so the controller refuses a backend that does
    /// not say its writes are.
    pub volatile_write_cache: bool,
    pub msi_policy: MsiPolicy,
}

impl Default for NvmeConfig {
    fn default() -> Self {
        Self {
            lba_size: LbaSize::B512,
            volatile_write_cache: true,
            msi_policy: MsiPolicy::PerBatch,
        }
    }
}
