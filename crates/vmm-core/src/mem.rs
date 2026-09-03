// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest physical memory management.
//!
//! # Safety model
//!
//! Guest memory is concurrently accessible by vCPUs and potentially by
//! DMA-capable devices. Therefore, we must never form Rust references
//! (`&` or `&mut`) to guest memory. That would violate Rust's
//! aliasing rules since vCPUs may read or write at any time.
//!
//! Instead, we use [`Mapping`] (an owned mmap region) and [`SubMapping`]
//! (a borrowed view) which operate exclusively through raw pointer
//! operations (`read_volatile`, `write_volatile`, `copy_nonoverlapping`).
//!
//! The lifetime of a [`SubMapping`] is bounded by the `Mapping` it
//! borrows from, preventing use-after-unmap.

use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, RwLock};

use crate::common::PAGE_SIZE;
use crate::hdl::VmmHdl;

mod mapping;

use mapping::Mapping;
pub use mapping::{Prot, SubMapping};

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

/// Start of the 32-bit MMIO hole.
///
/// This is the default lowmem limit in `machine.rs`. Everything from
/// here to 4 GiB belongs to PCI BARs, the APICs and the firmware flash
/// window, so no RAM may be placed in it at run time.
pub const MMIO_HOLE_BASE: u64 = 0xC000_0000;

/// End of the 32-bit MMIO hole, which is the 4 GiB line.
pub const MMIO_HOLE_END: u64 = 0x1_0000_0000;

/// Where a claim is allowed to sit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Boot time. The 32-bit hole is legal, because the boot ROM, its
    /// variable store and the firmware scratch RAM all live there.
    Firmware,
    /// Run time. RAM only, so the hole stays clear for the PCI BARs,
    /// the APICs and the flash window.
    Ram,
}

/// What is published in the guest physical address space, and what is
/// spoken for by an add that has not finished.
#[derive(Default)]
struct AddressSpace {
    /// Published regions, sorted by GPA.
    regions: Vec<Region>,
    /// `[gpa, end)` ranges held by an add still in progress.
    ///
    /// A claim keeps its place while the kernel work runs outside the
    /// lock, so two adds can never map the same guest addresses.
    claims: Vec<(u64, u64)>,
}

impl AddressSpace {
    /// Reject a `[gpa, end)` range that touches anything already here.
    ///
    /// Both the boot path and the run-time path go through this, so
    /// there is one overlap rule and not two.
    fn check_free(&self, gpa: u64, end: u64) -> Result<()> {
        for r in &self.regions {
            let r_end = r.gpa + r.len as u64;
            if gpa < r_end && end > r.gpa {
                return Err(overlaps(gpa, end, r.gpa, r_end, "existing"));
            }
        }
        for &(c_gpa, c_end) in &self.claims {
            if gpa < c_end && end > c_gpa {
                return Err(overlaps(gpa, end, c_gpa, c_end, "claimed"));
            }
        }
        Ok(())
    }
}

/// Manages the guest physical address space.
///
/// Tracks all memory regions (RAM, ROM, MMIO) and provides GPA-to-mapping
/// translation. Regions are non-overlapping and sorted by GPA.
pub struct PhysMap {
    space: RwLock<AddressSpace>,
    next_segid: i32,
}

impl Default for PhysMap {
    fn default() -> Self {
        Self::new()
    }
}

impl PhysMap {
    pub fn new() -> Self {
        Self {
            space: RwLock::new(AddressSpace::default()),
            next_segid: 0,
        }
    }

    /// Device tests need real `PhysMap::lookup` boundaries without creating a
    /// kernel VM, which is unavailable on non-illumos test hosts.
    #[doc(hidden)]
    pub fn new_anon(gpa: u64, len: usize) -> Result<Self> {
        let _ = region_end(gpa, len)?;

        Ok(Self {
            space: RwLock::new(AddressSpace {
                regions: vec![Region {
                    gpa,
                    len,
                    kind: RegionKind::Ram,
                    segid: -1,
                    mapping: Arc::new(Mapping::anon(len)?),
                }],
                claims: Vec::new(),
            }),
            next_segid: 0,
        })
    }

