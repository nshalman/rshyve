// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO block device.
//!
//! Implements a virtio-blk device backed by a host file. The device
//! handles read, write, and flush requests from the guest through
//! the virtqueue descriptor chain protocol.
//!
//! # Request format
//!
//! Each request is a descriptor chain with:
//! 1. Header descriptor (device-readable): type (u32), reserved (u32), sector (u64)
//! 2. Data descriptor(s): sector data buffers
//! 3. Status descriptor (device-writable, 1 byte): 0=OK, 1=IOERR, 2=UNSUPP
//!
//! # Device-specific config (at BAR0 offset 0x14)
//!
//! - capacity: u64 (total size in 512-byte sectors)

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use vmm_core::mem::PhysMap;

use super::bits;
use super::queue::VirtQueue;
use super::VirtioDevice;

/// Size of the virtio-blk config space (capacity only).
pub const BLK_CONFIG_SIZE: u16 = 8;

/// Size of the virtio-blk request header in bytes.
const BLK_REQ_HEADER_SIZE: usize = 16;

/// VirtIO block request header as laid out in guest memory.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct VirtioBlkReqHdr {
    /// Request type: IN=0, OUT=1, FLUSH=4
    rtype: u32,
    /// Reserved (unused).
    _reserved: u32,
    /// Starting sector for read/write.
    sector: u64,
}

/// Configuration options for a virtio-blk device.
#[derive(Debug, Clone)]
pub struct VirtioBlockOpts {
    /// Disk is read-only.
    pub read_only: bool,
    /// Disable TRIM/discard (nodelete). When true, the device does
    /// not advertise the discard feature.
    pub nodelete: bool,
    /// Logical sector size in bytes (default 512).
    pub sector_size: u32,
}

impl Default for VirtioBlockOpts {
    fn default() -> Self {
        Self {
            read_only: false,
            nodelete: false,
            sector_size: 512,
        }
    }
}

/// VirtIO block device backed by a file.
pub struct VirtioBlock {
    inner: Mutex<VirtioBlockInner>,
}

struct VirtioBlockInner {
    /// Backing storage file.
    file: File,
    /// Total capacity in 512-byte sectors.
    capacity: u64,
    /// Whether the device is read-only.
    read_only: bool,
    /// Logical sector size in bytes.
    sector_size: u32,
    /// Whether discard/TRIM is disabled.
    nodelete: bool,
    /// Features negotiated by the guest.
    negotiated_features: u32,
}

/// Size of the extended config when BLK_SIZE feature is advertised.
/// capacity(8) + size_max(4) + seg_max(4) + geometry(8) + blk_size(4) = 28
pub const BLK_CONFIG_SIZE_EXT: u16 = 28;

impl VirtioBlock {
    /// Create a new virtio-block device from an open file.
    ///
    /// The file's size determines the device capacity.
    ///
    /// # Errors
    ///
    /// Returns an error if the file metadata cannot be read.
    pub fn new(file: File, opts: &VirtioBlockOpts) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        let size = metadata.len();
        let capacity = size / u64::from(opts.sector_size);

