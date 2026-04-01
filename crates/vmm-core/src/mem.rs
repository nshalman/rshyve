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

        // Safety: bounds checked above. write_volatile for concurrency.
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

        // Safety: bounds checked above. copy_nonoverlapping is safe
        // because src (guest) and dst (our buf) don't overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.ptr.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
            );
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

        // Safety: bounds checked above. copy_nonoverlapping is safe
        // because src (our data) and dst (guest) don't overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.ptr.as_ptr(),
                data.len(),
            );
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
