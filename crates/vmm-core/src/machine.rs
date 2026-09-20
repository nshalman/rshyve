// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VM machine builder and aggregate.
//!
//! # Memory layout
//!
//! x86-64 guests use a split memory layout to accommodate the MMIO hole:
//!
//! ```text
//!   0x0000_0000  ┌─────────────────┐
//!                │     Low RAM     │  (0 to lowmem_limit, default 3 GiB)
//!   0xC000_0000  ├─────────────────┤
//!                │   MMIO hole     │  (3 GiB to 4 GiB: PCI BARs, etc.)
//!   0x1_0000_0000├─────────────────┤
//!                │    High RAM     │  (4 GiB+, if mem_size > lowmem_limit)
//!                └─────────────────┘
//! ```

use std::io;
use std::sync::Arc;

use crate::hdl::{CreateOpts, VmmHdl};
use crate::mem::{MemCtx, PhysMap, SegidAlloc, MMIO_HOLE_BASE, MMIO_HOLE_END};
use crate::mmio::MmioBus;
use crate::pio::PioBus;
use crate::vcpu::Vcpu;

/// Most vCPUs one VM can have. The kernel's `VM_MAXCPU`.
pub const VM_MAXCPU: u32 = bhyve_api::VM_MAXCPU as u32;

/// High memory begins at the end of the MMIO hole.
const FOUR_GIB: u64 = MMIO_HOLE_END;

/// Default lowmem limit (below the MMIO hole).
const DEFAULT_LOWMEM_LIMIT: u64 = MMIO_HOLE_BASE;

/// Minimum supported memory size (1 MiB).
const MIN_MEM_SIZE: usize = 1024 * 1024;

/// Maximum supported memory size (1 TiB).
const MAX_MEM_SIZE: usize = 1024 * 1024 * 1024 * 1024;

/// Aggregate VM hardware (finalized, immutable).
///
/// Fields are private — use accessor methods. The Machine is constructed
/// via [`Builder`] -> [`MachineSetup`] -> [`Machine`].
pub struct Machine {
    hdl: Arc<VmmHdl>,
    map: Arc<PhysMap>,
    memctx: MemCtx,
    vcpus: Vec<Vcpu>,
    num_cpus: u32,
    mem_size: usize,
    bus_pio: Arc<PioBus>,
    bus_mmio: Arc<MmioBus>,
    segids: Arc<SegidAlloc>,
}

impl Machine {
    pub fn hdl(&self) -> &Arc<VmmHdl> {
        &self.hdl
    }

    pub fn memctx(&self) -> &MemCtx {
        &self.memctx
    }

    pub fn physmap(&self) -> &Arc<PhysMap> {
        &self.map
    }

    pub fn vcpus(&self) -> &[Vcpu] {
        &self.vcpus
    }

    pub fn num_cpus(&self) -> u32 {
        self.num_cpus
    }

    pub fn mem_size(&self) -> usize {
        self.mem_size
    }

    pub fn total_mapped_memory(&self) -> usize {
        self.map.total_memory()
    }

    pub fn bus_pio(&self) -> &Arc<PioBus> {
        &self.bus_pio
    }

    pub fn bus_mmio(&self) -> &Arc<MmioBus> {
        &self.bus_mmio
    }

    pub fn segids(&self) -> &Arc<SegidAlloc> {
        &self.segids
    }
}

/// Intermediate state: VM created with RAM, but not yet finalized.
///
/// Provides mutable access to the `PhysMap` so callers can add ROM
/// regions (bootrom) and other memory segments before the VM starts.
/// Call [`finalize`](MachineSetup::finalize) to produce the final
/// [`Machine`].
pub struct MachineSetup {
    hdl: Arc<VmmHdl>,
    map: PhysMap,
    num_cpus: u32,
    mem_size: usize,
}

impl MachineSetup {
    /// Get a reference to the VM handle.
    pub fn hdl(&self) -> &Arc<VmmHdl> {
        &self.hdl
    }

    /// Get a mutable reference to the physical memory map.
    ///
    /// Use this to add ROM regions (e.g., bootrom) before finalizing.
    pub fn map_mut(&mut self) -> &mut PhysMap {
        &mut self.map
    }

    /// Finalize the setup, producing an immutable [`Machine`].
    ///
    /// After this, the PhysMap is wrapped in Arc and no further
    /// memory regions can be added.
    pub fn finalize(self) -> Machine {
        // Shared, not copied: a second allocator would hand out
        // duplicate segment IDs.
        let segids = Arc::new(SegidAlloc::new(self.map.next_segid()));
        let map = Arc::new(self.map);
        let memctx = MemCtx::new(map.clone());

        let vcpus: Vec<Vcpu> = (0..self.num_cpus)
            .map(|i| Vcpu::new(i as i32, self.hdl.clone()))
            .collect();

        let bus_pio = Arc::new(PioBus::new());
        let bus_mmio = Arc::new(MmioBus::new());

        Machine {
            hdl: self.hdl,
            map,
            memctx,
            vcpus,
            num_cpus: self.num_cpus,
            mem_size: self.mem_size,
            bus_pio,
            bus_mmio,
            segids,
        }
    }
}

/// Builder for constructing a [`Machine`].
///
/// Validates all inputs before creating kernel resources.
pub struct Builder {
    name: String,
    opts: CreateOpts,
    num_cpus: u32,
    mem_size: usize,
    lowmem_limit: u64,
}