        Ok(Self {
            inner: Mutex::new(VirtioBlockInner {
                file,
                capacity,
                read_only: opts.read_only,
                sector_size: opts.sector_size,
                nodelete: opts.nodelete,
                negotiated_features: 0,
            }),
        })
    }

    /// Config space size based on whether BLK_SIZE is advertised.
    pub fn config_size(&self) -> u16 {
        let inner = self.inner.lock().expect("block lock");
        if inner.sector_size != 512 {
            BLK_CONFIG_SIZE_EXT
        } else {
            BLK_CONFIG_SIZE
        }
    }

    /// Process a single block request from the virtqueue.
    ///
    /// Walks the descriptor chain, parses the request header, performs
    /// the I/O, and writes the status byte. Returns the number of
    /// bytes written to device-writable buffers.
    fn handle_request(
        &self,
        queue: &VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32 {
        // Collect the descriptor chain
        let chain = match queue.collect_chain(physmap, head) {
            Some(c) => c,
            None => return 0,
        };

        if chain.len() < 2 {
            // Need at least header + status
            return self.write_status(physmap, &chain, bits::VIRTIO_BLK_S_IOERR);
        }

        // Read the request header from the first descriptor
        let header = match &chain[0] {
            super::queue::ChainBuf::Readable { addr, len } => {
                if (*len as usize) < BLK_REQ_HEADER_SIZE {
                    return self.write_status(
                        physmap, &chain, bits::VIRTIO_BLK_S_IOERR,
                    );
                }
                match physmap.lookup(*addr, BLK_REQ_HEADER_SIZE) {
                    Some(sub) => match sub.read::<VirtioBlkReqHdr>() {
                        Ok(h) => h,
                        Err(_) => {
                            return self.write_status(
                                physmap, &chain, bits::VIRTIO_BLK_S_IOERR,
                            );
                        }
                    },
                    None => {
                        return self.write_status(
                            physmap, &chain, bits::VIRTIO_BLK_S_IOERR,
                        );
                    }
                }
            }
            _ => {
                // Header must be device-readable
                return self.write_status(
                    physmap, &chain, bits::VIRTIO_BLK_S_IOERR,
                );
            }
        };

        let mut inner = self.inner.lock().expect("block lock poisoned");

        let status = match header.rtype {
            bits::VIRTIO_BLK_T_IN => {
                self.do_read(&mut inner, header.sector, &chain[1..], physmap)
            }
            bits::VIRTIO_BLK_T_OUT => {
                if inner.read_only {
                    bits::VIRTIO_BLK_S_IOERR
                } else {
                    self.do_write(&mut inner, header.sector, &chain[1..], physmap)
                }
            }
            bits::VIRTIO_BLK_T_FLUSH => {
                self.do_flush(&mut inner)
            }
            _ => bits::VIRTIO_BLK_S_UNSUPP,
        };

        drop(inner);
        self.write_status(physmap, &chain, status)
    }

    /// Read sectors from the file into guest writable buffers.
    fn do_read(
        &self,
        inner: &mut VirtioBlockInner,
        sector: u64,
        bufs: &[super::queue::ChainBuf],
        physmap: &PhysMap,
    ) -> u8 {
        // Validate sector is within bounds
        if sector >= inner.capacity {
            return bits::VIRTIO_BLK_S_IOERR;
        }

        let offset = sector * bits::SECTOR_SIZE;
        if inner.file.seek(SeekFrom::Start(offset)).is_err() {
            return bits::VIRTIO_BLK_S_IOERR;
        }

        // Process all data descriptors (skip the last one, which is the status)
        let data_bufs = if bufs.len() > 1 { &bufs[..bufs.len() - 1] } else { return bits::VIRTIO_BLK_S_IOERR };

        for buf in data_bufs {
            match buf {
                super::queue::ChainBuf::Writable { addr, len } => {
                    let byte_len = *len as usize;
                    // Bounds check: don't read past end of device
                    let file_pos = match inner.file.stream_position() {
                        Ok(p) => p,
                        Err(_) => return bits::VIRTIO_BLK_S_IOERR,
                    };
                    if file_pos + byte_len as u64 > inner.capacity * bits::SECTOR_SIZE {
                        return bits::VIRTIO_BLK_S_IOERR;
                    }

                    let mut tmp = vec![0u8; byte_len];
                    if inner.file.read_exact(&mut tmp).is_err() {
                        return bits::VIRTIO_BLK_S_IOERR;
                    }
                    match physmap.lookup(*addr, byte_len) {
                        Some(sub) => {
                            if sub.write_bytes(&tmp).is_err() {
                                return bits::VIRTIO_BLK_S_IOERR;
                            }
                        }
                        None => return bits::VIRTIO_BLK_S_IOERR,
                    }
                }
                super::queue::ChainBuf::Readable { .. } => {
                    // Unexpected readable buffer in read request data
                    // (skip it, some guests may include the header here)
                }
            }
        }

        bits::VIRTIO_BLK_S_OK
    }

    /// Write sectors from guest readable buffers to the file.
    fn do_write(
        &self,
        inner: &mut VirtioBlockInner,
        sector: u64,
        bufs: &[super::queue::ChainBuf],
        physmap: &PhysMap,
    ) -> u8 {
        if sector >= inner.capacity {
            return bits::VIRTIO_BLK_S_IOERR;
        }

        let offset = sector * bits::SECTOR_SIZE;
        if inner.file.seek(SeekFrom::Start(offset)).is_err() {
            return bits::VIRTIO_BLK_S_IOERR;
        }

        // Process data descriptors (skip the last one, which is the status)
        let data_bufs = if bufs.len() > 1 { &bufs[..bufs.len() - 1] } else { return bits::VIRTIO_BLK_S_IOERR };

        for buf in data_bufs {
            match buf {
                super::queue::ChainBuf::Readable { addr, len } => {
                    let byte_len = *len as usize;
                    let file_pos = match inner.file.stream_position() {
                        Ok(p) => p,
                        Err(_) => return bits::VIRTIO_BLK_S_IOERR,
                    };
                    if file_pos + byte_len as u64 > inner.capacity * bits::SECTOR_SIZE {
                        return bits::VIRTIO_BLK_S_IOERR;
                    }

                    let mut tmp = vec![0u8; byte_len];
                    match physmap.lookup(*addr, byte_len) {
                        Some(sub) => {
                            if sub.read_bytes(&mut tmp).is_err() {
                                return bits::VIRTIO_BLK_S_IOERR;
                            }
                        }
                        None => return bits::VIRTIO_BLK_S_IOERR,
                    }
                    if inner.file.write_all(&tmp).is_err() {
                        return bits::VIRTIO_BLK_S_IOERR;
                    }
                }
                super::queue::ChainBuf::Writable { .. } => {
                    // Unexpected writable buffer in write request data
                }
            }
        }

        bits::VIRTIO_BLK_S_OK
    }

    /// Flush the file to disk.
    fn do_flush(&self, inner: &mut VirtioBlockInner) -> u8 {
        match inner.file.flush() {
            Ok(()) => bits::VIRTIO_BLK_S_OK,
            Err(_) => bits::VIRTIO_BLK_S_IOERR,
        }
    }

    /// Write the status byte to the last descriptor in the chain.
    ///
    /// Returns the number of bytes written to writable descriptors
    /// (always 1 for the status byte).
    fn write_status(
        &self,
        physmap: &PhysMap,
        chain: &[super::queue::ChainBuf],
        status: u8,
    ) -> u32 {
        // The status descriptor is the last one in the chain and must be writable
        if let Some(last) = chain.last() {
            match last {
                super::queue::ChainBuf::Writable { addr, len } => {
                    if *len >= 1 {
                        if let Some(sub) = physmap.lookup(*addr, 1) {
                            let _ = sub.write_bytes(&[status]);
                        }
                    }
                }
                _ => {
                    // Last descriptor is not writable; cannot write status
                }
            }
        }
        1
    }
}

