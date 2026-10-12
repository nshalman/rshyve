// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared virtio device construction for the binaries' device catalogs.
//!
//! Both binaries build the same three virtio devices; only the set of
//! accepted driver names differs. The construction lives here so the
//! viona interrupt back-wire, which must happen after the transport
//! exists, has exactly one implementation.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;

use vmm_devices::pci::msix::{HdlMsiSink, MsiSink, MsixTable};
use vmm_devices::pci::LintrCfg;

use super::bits;
use super::block::{VirtioBlock, VirtioBlockOpts};
use super::console::{
    VirtioConsole, CONSOLE_MSIX_VECTORS, CONSOLE_NUM_QUEUES, CONSOLE_QUEUE_SIZE,
};
use super::fs::{
    VirtioFs, VirtioFsOpts, FS_CONFIG_SIZE, FS_MSIX_VECTORS, FS_NUM_QUEUES,
};
use super::pci::VirtioPciDevice;
use super::rng::VirtioRng;
use super::viona;
use super::viona::VirtioViona;

/// Machine-owned handles every virtio device needs. Field types match
/// `PciDeviceCtx`'s so a catalog builds one by plain reference borrow.
pub struct VirtioAttachCtx<'a> {
    pub physmap: &'a Arc<PhysMap>,
    pub bus_pio: &'a Arc<PioBus>,
    pub bus_mmio: &'a Arc<MmioBus>,
    pub hdl: &'a Arc<VmmHdl>,
}

impl VirtioAttachCtx<'_> {
    /// The MSI-X delivery path for a device built from this context.
    ///
    /// The raw handle stays in the context because viona needs the VM
    /// file descriptor. Every other use of it is this sink.
    pub fn msi_sink(&self) -> Arc<dyn MsiSink> {
        HdlMsiSink::new(Arc::clone(self.hdl))
    }
}

