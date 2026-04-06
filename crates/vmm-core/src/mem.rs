// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest physical memory management.
//!
//! # Safety model
//!
//! Guest memory is concurrently accessible by vCPUs and potentially by
//! DMA-capable devices. Therefore, we must never form Rust references
//! (`&` or `&mut`) to guest memory — doing so would violate Rust's
//! aliasing rules since vCPUs may read or write at any time.
//!
//! Instead, we use [`Mapping`] (an owned mmap region) and [`SubMapping`]
//! (a borrowed view) which operate exclusively through raw pointer
//! operations (`read_volatile`, `write_volatile`, `copy_nonoverlapping`).
//!
//! The lifetime of a [`SubMapping`] is bounded by the [`Mapping`] it
//! borrows from, preventing use-after-unmap.

use std::io::{Error, ErrorKind, Result};
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::Arc;

use bitflags::bitflags;

use crate::hdl::VmmHdl;

bitflags! {
    /// Memory protection flags.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct Prot: i32 {
        const READ  = libc::PROT_READ;
        const WRITE = libc::PROT_WRITE;
        const EXEC  = libc::PROT_EXEC;
        const RW    = libc::PROT_READ | libc::PROT_WRITE;
        const RWX   = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
    }
}

/// An owned region of mapped memory.
///
/// Wraps an mmap'd region with protection metadata. The mapping is
/// unmapped on drop. No Rust references to the underlying memory are
/// ever created — all access goes through raw pointer operations.
///
/// # Safety
///
/// The `Send` and `Sync` impls are safe because:
/// - The pointer is obtained from `mmap()` and remains valid until `drop()`.
/// - The API never exposes raw pointers or references to callers.
/// - All reads/writes go through `SubMapping` which enforces bounds
///   and protection checks.
struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
    prot: Prot,
}

// Safety: See doc comment on Mapping. The pointer is stable (from mmap),
// and we never create references to the underlying data.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Create a new memory mapping from a VMM file descriptor.
    ///
    /// # Arguments
    /// - `size`: Region size in bytes (must be page-aligned).
    /// - `prot`: Requested protection flags.
    /// - `hdl`: VMM handle providing the file descriptor.
    /// - `offset`: Offset within the VMM fd to map.
    fn new(size: usize, prot: Prot, hdl: &VmmHdl, offset: i64) -> Result<Self> {
        assert!(size > 0, "mapping size must be non-zero");

        // Never map with EXEC into our process — only the guest sees EXEC.
        let mmap_prot = prot.intersection(Prot::RW);

        // Safety: NULL addr lets the OS choose a non-conflicting location.
        // The caller must ensure the VMM fd and offset are valid and that
        // the underlying segment outlives this Mapping.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                mmap_prot.bits(),
                libc::MAP_SHARED,
                hdl.as_raw_fd(),
                offset,
            )
        };

        if raw == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }

        let ptr =
            NonNull::new(raw as *mut u8).expect("mmap returned non-NULL");

        Ok(Self { ptr, len: size, prot })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // Safety: ptr and len were set by a successful mmap() call.
        unsafe {
            libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.len);
        }
    }
}

/// A borrowed, bounds-checked view into a [`Mapping`].
///
/// Provides safe read/write operations on guest memory without ever
/// creating Rust references to that memory.
///
/// The `SubMapping` holds an `Arc<Mapping>`, which keeps the mmap
/// alive for as long as any SubMapping exists. This is safe because
/// the Mapping's Drop (munmap) only runs when all Arc references
/// are dropped.
///
/// All reads use `read_volatile` and all writes use `write_volatile`
/// because guest vCPUs may concurrently access the same memory.
pub struct SubMapping {
    /// Shared ownership of the backing Mapping, preventing munmap
    /// while this SubMapping exists.
    _backing: Arc<Mapping>,
    ptr: NonNull<u8>,
    len: usize,
    prot: Prot,
}

impl SubMapping {
    fn new(mapping: &Arc<Mapping>) -> Self {
        Self {
            _backing: Arc::clone(mapping),
            ptr: mapping.ptr,
            len: mapping.len,
            prot: mapping.prot,
        }
    }

