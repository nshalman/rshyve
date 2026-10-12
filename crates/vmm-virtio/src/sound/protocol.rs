// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! virtio-sound wire format (virtio 1.3 section 5.14).
//!
//! The values are Linux's `include/uapi/linux/virtio_snd.h`, which is
//! what the guest driver sends. Every field is little-endian, and every
//! struct spells out its padding so the zerocopy traits derive.

use zerocopy::{FromBytes, Immutable, IntoBytes};

// Virtqueues.
pub const VQ_CONTROL: u16 = 0;
pub const VQ_EVENT: u16 = 1;
pub const VQ_TX: u16 = 2;
pub const VQ_RX: u16 = 3;
pub const NUM_QUEUES: usize = 4;

// Directions.
pub const D_OUTPUT: u8 = 0;

// Control requests.
pub const R_JACK_INFO: u32 = 1;
pub const R_PCM_INFO: u32 = 0x0100;
pub const R_PCM_SET_PARAMS: u32 = 0x0101;
pub const R_PCM_PREPARE: u32 = 0x0102;
pub const R_PCM_RELEASE: u32 = 0x0103;
pub const R_PCM_START: u32 = 0x0104;
pub const R_PCM_STOP: u32 = 0x0105;
pub const R_CHMAP_INFO: u32 = 0x0200;

// Statuses.
pub const S_OK: u32 = 0x8000;
pub const S_BAD_MSG: u32 = 0x8001;
pub const S_NOT_SUPP: u32 = 0x8002;
pub const S_IO_ERR: u32 = 0x8003;

// Sample formats and rates, as bit positions.
pub const PCM_FMT_S16: u8 = 5;
pub const PCM_RATE_48000: u8 = 7;

// Channel positions.
pub const CHMAP_FL: u8 = 3;
pub const CHMAP_FR: u8 = 4;
pub const CHMAP_MAX_SIZE: usize = 18;

/// `struct virtio_snd_config`. `controls` is read only with
/// VIRTIO_SND_F_CTLS, which this device does not offer.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct Config {
    pub jacks: u32,
    pub streams: u32,
    pub chmaps: u32,
    pub controls: u32,
}

/// `struct virtio_snd_hdr`: a request's code, a response's status.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct Hdr {
    pub code: u32,
}

/// `struct virtio_snd_query_info`, without its header.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct QueryInfo {
    pub start_id: u32,
    pub count: u32,
    pub size: u32,
}

/// `struct virtio_snd_pcm_info`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct PcmInfo {
    pub hda_fn_nid: u32,
    pub features: u32,
    pub formats: u64,
    pub rates: u64,
    pub direction: u8,
    pub channels_min: u8,
    pub channels_max: u8,
    pub padding: [u8; 5],
}

/// `struct virtio_snd_chmap_info`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct ChmapInfo {
    pub hda_fn_nid: u32,
    pub direction: u8,
    pub channels: u8,
    pub positions: [u8; CHMAP_MAX_SIZE],
}

/// `struct virtio_snd_pcm_hdr`, without its header: the stream a PCM
/// request is for.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct PcmHdr {
    pub stream_id: u32,
}

/// `struct virtio_snd_pcm_set_params`, without its `virtio_snd_pcm_hdr`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct PcmSetParams {
    pub buffer_bytes: u32,
    pub period_bytes: u32,
    pub features: u32,
    pub channels: u8,
    pub format: u8,
    pub rate: u8,
    pub padding: u8,
}

/// `struct virtio_snd_pcm_xfer`: the head of every I/O message.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct PcmXfer {
    pub stream_id: u32,
}

/// `struct virtio_snd_pcm_status`: the tail of every I/O message.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct PcmStatus {
    pub status: u32,
    pub latency_bytes: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes of the C structs. A field in the wrong place changes
    /// one.
    #[test]
    fn wire_sizes_match_the_header() {
        assert_eq!(size_of::<Config>(), 16);
        assert_eq!(size_of::<QueryInfo>() + size_of::<Hdr>(), 16);
        assert_eq!(size_of::<PcmInfo>(), 32);
        assert_eq!(size_of::<ChmapInfo>(), 24);
        assert_eq!(
            size_of::<Hdr>() + size_of::<PcmHdr>() + size_of::<PcmSetParams>(),
            24
        );
        assert_eq!(size_of::<PcmXfer>(), 4);
        assert_eq!(size_of::<PcmStatus>(), 8);
    }
}
