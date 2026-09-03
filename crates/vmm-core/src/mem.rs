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
//!
//! # Why every region is mapped twice
//!
//! The kernel gives two ways to reach the pages of a memory segment,
//! and they do not behave the same.
//!
//! A mapping of the guest physical address space (`vm_segmap_space` in
//! `uts/intel/io/vmm/vmm_vm.c`) faults through `segvmm_fault_space`,
//! which holds each page with `vmc_hold` and releases it with
//! `vmp_release`. That release is the one place that sets the dirty bit
//! in the nested page tables, and the dirty bitmap is all that live
//! migration has to find pages the VMM changed.
//!
//! A mapping of the devmem object (`vm_segmap_obj`) faults through
//! `segvmm_fault_obj`, which does a bare `hat_devload` on the raw page
//! frame. The kernel never sees a write through it.
//!
//! So each region keeps both: [`PhysMap::lookup`] hands out the tracked
//! view for device emulation, and [`PhysMap::lookup_untracked`] hands
//! out the devmem view for the boot-image loaders, which run before any
//! vCPU and before a migration can start. This mirrors Propolis, whose
//! `map_guest` and `map_seg` pair this is adapted from.

use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, RwLock};

use crate::common::PAGE_SIZE;
use crate::hdl::VmmHdl;

mod ctx;
mod devmem;
mod mapping;
mod segid;

pub use ctx::MemCtx;
pub use devmem::DevMemSeg;
use mapping::Mapping;
pub use mapping::{Prot, SubMapping};
pub use segid::{SegidAlloc, VM_MAX_MEMSEGS};

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
    /// Mapping of the guest physical address space. A write through
    /// this one is what the kernel marks dirty. See the module comment.
    tracked: Arc<Mapping>,
    /// Mapping of the memory segment itself. Always writable, and
    /// outside the kernel's dirty tracking.
    direct: Arc<Mapping>,
}

impl Region {
    /// The first guest address past this region.
    fn end(&self) -> u64 {
        self.gpa + self.len as u64
    }
}

/// The two host mappings of one region.
struct RegionViews {
    tracked: Arc<Mapping>,
    direct: Arc<Mapping>,
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
        let map = Self::new();
        map.add_region_anon(gpa, len, RegionKind::Ram)?;
        Ok(map)
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
    ///
    /// Both views share one buffer, as they alias the same pages on a
    /// real VM.
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
        self.publish_claim(
            claim,
            -1,
            RegionViews {
                tracked: Arc::clone(&mapping),
                direct: mapping,
            },
        );
        Ok(())
    }

    /// Publish an anonymous region whose two views do not alias.
    ///
    /// Only this makes the two views tell apart without a kernel VM. A
    /// real VM maps the same pages twice, so nothing outside a test may
    /// depend on the views holding different bytes.
    #[cfg(test)]
    fn add_region_split_anon(&self, gpa: u64, len: usize) -> Result<()> {
        let claim = self.claim(
            gpa,
            len,
            RegionKind::Ram,
            Prot::RWX,
            Placement::Firmware,
        )?;
        self.publish_claim(
            claim,
            -1,
            RegionViews {
                tracked: Arc::new(Mapping::anon(len)?),
                direct: Arc::new(Mapping::anon(len)?),
            },
        );
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
        views: RegionViews,
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
                tracked: views.tracked,
                direct: views.direct,
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
    ///
    /// This is the view every device gets. Writes through it are
    /// visible to the kernel's dirty page tracking, which is what live
    /// migration reads. See the module comment.
    pub fn lookup(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        self.lookup_view(gpa, len, |region| &region.tracked)
    }

    /// Look up a range in the mapping of the memory segment itself.
    ///
    /// The kernel does not see writes through this view, so a live
    /// migration would leave the page behind. Only for writes that
    /// finish before any vCPU runs, and for reads, which must not
    /// disturb the dirty bitmap. Propolis calls this pair
    /// `direct_writable_region` and `direct_readable_region`.
    pub fn lookup_untracked(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        self.lookup_view(gpa, len, |region| &region.direct)
    }

    /// Shared body of [`Self::lookup`] and [`Self::lookup_untracked`].
    fn lookup_view(
        &self,
        gpa: u64,
        len: usize,
        pick: fn(&Region) -> &Arc<Mapping>,
    ) -> Option<SubMapping> {
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
                } else if gpa >= r.end() {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;

        let region = &space.regions[idx];

        // Verify the entire requested range falls within this region
        if end > region.end() {
            return None;
        }

        let offset = (gpa - region.gpa) as usize;
        SubMapping::new(pick(region)).subregion(offset, len)
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

        // The tracked mapping comes before the kernel mapping. The
        // kernel faults its pages in one at a time on first use, so a
        // mapping of guest addresses that are not backed yet is legal,
        // and one that cannot be made costs nothing to abandon.
        let tracked = tracked_view(hdl, gpa, len, self.kind, self.guest_prot)?;

        hdl.map_memseg(segid, gpa, len, segoff, self.guest_prot)?;

        // The guest can reach the memory from here on, so a failure has
        // to take the mapping back out. A guest mapping the physical map
        // does not know about would be handed out a second time.
        match devmem_view(hdl, segid, segoff, len) {
            Ok(direct) => {
                // A region the guest cannot write has nothing to track,
                // and the bootrom loader needs `lookup` to stay
                // writable, so it keeps one view.
                let tracked = tracked.unwrap_or_else(|| Arc::clone(&direct));
                map.publish_claim(self, segid, RegionViews { tracked, direct });
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

/// Map the memory segment itself into this process.
///
/// The kernel does not track writes through this mapping. See the
/// module comment.
fn devmem_view(
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

/// Map a slice of the guest physical address space into this process.
///
/// This is the view the kernel tracks. `None` for a region the guest
/// cannot write: there is nothing to track, and the mapping would carry
/// the guest's own read-only protection, which the bootrom loader needs
/// `lookup` not to have.
fn tracked_view(
    hdl: &VmmHdl,
    gpa: u64,
    len: usize,
    kind: RegionKind,
    guest_prot: Prot,
) -> Result<Option<Arc<Mapping>>> {
    if kind != RegionKind::Ram {
        return Ok(None);
    }
    let offset = i64::try_from(gpa).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("guest address {gpa:#x} exceeds i64"),
        )
    })?;
    Ok(Some(Arc::new(Mapping::new(len, guest_prot, hdl, offset)?)))
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

#[cfg(test)]
mod tests;
