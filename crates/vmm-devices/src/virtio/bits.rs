// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO specification constants.
//!
//! Covers device status bits, virtqueue descriptor flags, feature bits,
//! legacy PCI transport register layout, and PCI identity values.

// ---------------------------------------------------------------------------
// Device status register bits (VirtIO 1.2 sec 2.1)
// ---------------------------------------------------------------------------

/// Guest OS has found the device and recognized it as a valid virtio device.
pub const STATUS_ACKNOWLEDGE: u8 = 1;
/// Guest OS knows how to drive the device.
pub const STATUS_DRIVER: u8 = 2;
/// Driver is set up and ready to drive the device.
pub const STATUS_DRIVER_OK: u8 = 4;
/// Driver has acknowledged all the features it understands.
pub const STATUS_FEATURES_OK: u8 = 8;
/// Device has experienced an error from which it cannot recover.
pub const STATUS_DEVICE_NEEDS_RESET: u8 = 64;
/// Guest OS has given up on the device.
pub const STATUS_FAILED: u8 = 128;

// ---------------------------------------------------------------------------
// VirtQueue descriptor flags (VirtIO 1.2 sec 2.7.5)
// ---------------------------------------------------------------------------

/// This marks a buffer as continuing via the `next` field.
pub const VRING_DESC_F_NEXT: u16 = 1;
/// This marks a buffer as device write-only (otherwise device read-only).
pub const VRING_DESC_F_WRITE: u16 = 2;
/// This means the buffer contains a list of buffer descriptors.
pub const VRING_DESC_F_INDIRECT: u16 = 4;

// ---------------------------------------------------------------------------
// VirtIO feature bits (VirtIO 1.2 sec 6)
// ---------------------------------------------------------------------------

/// Device supports indirect descriptors.
pub const VIRTIO_F_RING_INDIRECT_DESC: u64 = 1 << 28;
/// Device supports the event index feature for suppressing notifications.
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << 29;
/// Device complies with VirtIO 1.0+ specification.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// ---------------------------------------------------------------------------
// VirtIO block request types (VirtIO 1.2 sec 5.2.6)
// ---------------------------------------------------------------------------

/// Read from disk.
pub const VIRTIO_BLK_T_IN: u32 = 0;
/// Write to disk.
pub const VIRTIO_BLK_T_OUT: u32 = 1;
/// Flush write cache.
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;

/// Block request status: success.
pub const VIRTIO_BLK_S_OK: u8 = 0;
/// Block request status: I/O error.
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
/// Block request status: unsupported request.
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// ---------------------------------------------------------------------------
// VirtIO block feature bits
// ---------------------------------------------------------------------------

/// Device supports read-only mode.
pub const VIRTIO_BLK_F_RO: u64 = 1 << 5;
/// Device supports flush command.
pub const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
/// Device exports block size.
pub const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;

// ---------------------------------------------------------------------------
// Legacy PCI transport register offsets (VirtIO 1.2 sec 4.1.4.8)
// ---------------------------------------------------------------------------

/// Device features (4 bytes, read-only).
pub const LEGACY_REG_DEVICE_FEATURES: u16 = 0x00;
/// Guest (driver) features (4 bytes, write-only).
pub const LEGACY_REG_GUEST_FEATURES: u16 = 0x04;
/// Queue PFN (4 bytes, read/write). Page frame number of selected queue.
pub const LEGACY_REG_QUEUE_PFN: u16 = 0x08;
/// Queue size (2 bytes, read-only). Max descriptors in selected queue.
pub const LEGACY_REG_QUEUE_SIZE: u16 = 0x0C;
/// Queue select (2 bytes, write-only). Selects which queue to configure.
pub const LEGACY_REG_QUEUE_SELECT: u16 = 0x0E;
/// Queue notify (2 bytes, write-only). Writing queue index triggers processing.
pub const LEGACY_REG_QUEUE_NOTIFY: u16 = 0x10;
/// Device status (1 byte, read/write).
pub const LEGACY_REG_DEVICE_STATUS: u16 = 0x12;
/// ISR status (1 byte, read-only). Reading clears the register.
pub const LEGACY_REG_ISR_STATUS: u16 = 0x13;
/// Start of device-specific configuration space.
pub const LEGACY_REG_DEVICE_CONFIG: u16 = 0x14;

/// Size of legacy common config registers (excluding device-specific).
pub const LEGACY_COMMON_SIZE: u16 = 0x14;

// ---------------------------------------------------------------------------
// PCI identity for VirtIO devices
// ---------------------------------------------------------------------------

/// VirtIO PCI vendor ID (Red Hat).
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1AF4;

// ---------------------------------------------------------------------------
// VirtIO device type IDs
// ---------------------------------------------------------------------------

/// Network device.
pub const VIRTIO_DEV_TYPE_NET: u16 = 1;
/// Block device.
pub const VIRTIO_DEV_TYPE_BLOCK: u16 = 2;

/// Sector size for virtio-block (always 512 bytes).
pub const SECTOR_SIZE: u64 = 512;

// ---------------------------------------------------------------------------
// ISR status bits
// ---------------------------------------------------------------------------

/// Queue interrupt pending.
pub const ISR_QUEUE_INTR: u8 = 1;
/// Device configuration change pending.
pub const ISR_CFG_CHANGE: u8 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bits_are_distinct() {
        let all = STATUS_ACKNOWLEDGE
            | STATUS_DRIVER
            | STATUS_DRIVER_OK
            | STATUS_FEATURES_OK
            | STATUS_DEVICE_NEEDS_RESET
            | STATUS_FAILED;
        // Each bit is distinct; OR-ing them all should have 6 bits set
        assert_eq!(all.count_ones(), 6);
    }

    #[test]
    fn descriptor_flags_are_distinct() {
        assert_eq!(VRING_DESC_F_NEXT & VRING_DESC_F_WRITE, 0);
        assert_eq!(VRING_DESC_F_NEXT & VRING_DESC_F_INDIRECT, 0);
        assert_eq!(VRING_DESC_F_WRITE & VRING_DESC_F_INDIRECT, 0);
    }

    #[test]
    fn legacy_register_offsets_non_overlapping() {
        assert!(LEGACY_REG_DEVICE_FEATURES < LEGACY_REG_GUEST_FEATURES);
        assert!(LEGACY_REG_GUEST_FEATURES < LEGACY_REG_QUEUE_PFN);
        assert!(LEGACY_REG_QUEUE_PFN < LEGACY_REG_QUEUE_SIZE);
        assert!(LEGACY_REG_QUEUE_SIZE < LEGACY_REG_QUEUE_SELECT);
        assert!(LEGACY_REG_QUEUE_SELECT < LEGACY_REG_QUEUE_NOTIFY);
        assert!(LEGACY_REG_QUEUE_NOTIFY < LEGACY_REG_DEVICE_STATUS);
        assert!(LEGACY_REG_DEVICE_STATUS < LEGACY_REG_ISR_STATUS);
        assert!(LEGACY_REG_ISR_STATUS < LEGACY_REG_DEVICE_CONFIG);
    }

    #[test]
    fn device_config_starts_at_0x14() {
        assert_eq!(LEGACY_REG_DEVICE_CONFIG, 0x14);
    }
}