impl VirtioDevice for VirtioBlock {
    fn device_features(&self) -> u32 {
        let inner = self.inner.lock().expect("block lock poisoned");
        let mut features = bits::VIRTIO_BLK_F_FLUSH as u32;
        if inner.read_only {
            features |= bits::VIRTIO_BLK_F_RO as u32;
        }
        if inner.sector_size != 512 {
            features |= bits::VIRTIO_BLK_F_BLK_SIZE as u32;
        }
        // nodelete: we simply don't advertise discard features
        features
    }

    fn set_features(&self, features: u32) {
        let mut inner = self.inner.lock().expect("block lock poisoned");
        inner.negotiated_features = features;
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        let inner = self.inner.lock().expect("block lock poisoned");

        // Device config layout (virtio 1.2 sec 5.2.4):
        //   0x00: capacity (u64, 8 bytes)
        //   0x08: size_max (u32) — not used
        //   0x0C: seg_max (u32) — not used
        //   0x10: geometry (8 bytes) — not used
        //   0x18: blk_size (u32) — sector size if BLK_SIZE feature
        let mut config = [0u8; 28];
        config[..8].copy_from_slice(&inner.capacity.to_le_bytes());
        if inner.sector_size != 512 {
            config[0x18..0x1C].copy_from_slice(&inner.sector_size.to_le_bytes());
        }
        let off = offset as usize;

        if off >= config.len() {
            return 0;
        }

        let mut result = 0u32;
        for i in 0..len.min(4) as usize {
            if off + i < config.len() {
                result |= (config[off + i] as u32) << (i * 8);
            }
        }
        result
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {
        // Block device config is read-only
    }

    fn process_queue(
        &self,
        _queue_idx: u16,
        queue: &mut VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32 {
        self.handle_request(queue, head, physmap)
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().expect("block lock poisoned");
        inner.negotiated_features = 0;
        // Seek back to start on reset
        let _ = inner.file.seek(SeekFrom::Start(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::NamedTempFile;

    fn make_test_block(size: u64) -> (VirtioBlock, NamedTempFile) {
        make_test_block_opts(size, &VirtioBlockOpts::default())
    }

    fn make_test_block_opts(size: u64, opts: &VirtioBlockOpts) -> (VirtioBlock, NamedTempFile) {
        let mut tmp = NamedTempFile::new().unwrap();
        let zeros = vec![0u8; size as usize];
        tmp.write_all(&zeros).unwrap();
        tmp.flush().unwrap();

        let file = tmp.reopen().unwrap();
        let blk = VirtioBlock::new(file, opts).unwrap();
        (blk, tmp)
    }

    #[test]
    fn capacity_calculation() {
        let (blk, _tmp) = make_test_block(512 * 100);
        let inner = blk.inner.lock().unwrap();
        assert_eq!(inner.capacity, 100);
    }

    #[test]
    fn capacity_partial_sector() {
        // 512 * 10 + 100 = 5220 bytes => 10 sectors (truncated)
        let (blk, _tmp) = make_test_block(5220);
        let inner = blk.inner.lock().unwrap();
        assert_eq!(inner.capacity, 10);
    }

    #[test]
    fn device_features_rw() {
        let (blk, _tmp) = make_test_block(512 * 10);
        let features = blk.device_features();
        // FLUSH should be set
        assert_ne!(features & bits::VIRTIO_BLK_F_FLUSH as u32, 0);
        // RO should NOT be set
        assert_eq!(features & bits::VIRTIO_BLK_F_RO as u32, 0);
    }

    #[test]
    fn device_features_readonly() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut f = tmp.reopen().unwrap();
            f.write_all(&[0u8; 512]).unwrap();
        }
        let file = tmp.reopen().unwrap();
        let blk = VirtioBlock::new(file, &VirtioBlockOpts {
            read_only: true,
            ..Default::default()
        }).unwrap();
        let features = blk.device_features();
        assert_ne!(features & bits::VIRTIO_BLK_F_RO as u32, 0);
    }

    #[test]
    fn cfg_read_capacity() {
        let (blk, _tmp) = make_test_block(512 * 100);
        // Low 32 bits of capacity
        assert_eq!(blk.cfg_read(0, 4), 100);
        // High 32 bits
        assert_eq!(blk.cfg_read(4, 4), 0);
    }

    #[test]
    fn cfg_read_unknown_offset() {
        let (blk, _tmp) = make_test_block(512 * 10);
        assert_eq!(blk.cfg_read(8, 4), 0);
    }

    #[test]
    fn reset_clears_features() {
        let (blk, _tmp) = make_test_block(512 * 10);
        blk.set_features(0xFF);
        {
            let inner = blk.inner.lock().unwrap();
            assert_eq!(inner.negotiated_features, 0xFF);
        }
        blk.reset();
        {
            let inner = blk.inner.lock().unwrap();
            assert_eq!(inner.negotiated_features, 0);
        }
    }
}