/// Build a virtio-blk device and its PCI transport.
///
/// The caller routes the INTx pin before this call and attaches the
/// returned device to the chipset after it, so this function names no
/// chipset type.
///
/// `defer_open` is for a migration destination: the disk is opened only
/// when the device attaches its backing store, after the commit.
pub fn virtio_blk(
    path: &Path,
    opts: &VirtioBlockOpts,
    num_vcpus: u32,
    lintr: Option<LintrCfg>,
    defer_open: bool,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioBlock>>> {
    let log = log.new(slog::o!("path" => path.display().to_string()));
    let disk = vmm_devices::blkdev::BackingDisk::new(
        path,
        opts.read_only,
        defer_open,
        &log,
    )
    .with_context(|| format!("failed to open disk: {}", path.display()))?;

    // A zvol has one SPA sync pipeline, so beyond ~4 queues there is no
    // throughput gain.
    let num_queues: u16 = opts
        .num_queues
        .unwrap_or_else(|| std::cmp::min(num_vcpus as u16, 4).max(1));

    let blk =
        VirtioBlock::with_disk(disk, opts, Arc::clone(ctx.physmap), num_queues)
            .with_context(|| {
                format!("failed to create virtio-blk for: {}", path.display())
            })?;

    let cfg_size = blk.config_size();
    // One MSI-X vector per queue, plus one for config change.
    let msix = Arc::new(MsixTable::new(num_queues + 1, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        blk,
        bits::VIRTIO_DEV_TYPE_BLOCK,
        num_queues as usize,
        128,
        cfg_size,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Workers raise through the transport so the interrupt takes the
    // MSI-X vector when the guest enabled it, and INTx otherwise.
    pci_dev.device().install_interrupt(pci_dev.backend_intr());
    // A worker drains a ring it left chains in for want of budget.
    pci_dev.device().install_kick(pci_dev.backend_kick());

    Ok(pci_dev)
}

/// Build a viona device and its PCI transport on `vnic_name`.
///
/// `defer_poll` holds the in-kernel interrupt poll thread back until ring
/// state is restored; a binary with no migration path passes `false`.
pub fn virtio_viona(
    vnic_name: &str,
    promiscphys: bool,
    lintr: Option<LintrCfg>,
    defer_poll: bool,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioViona>>> {
    anyhow::ensure!(
        !vnic_name.is_empty(),
        "virtio-net-viona requires a VNIC name"
    );

    use std::os::fd::AsFd;
    let dev = VirtioViona::new(
        vnic_name,
        ctx.hdl.as_fd(),
        Arc::clone(ctx.hdl.rings()),
    )
    .with_context(|| {
        format!("failed to create viona device for VNIC: {vnic_name}")
    })?;

    if promiscphys {
        dev.set_promisc(true).with_context(|| {
            format!("failed to set promiscphys on {vnic_name}")
        })?;
    }

    // One MSI-X vector per queue (rx, tx), plus one for config change.
    let msix = Arc::new(MsixTable::new(
        viona::NET_NUM_QUEUES as u16 + 1,
        ctx.msi_sink(),
    ));

    // The poll thread reads the interrupt slot on every wakeup, so it
    // can start before the transport exists.
    let intr_log = log.new(slog::o!("component" => "viona-intr"));
    if defer_poll {
        dev.defer_intr_poll(intr_log);
    } else {
        dev.start_intr_poll(intr_log);
    }

    let pci_dev = VirtioPciDevice::new(
        dev,
        bits::VIRTIO_DEV_TYPE_NET,
        viona::NET_NUM_QUEUES,
        viona::NET_QUEUE_SIZE,
        viona::NET_CONFIG_SIZE,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok(pci_dev)
}

/// Build a virtio-console device and its PCI transport from a socket
/// path.
///
/// Ordering is load-bearing: the interrupt slot is cloned before the
/// device moves into the transport, and the `Weak` back-reference is
/// filled in after `VirtioPciDevice::new` returns.
pub fn virtio_console(
    socket_path: &Path,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioConsole>>> {
    let console =
        VirtioConsole::new(socket_path, Arc::clone(ctx.physmap), log.clone())?;
    let cfg_size = console.config_size();

    // One vector per queue, plus one for config change.
    let msix = Arc::new(MsixTable::new(CONSOLE_MSIX_VECTORS, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        console,
        bits::VIRTIO_DEV_TYPE_CONSOLE,
        CONSOLE_NUM_QUEUES,
        CONSOLE_QUEUE_SIZE,
        cfg_size,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Only the receive queue completes out of band; the transport
    // raises for the transmit queue on its own.
    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok(pci_dev)
}

/// Build a virtio-fs device and its PCI transport from parsed options.
///
/// `queue_size` must already be normalized by
/// [`super::fs::clamp_queue_size`]: `VirtQueue::new` panics on a size
/// that is not a power of two.
///
/// Ordering is load-bearing: the interrupt slot is cloned before the
/// device moves into the transport, and the `Weak` back-reference is
/// filled in after `VirtioPciDevice::new` returns.
pub fn virtio_fs(
    opts: &VirtioFsOpts,
    queue_size: u16,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioFs>>> {
    let fs = VirtioFs::new(opts, Arc::clone(ctx.physmap), log.clone())?;

    let msix = Arc::new(MsixTable::new(FS_MSIX_VECTORS, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        fs,
        bits::VIRTIO_DEV_TYPE_FS,
        FS_NUM_QUEUES,
        queue_size,
        FS_CONFIG_SIZE,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Both queues complete out of band, so the raise carries the queue
    // index and each queue raises its own MSI-X vector.
    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok(pci_dev)
}

/// Build a virtio-rnd device and its PCI transport.
pub fn virtio_rng(
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
) -> Arc<VirtioPciDevice<VirtioRng>> {
    // One queue, plus a config change vector.
    let msix = Arc::new(MsixTable::new(2, ctx.msi_sink()));
    VirtioPciDevice::new(
        VirtioRng::new(),
        bits::VIRTIO_DEV_TYPE_RNG,
        1,
        64,
        0,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    )
}
