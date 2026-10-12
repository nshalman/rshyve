// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! virtio-gpu wire format (virtio 1.3 §5.7.6).
//!
//! Every field is little-endian. Every wire struct derives the zerocopy
//! traits, and `IntoBytes` does not derive on a struct with implicit
//! padding, so each struct spells out its padding the way the spec
//! does.

use zerocopy::{FromBytes, Immutable, IntoBytes};

// 2D commands.
pub const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const CMD_SET_SCANOUT: u32 = 0x0103;
pub const CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
pub const CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
pub const CMD_SET_SCANOUT_BLOB: u32 = 0x010d;

// Cursor commands.
pub const CMD_UPDATE_CURSOR: u32 = 0x0300;
pub const CMD_MOVE_CURSOR: u32 = 0x0301;

// Success responses.
pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

// Error responses.
pub const RESP_ERR_UNSPEC: u32 = 0x1200;
pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// The response must carry the request's fence id.
pub const FLAG_FENCE: u32 = 1 << 0;

/// Feature: the device takes blob resources.
pub const F_RESOURCE_BLOB: u64 = 1 << 3;

/// A blob whose memory is the guest's pages, attached at creation.
pub const BLOB_MEM_GUEST: u32 = 0x0001;

/// Scanouts the spec allows a device to report.
pub const MAX_SCANOUTS: usize = 16;

/// Config-space event: the display configuration changed.
pub const EVENT_DISPLAY: u32 = 1 << 0;

// Formats. The names give the byte order in memory.
pub const FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const FORMAT_B8G8R8X8_UNORM: u32 = 2;

/// `struct virtio_gpu_ctrl_hdr`: the head of every request and response.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct CtrlHdr {
    pub type_: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

/// `struct virtio_gpu_rect`.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, FromBytes, Immutable, IntoBytes,
)]
#[repr(C)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Whether this rectangle lies within a `width` x `height` area.
    ///
    /// Every field is guest-supplied, so the edges are summed checked.
    pub fn fits_within(&self, width: u32, height: u32) -> bool {
        let right = self.x.checked_add(self.width);
        let bottom = self.y.checked_add(self.height);
        matches!((right, bottom), (Some(r), Some(b)) if r <= width && b <= height)
    }

    /// The overlap of two rectangles, or `None` when they do not meet.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self
            .x
            .saturating_add(self.width)
            .min(other.x.saturating_add(other.width));
        let y1 = self
            .y
            .saturating_add(self.height)
            .min(other.y.saturating_add(other.height));
        (x1 > x0 && y1 > y0).then(|| Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }

    /// The smallest rectangle covering both.
    pub fn union(&self, other: &Rect) -> Rect {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self
            .x
            .saturating_add(self.width)
            .max(other.x.saturating_add(other.width));
        let y1 = self
            .y
            .saturating_add(self.height)
            .max(other.y.saturating_add(other.height));
        Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    }
}

/// One entry of `struct virtio_gpu_resp_display_info`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct DisplayOne {
    pub r: Rect,
    pub enabled: u32,
    pub flags: u32,
}

/// `struct virtio_gpu_resp_display_info`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct RespDisplayInfo {
    pub hdr: CtrlHdr,
    pub pmodes: [DisplayOne; MAX_SCANOUTS],
}

/// `struct virtio_gpu_resource_create_2d`, without its header.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct ResourceCreate2d {
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

/// `struct virtio_gpu_resource_unref`, without its header.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct ResourceUnref {
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_set_scanout`, without its header.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct SetScanout {
    pub r: Rect,
    pub scanout_id: u32,
    pub resource_id: u32,
}

/// `struct virtio_gpu_resource_flush`, without its header.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct ResourceFlush {
    pub r: Rect,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_transfer_to_host_2d`, without its header.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct TransferToHost2d {
    pub r: Rect,
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_resource_attach_backing`, without its header.
/// `nr_entries` [`MemEntry`] values follow it.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct ResourceAttachBacking {
    pub resource_id: u32,
    pub nr_entries: u32,
}

/// `struct virtio_gpu_mem_entry`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct MemEntry {
    pub addr: u64,
    pub length: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_resource_detach_backing`, without its header.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct ResourceDetachBacking {
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_resource_create_blob`, without its header.
/// `nr_entries` [`MemEntry`] values follow it.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct ResourceCreateBlob {
    pub resource_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}

/// `struct virtio_gpu_set_scanout_blob`, without its header. `r` is
/// the part of the `width` x `height` image the scanout shows; plane 0
/// starts `offsets[0]` bytes into the blob, rows `strides[0]` apart.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct SetScanoutBlob {
    pub r: Rect,
    pub scanout_id: u32,
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub padding: u32,
    pub strides: [u32; 4],
    pub offsets: [u32; 4],
}

/// `struct virtio_gpu_cursor_pos`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct CursorPos {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_update_cursor`, without its header. Both cursor
/// commands share it; a move reads only `pos`.
#[derive(Debug, Default, Clone, Copy, FromBytes, Immutable, IntoBytes)]
#[repr(C)]
pub struct UpdateCursor {
    pub pos: CursorPos,
    pub resource_id: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub padding: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes the spec's C structs have. A field added in the wrong
    /// place changes one of these.
    #[test]
    fn wire_sizes_match_the_spec() {
        assert_eq!(size_of::<CtrlHdr>(), 24);
        assert_eq!(size_of::<Rect>(), 16);
        assert_eq!(size_of::<RespDisplayInfo>(), 24 + 16 * 24);
        assert_eq!(size_of::<ResourceCreate2d>(), 16);
        assert_eq!(size_of::<SetScanout>(), 24);
        assert_eq!(size_of::<ResourceFlush>(), 24);
        assert_eq!(size_of::<TransferToHost2d>(), 32);
        assert_eq!(size_of::<ResourceAttachBacking>(), 8);
        assert_eq!(size_of::<MemEntry>(), 16);
        assert_eq!(size_of::<UpdateCursor>(), 32);
        assert_eq!(size_of::<ResourceCreateBlob>(), 32);
        assert_eq!(size_of::<SetScanoutBlob>(), 72);
    }

    #[test]
    fn a_rect_that_overflows_does_not_fit() {
        let r = Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1,
        };
        assert!(!r.fits_within(u32::MAX, 1));
    }

    #[test]
    fn rects_fit_up_to_the_edge() {
        let r = Rect {
            x: 10,
            y: 20,
            width: 90,
            height: 80,
        };
        assert!(r.fits_within(100, 100));
        assert!(!r.fits_within(99, 100));
        assert!(!r.fits_within(100, 99));
    }

    #[test]
    fn intersect_and_union() {
        let a = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let b = Rect {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        };
        assert_eq!(
            a.intersect(&b),
            Some(Rect {
                x: 5,
                y: 5,
                width: 5,
                height: 5
            })
        );
        assert_eq!(
            a.union(&b),
            Rect {
                x: 0,
                y: 0,
                width: 15,
                height: 15
            }
        );
        let far = Rect {
            x: 20,
            y: 20,
            width: 1,
            height: 1,
        };
        assert_eq!(a.intersect(&far), None);
    }
}