    /// Next unused kernel memory-segment ID.
    pub fn next_segid(&self) -> i32 {
        self.next_segid
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

    /// Add a ROM region and a writable RAM region that share one kernel
    /// segment.
    ///
    /// The RAM comes from the tail of the segment, past the ROM image,
    /// and gets its own guest mapping at `ram_gpa`. The two mappings
    /// cover disjoint parts of the segment, so no byte is visible at
    /// two guest addresses.
    ///
    /// This exists to save a segment ID. The kernel gives one VM only
    /// `VM_MAX_MEMSEGS` of them, and `vm_mmap_memseg` puts no limit on
    /// how many mappings one segment may have.
    pub fn add_rom_with_ram_tail(
        &mut self,
        hdl: &VmmHdl,
        rom_gpa: u64,
        rom_len: usize,
        ram_gpa: u64,
        ram_len: usize,
    ) -> Result<()> {
        let seg_len = rom_len.checked_add(ram_len).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "ROM and RAM tail lengths overflow",
            )
        })?;
        let ram_off = i64::try_from(rom_len).map_err(|_| {
            Error::new(ErrorKind::InvalidInput, "ROM length exceeds i64")
        })?;

        let segid = self.next_segid;
        let mut created = false;
        let result = (|| -> Result<()> {
            // Both ranges are claimed before the segment exists, so a
            // bad layout cannot spend a segment the kernel never gives
            // back.
            let rom = self.claim(
                rom_gpa,
                rom_len,
                RegionKind::Rom,
                Prot::READ | Prot::EXEC,
                Placement::Firmware,
            )?;
            let ram = self.claim(
                ram_gpa,
                ram_len,
                RegionKind::Ram,
                Prot::RWX,
                Placement::Firmware,
            )?;
            hdl.create_memseg(segid, seg_len, &seg_name(segid))?;
            created = true;
            rom.commit(hdl, segid, 0)?;
            ram.commit(hdl, segid, ram_off)
        })();

        if created {
            self.next_segid += 1;
        }
        result
    }

    fn add_region(
        &mut self,
        hdl: &VmmHdl,
        gpa: u64,
        len: usize,
        kind: RegionKind,
        guest_prot: Prot,
    ) -> Result<()> {
        let segid = self.next_segid;
        let mut created = false;
        let result = (|| -> Result<()> {
            let claim =
                self.claim(gpa, len, kind, guest_prot, Placement::Firmware)?;
            hdl.create_memseg(segid, len, &seg_name(segid))?;
            created = true;
            claim.commit(hdl, segid, 0)
        })();

        // There is no ioctl that frees a segment, so one that was
        // created is spent whether or not its mapping worked.
        if created {
            self.next_segid += 1;
        }
        result
    }

    /// Map an already allocated segment as RAM while the guest runs.
    ///
    /// `VM_MMAP_MEMSEG` takes the VM write lock, which drives every
    /// vCPU out with a `VM_EXITCODE_BOGUS` exit and freezes it. The run
    /// loop treats that exit as a plain re-entry, so nothing else has
    /// to stop.
    ///
    /// The caller allocates the segment. `segoff` names the slice of it
    /// to map, so one segment can back several regions.
    pub fn add_ram_runtime(
        &self,
        hdl: &VmmHdl,
        segid: i32,
        gpa: u64,
        segoff: i64,
        len: usize,
    ) -> Result<()> {
        self.claim_ram(gpa, len)?.commit(hdl, segid, segoff)
    }

    /// Hold a range of the guest address space for a run-time RAM add.
    ///
    /// The claim is taken under the write lock and runs the same
    /// overlap rule the boot path uses, so two concurrent adds cannot
    /// both pass. Nothing is allocated yet: drop the claim to give the
    /// range back, or commit it once the segment exists.
    pub fn claim_ram(&self, gpa: u64, len: usize) -> Result<RamClaim<'_>> {
        self.claim(gpa, len, RegionKind::Ram, Prot::RWX, Placement::Ram)
    }

    /// Take a range, or report why it cannot be had.
    fn claim(
        &self,
        gpa: u64,
        len: usize,
        kind: RegionKind,
        guest_prot: Prot,
        placement: Placement,
    ) -> Result<RamClaim<'_>> {
        let end = region_end(gpa, len)?;

        if placement == Placement::Ram {
            // The kernel would answer EINVAL. Say why instead.
            if !gpa.is_multiple_of(PAGE_SIZE as u64)
                || !len.is_multiple_of(PAGE_SIZE)
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("RAM [{gpa:#x}, {end:#x}) is not page aligned"),
                ));
            }
            // The hole holds the PCI BARs, the APICs, the boot ROM and
            // its variable store. RAM placed there would be shadowed by
            // a BAR or would sit under the firmware flash window.
            if gpa < MMIO_HOLE_END && end > MMIO_HOLE_BASE {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "RAM [{gpa:#x}, {end:#x}) runs into the 32-bit \
                         MMIO hole [{MMIO_HOLE_BASE:#x}, \
                         {MMIO_HOLE_END:#x})",
                    ),
                ));
            }
        }

        let mut space = self.write_space();
        space.check_free(gpa, end)?;
        space.claims.push((gpa, end));

        Ok(RamClaim {
            map: self,
            gpa,
            len,
            kind,
            guest_prot,
            live: true,
        })
    }

    /// Give a claimed range back.
    fn release_claim(&self, gpa: u64) {
        let mut space = self.write_space();
        if let Some(idx) = space.claims.iter().position(|&(c, _)| c == gpa) {
            space.claims.swap_remove(idx);
        }
    }

    /// Publish a region backed by anonymous memory.
    ///
    /// Tests on a host with no kernel VM need real `lookup` boundaries.
    /// This is the multi-region form of [`Self::new_anon`].
    #[doc(hidden)]
    pub fn add_region_anon(
        &self,
        gpa: u64,
        len: usize,
        kind: RegionKind,
    ) -> Result<()> {
        let claim =
            self.claim(gpa, len, kind, Prot::RWX, Placement::Firmware)?;
        let mapping = Arc::new(Mapping::anon(len)?);
        self.publish_claim(claim, -1, mapping);
        Ok(())
    }

    /// Turn one claim into a published region, in one write-lock section.
    ///
    /// The range was checked when the claim was taken and the claim has
    /// held it since, so there is nothing left to refuse here. The list
    /// stays sorted by GPA for the binary search in `lookup`.
    fn publish_claim(
        &self,
        mut claim: RamClaim<'_>,
        segid: i32,
        mapping: Arc<Mapping>,
    ) {
        let mut space = self.write_space();
        if let Some(idx) =
            space.claims.iter().position(|&(c, _)| c == claim.gpa)
        {
            space.claims.swap_remove(idx);
        }
        claim.live = false;

        let insert_idx = space
            .regions
            .binary_search_by_key(&claim.gpa, |r| r.gpa)
            .unwrap_or_else(|idx| idx);

        space.regions.insert(
            insert_idx,
            Region {
                gpa: claim.gpa,
                len: claim.len,
                kind: claim.kind,
                segid,
                mapping,
            },
        );
    }

    fn read_space(&self) -> std::sync::RwLockReadGuard<'_, AddressSpace> {
        self.space.read().expect("physmap regions lock poisoned")
    }

    fn write_space(&self) -> std::sync::RwLockWriteGuard<'_, AddressSpace> {
        self.space.write().expect("physmap regions lock poisoned")
    }

    /// Look up the region containing a guest physical address and return
    /// a lifetime-bounded [`SubMapping`] into it.
    ///
    /// The returned SubMapping is offset to start at `gpa` with length
    /// `len`. Returns `None` if no region contains the full range.
    pub fn lookup(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        let end = gpa.checked_add(len as u64)?;

        // The guard is released when this call returns: `SubMapping` owns
        // an `Arc<Mapping>` and borrows nothing from the region list.
        let space = self.read_space();

        // Binary search for the region containing this GPA
        let idx = space
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

        let region = &space.regions[idx];
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
        self.read_space()
            .regions
            .iter()
            .map(|r| (r.gpa, r.len, r.kind))
            .collect()
    }

    /// Total mapped memory in bytes (excluding MMIO reservations).
    pub fn total_memory(&self) -> usize {
        self.read_space()
            .regions
            .iter()
            .filter(|r| r.kind != RegionKind::Mmio)
            .map(|r| r.len)
            .sum()
    }

    /// Number of memory regions.
    pub fn num_regions(&self) -> usize {
        self.read_space().regions.len()
    }
}