impl Builder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            opts: CreateOpts::default(),
            num_cpus: 1,
            mem_size: 256 * 1024 * 1024,
            lowmem_limit: DEFAULT_LOWMEM_LIMIT,
        }
    }

    pub fn opts(mut self, opts: CreateOpts) -> Self {
        self.opts = opts;
        self
    }

    pub fn cpus(mut self, count: u32) -> Self {
        self.num_cpus = count;
        self
    }

    pub fn memory(mut self, size: usize) -> Self {
        self.mem_size = size;
        self
    }

    pub fn lowmem_limit(mut self, limit: u64) -> Self {
        self.lowmem_limit = limit;
        self
    }

    /// Build the VM, creating kernel resources and RAM regions.
    ///
    /// Returns a [`MachineSetup`] with mutable PhysMap access for
    /// adding ROM and other regions before finalization.
    pub fn build(self) -> anyhow::Result<MachineSetup> {
        anyhow::ensure!(
            self.num_cpus > 0 && self.num_cpus <= VM_MAXCPU,
            "num_cpus must be 1..={}, got {}",
            VM_MAXCPU,
            self.num_cpus,
        );
        anyhow::ensure!(
            self.mem_size >= MIN_MEM_SIZE,
            "mem_size must be >= {} bytes (1 MiB), got {}",
            MIN_MEM_SIZE,
            self.mem_size,
        );
        anyhow::ensure!(
            self.mem_size <= MAX_MEM_SIZE,
            "mem_size must be <= {} bytes (1 TiB), got {}",
            MAX_MEM_SIZE,
            self.mem_size,
        );
        anyhow::ensure!(
            self.lowmem_limit > 0 && self.lowmem_limit <= FOUR_GIB,
            "lowmem_limit must be (0, 4 GiB], got {:#x}",
            self.lowmem_limit,
        );

        let hdl = Arc::new(VmmHdl::create(&self.name, &self.opts)?);
        // Arm the kernel reclaimer before any later step can fail. It is the
        // only thing that releases the instance if the process dies without
        // teardown.
        arm_or_destroy(|| hdl.set_autodestruct(true), || hdl.destroy())?;

        let mut map = PhysMap::new();

        // Low memory: 0 to min(mem_size, lowmem_limit)
        let lowmem_size = self.mem_size.min(self.lowmem_limit as usize);
        map.add_ram(&hdl, 0, lowmem_size)?;

        // High memory: above 4 GiB if mem_size exceeds lowmem_limit
        if self.mem_size > self.lowmem_limit as usize {
            let highmem_size = self.mem_size - self.lowmem_limit as usize;
            map.add_ram(&hdl, FOUR_GIB, highmem_size)?;
        }

        Ok(MachineSetup {
            hdl,
            map,
            num_cpus: self.num_cpus,
            mem_size: self.mem_size,
        })
    }
}

/// Arm the kernel reclaimer, destroying the instance if it cannot be.
///
/// `VMM_CREATE_VM` has already published `/dev/vmm/<name>`. Dropping the
/// handle unarmed leaves an instance no fd owns and no reclaimer will
/// take, so the next start of the same name fails `EEXIST` until an
/// operator destroys it by hand. `VmmHdl::create` does the same for the
/// analogous failed open.
///
/// Both errors are kept: the arming failure says why the instance had to
/// go, and a failed destroy says the operator still has one to clear.
fn arm_or_destroy(
    arm: impl FnOnce() -> io::Result<()>,
    destroy: impl FnOnce() -> io::Result<()>,
) -> anyhow::Result<()> {
    let Err(refused) = arm() else {
        return Ok(());
    };
    match destroy() {
        Ok(()) => Err(anyhow::Error::new(refused)
            .context("failed to arm VM autodestruct")),
        Err(lost) => Err(anyhow::Error::new(refused).context(format!(
            "failed to arm VM autodestruct, and the unowned instance \
             could not be destroyed either: {lost}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn an_armed_reclaimer_destroys_nothing() {
        let destroyed = Cell::new(false);

        arm_or_destroy(
            || Ok(()),
            || {
                destroyed.set(true);
                Ok(())
            },
        )
        .expect("arming succeeded");

        assert!(!destroyed.get());
    }

    /// An instance left unarmed is reclaimed by nothing, so it has to be
    /// destroyed here or an operator clears it by hand.
    #[test]
    fn a_refused_arming_destroys_the_instance() {
        let destroyed = Cell::new(false);

        let error = arm_or_destroy(
            || Err(io::Error::from_raw_os_error(libc::EPERM)),
            || {
                destroyed.set(true);
                Ok(())
            },
        )
        .expect_err("arming failed");

        assert!(destroyed.get(), "the unowned instance must be destroyed");
        assert!(format!("{error:#}").contains("autodestruct"), "{error:#}",);
    }

    #[test]
    fn a_destroy_that_also_fails_reports_both() {
        let error = arm_or_destroy(
            || Err(io::Error::from_raw_os_error(libc::EPERM)),
            || Err(io::Error::from_raw_os_error(libc::EBUSY)),
        )
        .expect_err("both failed");

        let text = format!("{error:#}");
        assert!(text.contains("autodestruct"), "{text}");
        assert!(text.contains("could not be destroyed"), "{text}");
    }
}