    /// Get the raw host pointer to the start of this mapping.
    ///
    /// # Safety
    ///
    /// The returned pointer aliases guest memory that vCPUs access
    /// concurrently. It must only be used as a target for raw I/O
    /// operations (`pread`, `pwrite`, `preadv`, `pwritev`) — never
    /// dereferenced as a Rust reference. The caller must keep this
    /// `SubMapping` alive for the duration of the I/O.
    #[inline]
    pub unsafe fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Create a sub-region within this mapping.
    ///
    /// Returns `None` if `offset + len` exceeds the mapping bounds.
    pub fn subregion(&self, offset: usize, len: usize) -> Option<SubMapping> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }

        // Safety: offset is within bounds (checked above), and the
        // Arc<Mapping> keeps the mmap alive.
        let ptr = unsafe { self.ptr.as_ptr().add(offset) };
        Some(SubMapping {
            _backing: Arc::clone(&self._backing),
            ptr: NonNull::new(ptr).expect("offset from NonNull is non-null"),
            len,
            prot: self.prot,
        })
    }

    /// Read a value of type T from the start of this mapping.
    ///
    /// Uses `read_volatile` because guest memory is concurrently mutable.
    pub fn read<T: Copy>(&self) -> Result<T> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not readable",
            ));
        }
        if self.len < std::mem::size_of::<T>() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping too small for requested type",
            ));
        }
        if (self.ptr.as_ptr() as usize) % std::mem::align_of::<T>() != 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping not aligned for requested type",
            ));
        }

        // Safety: bounds and alignment checked above. We use
        // read_volatile because guest vCPUs may write concurrently.
        let val = unsafe {
            std::ptr::read_volatile(self.ptr.as_ptr() as *const T)
        };
        Ok(val)
    }

    /// Write a value of type T to the start of this mapping.
    ///
    /// Uses `write_volatile` because guest memory is concurrently readable.
    pub fn write<T: Copy>(&self, val: &T) -> Result<()> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not writable",
            ));
        }
        if self.len < std::mem::size_of::<T>() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping too small for requested type",
            ));
        }
        if (self.ptr.as_ptr() as usize) % std::mem::align_of::<T>() != 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping not aligned for requested type",
            ));
        }

        // Safety: bounds and alignment checked above. write_volatile
        // for concurrency.
        unsafe {
            std::ptr::write_volatile(self.ptr.as_ptr() as *mut T, *val);
        }
        Ok(())
    }

    /// Read bytes from this mapping into a buffer.
    pub fn read_bytes(&self, buf: &mut [u8]) -> Result<()> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not readable",
            ));
        }
        if buf.len() > self.len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "read exceeds mapping bounds",
            ));
        }

        // Safety: bounds checked above. We use volatile reads because
        // guest memory is concurrently mutable by vCPUs; non-volatile
        // copies may be optimized in ways that miss concurrent writes.
        let src = self.ptr.as_ptr();
        let n = buf.len();
        for i in 0..n {
            unsafe {
                buf[i] = std::ptr::read_volatile(src.add(i));
            }
        }
        Ok(())
    }

    /// Write bytes from a buffer into this mapping.
    pub fn write_bytes(&self, data: &[u8]) -> Result<()> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not writable",
            ));
        }
        if data.len() > self.len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "write exceeds mapping bounds",
            ));
        }

        // Safety: bounds checked above. We use volatile writes because
        // guest memory is concurrently readable by vCPUs; non-volatile
        // copies may be reordered or elided by the optimizer.
        let dst = self.ptr.as_ptr();
        let n = data.len();
        for i in 0..n {
            unsafe {
                std::ptr::write_volatile(dst.add(i), data[i]);
            }
        }
        Ok(())
    }

    /// Length of this mapping in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether this mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Type of a guest physical memory region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Read/write/execute RAM.
    Ram,
    /// Read/execute ROM (e.g., bootrom).
    Rom,
    /// MMIO reservation (no backing memory).
    Mmio,
}

/// A region in the guest physical address space.
struct Region {
    gpa: u64,
    len: usize,
    kind: RegionKind,
    /// Kernel segment ID, used for migration state export.
    #[allow(dead_code)]
    segid: i32,
    /// Full R/W mapping of the memory segment, used by the VMM for
    /// device emulation and migration.
    mapping: Arc<Mapping>,
}

