// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO entropy (RNG) device.
//!
//! Provides cryptographically secure random bytes to the guest via a
//! single virtqueue. The guest submits writable buffers; we fill them
//! from the host's entropy source and return them.
//!
//! This is a security-critical device: guest entropy quality depends
//! entirely on the host source. We use `getrandom(2)` which draws
//! from the kernel CSPRNG.

use vmm_core::mem::PhysMap;

use super::queue::VirtQueue;
use super::VirtioDevice;
use vmm_devices::Lifecycle;

/// VirtIO entropy device (type 4).
pub struct VirtioRng;

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioRng {
    pub fn new() -> Self {
        Self
    }
}

impl VirtioDevice for VirtioRng {
    fn device_features(&self) -> u64 {
        // Ring features: legacy transport strips these in pci.rs.
        super::bits::VIRTIO_F_RING_EVENT_IDX
            | super::bits::VIRTIO_F_RING_INDIRECT_DESC
    }

    fn set_features(&self, _features: u64) {}

    fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
        0 // No device-specific config
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        queue: &mut VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32 {
        // Walk the descriptor chain, fill all writable buffers with
        // random bytes from the host CSPRNG.
        let chain = match queue.collect_chain(physmap, head) {
            Some(c) => c,
            None => return 0,
        };

        let mut total_written = 0u32;

        for buf in &chain {
            if let super::queue::ChainBuf::Writable { addr, len } = buf {
                let len = *len as usize;
                // Fill in chunks to avoid large stack allocations
                const CHUNK: usize = 4096;
                let mut remaining = len;
                let mut offset = 0u64;

                while remaining > 0 {
                    let n = remaining.min(CHUNK);
                    let mut rand_buf = vec![0u8; n];

                    // getrandom(2) — blocks until entropy is available,
                    // never fails for reasonable buffer sizes.
                    getrandom(&mut rand_buf);

                    if let Some(sub) = physmap.lookup(*addr + offset, n) {
                        // write to guest memory; no recovery if unmapped
                        let _ = sub.write_bytes(&rand_buf);
                    }

                    offset += n as u64;
                    remaining -= n;
                    total_written = total_written.saturating_add(n as u32);
                }
            }
        }

        total_written
    }

    fn reset(&self) {}
}

impl Lifecycle for VirtioRng {
    fn type_name(&self) -> &'static str {
        "virtio-rng"
    }
}

/// Fill a buffer with cryptographically secure random bytes.
///
/// Uses `getrandom(2)` on illumos, `getentropy(2)` on macOS.
/// Retries on partial reads and EINTR.
#[cfg(target_os = "illumos")]
fn getrandom(buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        let ret = unsafe {
            libc::getrandom(
                buf[filled..].as_mut_ptr().cast(),
                buf.len() - filled,
                0,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            panic!("getrandom failed: {err}");
        }
        filled += ret as usize;
    }
}

#[cfg(not(target_os = "illumos"))]
fn getrandom(buf: &mut [u8]) {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")
        .expect("failed to open /dev/urandom");
    f.read_exact(buf).expect("failed to read from /dev/urandom");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_include_event_idx_and_indirect() {
        use crate::bits;
        let rng = VirtioRng::new();
        let f = rng.device_features();
        assert_ne!(f & bits::VIRTIO_F_RING_EVENT_IDX, 0);
        assert_ne!(f & bits::VIRTIO_F_RING_INDIRECT_DESC, 0);
    }

    #[test]
    fn config_read_returns_zero() {
        let rng = VirtioRng::new();
        assert_eq!(rng.cfg_read(0, 4), 0);
        assert_eq!(rng.cfg_read(4, 1), 0);
    }

    #[test]
    fn getrandom_fills_buffer() {
        let mut buf = [0u8; 64];
        getrandom(&mut buf);
        // Extremely unlikely that 64 random bytes are all zero
        assert_ne!(buf, [0u8; 64]);
    }
}