/// A held range of the guest physical address space.
///
/// Nothing is allocated for it yet. Drop it to give the range back, or
/// [`commit`](Self::commit) it once the kernel segment exists.
#[must_use = "a dropped claim gives the range back"]
pub struct RamClaim<'a> {
    map: &'a PhysMap,
    gpa: u64,
    len: usize,
    kind: RegionKind,
    guest_prot: Prot,
    live: bool,
}

impl std::fmt::Debug for RamClaim<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RamClaim")
            .field("gpa", &format_args!("{:#x}", self.gpa))
            .field("len", &self.len)
            .field("kind", &self.kind)
            .finish()
    }
}

impl RamClaim<'_> {
    /// The guest address this claim holds.
    pub fn gpa(&self) -> u64 {
        self.gpa
    }

    /// Map `segid` at the claimed address and publish the region.
    ///
    /// `segoff` is the offset into the segment, which lets one segment
    /// back several regions.
    pub fn commit(self, hdl: &VmmHdl, segid: i32, segoff: i64) -> Result<()> {
        if segoff < 0 || !(segoff as u64).is_multiple_of(PAGE_SIZE as u64) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("segment offset {segoff} is not a page offset"),
            ));
        }
        let (map, gpa, len) = (self.map, self.gpa, self.len);

        hdl.map_memseg(segid, gpa, len, segoff, self.guest_prot)?;

        // The guest can reach the memory from here on, so a failure has
        // to take the mapping back out. A guest mapping the physical map
        // does not know about would be handed out a second time.
        match host_view(hdl, segid, segoff, len) {
            Ok(mapping) => {
                map.publish_claim(self, segid, mapping);
                Ok(())
            }
            Err(e) => match hdl.munmap_memseg(gpa, len) {
                Ok(()) => Err(e),
                Err(unmap) => {
                    // The guest still reaches memory with no region
                    // behind it. Keeping the claim for the life of the
                    // VM stops the range being handed out again.
                    std::mem::forget(self);
                    Err(Error::other(format!(
                        "{e}; the guest mapping at {gpa:#x} could not be \
                         taken back ({unmap}), so the range is spent",
                    )))
                }
            },
        }
    }
}