/// Manages the guest physical address space.
///
/// Tracks all memory regions (RAM, ROM, MMIO) and provides GPA-to-mapping
/// translation. Regions are non-overlapping and sorted by GPA.
pub struct PhysMap {
    regions: Vec<Region>,
    next_segid: i32,
}

impl PhysMap {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            next_segid: 0,
        }
    }

    /// Add a RAM region backed by a new bhyve memory segment.
    pub fn add_ram(
        &mut self,
        hdl: &VmmHdl,
        gpa: u64,
        len: usize,
    ) -> Result<()> {
        self.add_region(hdl, gpa, len, RegionKind::Ram, Prot::RWX)
    }

    /// Add a ROM region backed by a new bhyve memory segment.
    pub fn add_rom(
        &mut self,
        hdl: &VmmHdl,
        gpa: u64,
        len: usize,
    ) -> Result<()> {
        self.add_region(hdl, gpa, len, RegionKind::Rom, Prot::READ | Prot::EXEC)
    }

    fn add_region(
        &mut self,
        hdl: &VmmHdl,
        gpa: u64,
        len: usize,
        kind: RegionKind,
        guest_prot: Prot,
    ) -> Result<()> {
        if len == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "region length must be non-zero",
            ));
        }

        // Check for overlap with existing regions
        let end = gpa.checked_add(len as u64).ok_or_else(|| {
            Error::new(ErrorKind::InvalidInput, "region overflows address space")
        })?;
        for r in &self.regions {
            let r_end = r.gpa + r.len as u64;
            if gpa < r_end && end > r.gpa {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    format!(
                        "region [{:#x}, {:#x}) overlaps with existing \
                         [{:#x}, {:#x})",
                        gpa, end, r.gpa, r_end,
                    ),
                ));
            }
        }

        let segid = self.next_segid;
        let seg_name = format!("seg{}", segid);

        // Allocate segment in kernel
        hdl.create_memseg(segid, len, &seg_name)?;

        // Map into guest physical address space
        hdl.map_memseg(segid, gpa, len, 0, guest_prot)?;

        self.next_segid += 1;

        // Map into our process for device emulation access (always R/W)
        let offset = hdl.devmem_offset(segid)?;
        let mapping = Mapping::new(len, Prot::RW, hdl, offset)?;

        // Insert sorted by GPA
        let insert_idx = self
            .regions
            .binary_search_by_key(&gpa, |r| r.gpa)
            .unwrap_or_else(|idx| idx);

        self.regions.insert(insert_idx, Region {
            gpa,
            len,
            kind,
            segid,
            mapping: Arc::new(mapping),
        });

        Ok(())
    }

    /// Look up the region containing a guest physical address and return
    /// a lifetime-bounded [`SubMapping`] into it.
    ///
    /// The returned SubMapping is offset to start at `gpa` with length
    /// `len`. Returns `None` if no region contains the full range.
    pub fn lookup(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        let end = gpa.checked_add(len as u64)?;

        // Binary search for the region containing this GPA
        let idx = self
            .regions
            .binary_search_by(|r| {
                if gpa < r.gpa {
                    std::cmp::Ordering::Greater
                } else if gpa >= r.gpa + r.len as u64 {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;

        let region = &self.regions[idx];
        let r_end = region.gpa + region.len as u64;

        // Verify the entire requested range falls within this region
        if end > r_end {
            return None;
        }

        let offset = (gpa - region.gpa) as usize;
        SubMapping::new(&region.mapping).subregion(offset, len)
    }

    /// Get a summary of all memory regions (for migration/diagnostics).
    pub fn regions(&self) -> Vec<(u64, usize, RegionKind)> {
        self.regions
            .iter()
            .map(|r| (r.gpa, r.len, r.kind))
            .collect()
    }

    /// Total mapped memory in bytes (excluding MMIO reservations).
    pub fn total_memory(&self) -> usize {
        self.regions
            .iter()
            .filter(|r| r.kind != RegionKind::Mmio)
            .map(|r| r.len)
            .sum()
    }

    /// Number of memory regions.
    pub fn num_regions(&self) -> usize {
        self.regions.len()
    }
}

/// Context for accessing guest memory.
///
/// Wraps a shared reference to the PhysMap and provides convenience
/// methods for reading/writing guest physical addresses.
///
/// All memory access is bounds-checked and uses volatile operations
/// appropriate for concurrently-mutable guest memory.
pub struct MemCtx {
    map: Arc<PhysMap>,
}

impl MemCtx {
    pub fn new(map: Arc<PhysMap>) -> Self {
        Self { map }
    }

    /// Read bytes from guest memory at the given GPA.
    ///
    /// Returns an error if the GPA range is not mapped or not readable.
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        let sub = self.map.lookup(gpa, buf.len()).ok_or_else(|| {
            Error::new(
                ErrorKind::AddrNotAvailable,
                format!("GPA {:#x} len {} not mapped", gpa, buf.len()),
            )
        })?;
        sub.read_bytes(buf)
    }

    /// Write bytes to guest memory at the given GPA.
    ///
    /// Returns an error if the GPA range is not mapped or not writable.
    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let sub = self.map.lookup(gpa, data.len()).ok_or_else(|| {
            Error::new(
                ErrorKind::AddrNotAvailable,
                format!("GPA {:#x} len {} not mapped", gpa, data.len()),
            )
        })?;
        sub.write_bytes(data)
    }

    /// Look up a GPA range and return a SubMapping for direct access.
    ///
    /// The returned SubMapping is lifetime-bounded to this MemCtx,
    /// preventing use-after-unmap.
    pub fn lookup(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        self.map.lookup(gpa, len)
    }

    /// Get all memory regions for migration dirty page tracking.
    pub fn regions(&self) -> Vec<(u64, usize, RegionKind)> {
        self.map.regions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test SubMapping backed by anonymous mmap (no VmmHdl needed).
    fn make_test_mapping(size: usize) -> Arc<Mapping> {
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(raw, libc::MAP_FAILED);
        let ptr = NonNull::new(raw as *mut u8).unwrap();
        Arc::new(Mapping {
            ptr,
            len: size,
            prot: Prot::RW,
        })
    }

    #[test]
    fn read_aligned_u32() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Page-aligned pointer can read u32
        let val = sub.read::<u32>();
        assert!(val.is_ok());
    }

    #[test]
    fn read_unaligned_u32_fails() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Offset by 1 byte to create misalignment for u32 (align=4)
        let unaligned = sub.subregion(1, 4).unwrap();
        let result = unaligned.read::<u32>();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn write_unaligned_u32_fails() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        let unaligned = sub.subregion(1, 4).unwrap();
        let result = unaligned.write::<u32>(&42);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn read_aligned_u16_at_offset() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Offset by 2 is still aligned for u16 (align=2)
        let aligned = sub.subregion(2, 2).unwrap();
        assert!(aligned.read::<u16>().is_ok());
    }

    #[test]
    fn read_unaligned_u16_fails() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Offset by 1 is misaligned for u16 (align=2)
        let unaligned = sub.subregion(1, 2).unwrap();
        assert!(unaligned.read::<u16>().is_err());
    }

    #[test]
    fn read_u8_always_aligned() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // u8 has align=1, so any offset works
        let at_offset_1 = sub.subregion(1, 1).unwrap();
        assert!(at_offset_1.read::<u8>().is_ok());
    }

    #[test]
    fn read_write_bytes_ignores_alignment() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        let unaligned = sub.subregion(1, 4).unwrap();
        // read_bytes and write_bytes work at any alignment
        let mut buf = [0u8; 4];
        assert!(unaligned.read_bytes(&mut buf).is_ok());
        assert!(unaligned.write_bytes(&[1, 2, 3, 4]).is_ok());
    }

    #[test]
    fn read_write_roundtrip() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        sub.write::<u32>(&0xDEAD_BEEF).unwrap();
        let val = sub.read::<u32>().unwrap();
        assert_eq!(val, 0xDEAD_BEEF);
    }

    #[test]
    fn subregion_bounds_check() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Exactly at end: should succeed
        assert!(sub.subregion(4092, 4).is_some());
        // Past end: should fail
        assert!(sub.subregion(4093, 4).is_none());
        // Overflow: should fail
        assert!(sub.subregion(usize::MAX, 1).is_none());
    }

    #[test]
    fn read_too_small_fails() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        let tiny = sub.subregion(0, 2).unwrap();
        assert!(tiny.read::<u32>().is_err());
    }

    #[test]
    fn write_u32_too_small_fails() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        let tiny = sub.subregion(0, 2).unwrap();
        let result = tiny.write::<u32>(&0xDEAD);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn read_u16_aligned_roundtrip() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Write at aligned offset, read back
        let region = sub.subregion(0, 2).unwrap();
        region.write::<u16>(&0xBEEF).unwrap();
        let val = region.read::<u16>().unwrap();
        assert_eq!(val, 0xBEEF);
    }

    #[test]
    fn read_u16_at_aligned_offset_roundtrip() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Offset 4 is aligned for u16
        let region = sub.subregion(4, 2).unwrap();
        region.write::<u16>(&0xCAFE).unwrap();
        let val = region.read::<u16>().unwrap();
        assert_eq!(val, 0xCAFE);
    }

    #[test]
    fn write_bytes_read_bytes_roundtrip() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        let data = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
        sub.write_bytes(&data).unwrap();
        let mut buf = [0u8; 8];
        sub.read_bytes(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn write_bytes_read_bytes_at_offset() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Write at an unaligned offset via subregion
        let region = sub.subregion(3, 5).unwrap();
        let data = [1, 2, 3, 4, 5];
        region.write_bytes(&data).unwrap();
        let mut buf = [0u8; 5];
        region.read_bytes(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn read_bytes_exceeds_bounds() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        let tiny = sub.subregion(0, 4).unwrap();
        let mut buf = [0u8; 8];
        let result = tiny.read_bytes(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn write_bytes_exceeds_bounds() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        let tiny = sub.subregion(0, 4).unwrap();
        let result = tiny.write_bytes(&[0u8; 8]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn subregion_exact_end() {
        let mapping = make_test_mapping(256);
        let sub = SubMapping::new(&mapping);
        // Exactly the full mapping
        assert!(sub.subregion(0, 256).is_some());
        // One byte past the end
        assert!(sub.subregion(0, 257).is_none());
        // Last byte
        assert!(sub.subregion(255, 1).is_some());
        // Zero-length at end
        assert!(sub.subregion(256, 0).is_some());
    }

    #[test]
    fn subregion_nested() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        // Create a subregion, then a subregion of that
        let outer = sub.subregion(100, 200).unwrap();
        let inner = outer.subregion(50, 100).unwrap();
        assert_eq!(inner.len(), 100);
        // Inner subregion cannot exceed outer
        assert!(outer.subregion(150, 100).is_none());
    }

    #[test]
    fn read_from_write_only_mapping_fails() {
        // Create a write-only mapping
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(raw, libc::MAP_FAILED);
        let ptr = NonNull::new(raw as *mut u8).unwrap();
        let mapping = Arc::new(Mapping {
            ptr,
            len: 4096,
            prot: Prot::WRITE,
        });
        let sub = SubMapping::new(&mapping);
        let result = sub.read::<u32>();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn write_to_read_only_mapping_fails() {
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(raw, libc::MAP_FAILED);
        let ptr = NonNull::new(raw as *mut u8).unwrap();
        let mapping = Arc::new(Mapping {
            ptr,
            len: 4096,
            prot: Prot::READ,
        });
        let sub = SubMapping::new(&mapping);
        let result = sub.write::<u32>(&42);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn len_and_is_empty() {
        let mapping = make_test_mapping(4096);
        let sub = SubMapping::new(&mapping);
        assert_eq!(sub.len(), 4096);
        assert!(!sub.is_empty());

        let zero = sub.subregion(0, 0).unwrap();
        assert_eq!(zero.len(), 0);
        assert!(zero.is_empty());
    }

    #[test]
    fn phys_map_empty() {
        let map = PhysMap::new();
        assert_eq!(map.num_regions(), 0);
        assert_eq!(map.total_memory(), 0);
        assert!(map.regions().is_empty());
        // Lookup in empty map returns None
        assert!(map.lookup(0, 1).is_none());
        assert!(map.lookup(0x1000, 4096).is_none());
    }
}