/// Map a region into this process, for device emulation.
fn host_view(
    hdl: &VmmHdl,
    segid: i32,
    segoff: i64,
    len: usize,
) -> Result<Arc<Mapping>> {
    let base = hdl.devmem_offset(segid)?;
    let offset = base.checked_add(segoff).ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, "device memory offset overflows")
    })?;
    Ok(Arc::new(Mapping::new(len, Prot::RW, hdl, offset)?))
}

impl Drop for RamClaim<'_> {
    fn drop(&mut self) {
        if self.live {
            self.map.release_claim(self.gpa);
        }
    }
}

/// The kernel name of a memory segment.
fn seg_name(segid: i32) -> String {
    format!("seg{segid}")
}

/// Validate a region request and return its exclusive end GPA.
fn region_end(gpa: u64, len: usize) -> Result<u64> {
    if len == 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "region length must be non-zero",
        ));
    }
    let len = u64::try_from(len).map_err(|_| {
        Error::new(ErrorKind::InvalidInput, "region length exceeds u64")
    })?;
    gpa.checked_add(len).ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, "region overflows address space")
    })
}

/// The error a range that is already spoken for gets.
fn overlaps(gpa: u64, end: u64, o_gpa: u64, o_end: u64, what: &str) -> Error {
    Error::new(
        ErrorKind::AlreadyExists,
        format!(
            "region [{gpa:#x}, {end:#x}) overlaps with {what} \
             [{o_gpa:#x}, {o_end:#x})",
        ),
    )
}

/// `VM_MAX_MEMSEGS` from illumos `uts/intel/io/vmm/vmm.c`.
pub const VM_MAX_MEMSEGS: i32 = 5;

/// Hands out kernel memory-segment IDs after the [`PhysMap`] is sealed.
///
/// This does not live in `PhysMap` because `MachineSetup::finalize` freezes
/// that map before devices are constructed.
pub struct SegidAlloc {
    next: std::sync::atomic::AtomicI32,
}

impl SegidAlloc {
    /// Start allocating at the next unused segment ID from [`PhysMap`].
    pub fn new(start: i32) -> Self {
        Self {
            next: std::sync::atomic::AtomicI32::new(start),
        }
    }

    /// Give back an ID whose segment was never created.
    ///
    /// Only the ID handed out last can come back, which is all a failed
    /// allocation needs. Reports whether it was taken: another caller
    /// may have allocated in between, and then the ID stays spent.
    ///
    /// Never call this for a segment `VM_ALLOC_MEMSEG` created. The
    /// kernel has no ioctl that frees one, so reusing the ID would
    /// answer `EEXIST` or, worse, hand out memory that is already in
    /// use.
    pub fn release(&self, segid: i32) -> bool {
        let Some(next) = segid.checked_add(1) else {
            return false;
        };
        self.next
            .compare_exchange(
                next,
                segid,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Allocate a segment ID, or return `None` at the kernel limit.
    ///
    /// Reporting exhaustion here gives startup a useful error instead
    /// of an unexplained `EINVAL` from `VM_ALLOC_MEMSEG`.
    pub fn alloc(&self) -> Option<i32> {
        self.next
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |next| {
                    if next < VM_MAX_MEMSEGS {
                        Some(next + 1)
                    } else {
                        None
                    }
                },
            )
            .ok()
    }
}

/// A named bhyve memory segment whose guest mapping is under device control.
///
/// A device BAR backed by this segment is served by EPT without an MMIO exit.
/// This is required for wide stores because the kernel MMIO exit carries only
/// one `u64` and cannot represent a 16-byte framebuffer store.
pub struct DevMemSeg {
    segid: i32,
    len: usize,
    host: Arc<Mapping>,
    hdl: Option<Arc<VmmHdl>>,
    mapped_gpa: std::sync::Mutex<Option<u64>>,
}

impl DevMemSeg {
    /// Allocate the segment and map only its host view.
    pub fn new(
        hdl: Arc<VmmHdl>,
        segid: i32,
        name: &str,
        len: usize,
    ) -> Result<Self> {
        hdl.create_memseg(segid, len, name)?;
        let offset = hdl.devmem_offset(segid)?;
        let host = Arc::new(Mapping::new(len, Prot::RW, &hdl, offset)?);

        Ok(Self {
            segid,
            len,
            host,
            hdl: Some(hdl),
            mapped_gpa: std::sync::Mutex::new(None),
        })
    }

    /// Create anonymous backing without a guest for unit tests.
    ///
    /// [`Self::map_at`] and [`Self::unmap`] are no-ops for this segment.
    #[doc(hidden)]
    pub fn new_anon(len: usize) -> Result<Self> {
        Ok(Self {
            segid: -1,
            len,
            host: Arc::new(Mapping::anon(len)?),
            hdl: None,
            mapped_gpa: std::sync::Mutex::new(None),
        })
    }

    /// Length of the memory segment in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the memory segment is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return a guest-visible view of the host mapping.
    pub fn view(&self) -> SubMapping {
        SubMapping::new(&self.host)
    }

    /// Return the GPA where the segment is currently mapped, if any.
    pub fn mapped_gpa(&self) -> Option<u64> {
        *self
            .mapped_gpa
            .lock()
            .expect("devmem mapping lock poisoned")
    }

    /// Map the segment into the guest physical address space.
    ///
    /// Mapping at the current GPA is idempotent. Kernel mapping errors are
    /// returned because a guest-selected GPA may collide with another region.
    pub fn map_at(&self, gpa: u64) -> Result<()> {
        let Some(hdl) = &self.hdl else {
            return Ok(());
        };

        let mut mapped = self
            .mapped_gpa
            .lock()
            .expect("devmem mapping lock poisoned");
        if *mapped == Some(gpa) {
            return Ok(());
        }

        hdl.map_memseg(self.segid, gpa, self.len, 0, Prot::RW)?;
        *mapped = Some(gpa);
        Ok(())
    }

    /// Remove the segment's current guest mapping, if any.
    ///
    /// The recorded GPA and length are reused because the kernel requires the
    /// pair to match the original mapping exactly.
    pub fn unmap(&self) -> Result<()> {
        let Some(hdl) = &self.hdl else {
            return Ok(());
        };

        let Some(gpa) = self
            .mapped_gpa
            .lock()
            .expect("devmem mapping lock poisoned")
            .take()
        else {
            return Ok(());
        };

        hdl.munmap_memseg(gpa, self.len)
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

    /// Bulk write to guest memory at the given GPA.
    ///
    /// Uses [`SubMapping::copy_in`], so it carries that method's
    /// constraint: only for writes no vCPU can observe partially. Meant
    /// for the multi-megabyte boot-path writes, which all happen before
    /// the vCPU threads start.
    pub fn write_bulk(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let sub = self.map.lookup(gpa, data.len()).ok_or_else(|| {
            Error::new(
                ErrorKind::AddrNotAvailable,
                format!("GPA {:#x} len {} not mapped", gpa, data.len()),
            )
        })?;
        sub.copy_in(data)
    }

    /// Load `len` bytes at `file_offset` in `file` into guest memory at
    /// `gpa`.
    ///
    /// The GPA range is validated as mapped and writable before any
    /// byte is read, so a segment header cannot steer the read outside
    /// guest RAM. Uses [`SubMapping::read_exact_from`] and carries its
    /// constraint: only before the vCPU threads start.
    pub fn load_from_file(
        &self,
        gpa: u64,
        file: &std::fs::File,
        file_offset: u64,
        len: usize,
    ) -> Result<()> {
        let sub = self.map.lookup(gpa, len).ok_or_else(|| {
            Error::new(
                ErrorKind::AddrNotAvailable,
                format!("GPA {gpa:#x} len {len} not mapped"),
            )
        })?;
        sub.read_exact_from(file, file_offset, len)
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
mod tests;
