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
use std::sync::{Arc, Mutex, Weak};

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
    VirtioConsole, CONSOLE_MSIX_VECTORS, CONSOLE_NUM_QUEUES,
    CONSOLE_QUEUE_SIZE, CONSOLE_RX_QUEUE,
};
use super::fs::{
    VirtioFs, VirtioFsOpts, FS_CONFIG_SIZE, FS_MSIX_VECTORS, FS_NUM_QUEUES,
};
use super::pci::VirtioPciDevice;
use super::rng::VirtioRng;
use super::viona;
use super::viona::VirtioViona;
use super::vsock::device::{
    VirtioVsock, VSOCK_MSIX_VECTORS, VSOCK_NUM_QUEUES, VSOCK_QUEUE_SIZE,
    VSOCK_RX_QUEUE,
};

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

/// Parse a virtio-blk config string:
/// `path[,nodelete][,ro][,sectorsize=N][,num-queues=N]`.
///
/// Unknown options are ignored so an older binary still boots a config
/// written for a newer one. A path cannot contain a comma, so the split
/// is unambiguous.
pub fn parse_blk_config(config: &str) -> (&str, VirtioBlockOpts) {
    let mut opts = VirtioBlockOpts::default();
    let mut path = config;

    if let Some(comma) = config.find(',') {
        path = &config[..comma];
        for opt in config[comma + 1..].split(',') {
            match opt {
                "nodelete" => opts.nodelete = true,
                "ro" => opts.read_only = true,
                s if s.starts_with("sectorsize=") => {
                    if let Ok(sz) = s["sectorsize=".len()..].parse::<u32>() {
                        if sz.is_power_of_two() && (512..=65536).contains(&sz) {
                            opts.sector_size = sz;
                        }
                    }
                }
                s if s.starts_with("num-queues=") => {
                    if let Ok(n) = s["num-queues=".len()..].parse::<u16>() {
                        if (1..=8).contains(&n) {
                            opts.num_queues = Some(n);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    (path, opts)
}

/// Build a virtio-blk device and its PCI transport.
///
/// The caller routes the INTx pin before this call and attaches the
/// returned device to the chipset after it, so this function names no
/// chipset type.
pub fn virtio_blk(
    config: &str,
    num_vcpus: u32,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioBlock>>> {
    anyhow::ensure!(!config.is_empty(), "virtio-blk requires a disk path");
    let (path, opts) = parse_blk_config(config);

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(!opts.read_only)
        .open(path)
        .with_context(|| format!("failed to open disk: {path}"))?;

    // A zvol has one SPA sync pipeline, so beyond ~4 queues there is no
    // throughput gain.
    let num_queues: u16 = opts
        .num_queues
        .unwrap_or_else(|| std::cmp::min(num_vcpus as u16, 4).max(1));

    let blk =
        VirtioBlock::new(file, &opts, Arc::clone(ctx.physmap), num_queues)
            .with_context(|| {
                format!("failed to create virtio-blk for: {path}")
            })?;

    let cfg_size = blk.config_size();
    let blk_intr = Arc::clone(&blk.interrupt);
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
    let pci_weak = Arc::downgrade(&pci_dev);
    *blk_intr.lock().expect("virtio-blk interrupt lock") =
        Some(Arc::new(move || {
            if let Some(dev) = pci_weak.upgrade() {
                dev.raise_queue_interrupt_for(0);
            }
        }));

    Ok(pci_dev)
}

/// Split a viona config string into the VNIC name and the promiscphys
/// flag.
///
/// promiscphys puts the VNIC in physical promiscuous mode, which a zone
/// with MAC spoofing allowed needs.
pub fn parse_viona_config(config: &str) -> (&str, bool) {
    match config.find(',') {
        Some(comma) => {
            let promisc = config[comma + 1..]
                .split(',')
                .any(|o| o == "promiscphys" || o == "promiscphys=true");
            (&config[..comma], promisc)
        }
        None => (config, false),
    }
}

/// Build a viona device and its PCI transport from `vnic[,promiscphys]`.
///
/// `defer_poll` holds the in-kernel interrupt poll thread back until ring
/// state is restored; a binary with no migration path passes `false`.
pub fn virtio_viona(
    config: &str,
    lintr: Option<LintrCfg>,
    defer_poll: bool,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioViona>>> {
    anyhow::ensure!(
        !config.is_empty(),
        "virtio-net-viona requires a VNIC name"
    );

    let (vnic_name, promiscphys) = parse_viona_config(config);

    use std::os::fd::AsRawFd;
    let dev = VirtioViona::new(vnic_name, ctx.hdl.as_raw_fd()).with_context(
        || format!("failed to create viona device for VNIC: {vnic_name}"),
    )?;

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

    // The poll thread needs the raise callback before the transport
    // exists, so the callback reads through a Weak that is filled in
    // once VirtioPciDevice::new returns.
    let hook: Arc<Mutex<Option<Weak<VirtioPciDevice<VirtioViona>>>>> =
        Arc::new(Mutex::new(None));
    let hook_clone = Arc::clone(&hook);
    let raise_fn: Arc<dyn Fn(u16) + Send + Sync> =
        Arc::new(move |queue_idx: u16| {
            if let Some(d) = hook_clone
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .and_then(|w| w.upgrade())
            {
                d.raise_queue_interrupt_for(queue_idx);
            }
        });

    let intr_log = log.new(slog::o!("component" => "viona-intr"));
    if defer_poll {
        dev.store_poll_fn(raise_fn, intr_log);
    } else {
        dev.start_intr_poll(raise_fn, intr_log);
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

    if let Ok(mut h) = hook.lock() {
        *h = Some(Arc::downgrade(&pci_dev));
    }

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
    let con_intr = console.interrupt();

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
    let pci_weak = Arc::downgrade(&pci_dev);
    *con_intr.lock().expect("virtio-console interrupt lock") =
        Some(Arc::new(move || {
            if let Some(dev) = pci_weak.upgrade() {
                dev.raise_queue_interrupt_for(CONSOLE_RX_QUEUE);
            }
        }));

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
    let fs_intr = Arc::clone(&fs.interrupt);

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

    // Both queues complete out of band, so the callback carries the
    // queue index and each queue raises its own MSI-X vector.
    let pci_weak = Arc::downgrade(&pci_dev);
    *fs_intr.lock().expect("virtio-fs interrupt lock") =
        Some(Arc::new(move |queue_idx: u16| {
            if let Some(dev) = pci_weak.upgrade() {
                dev.raise_queue_interrupt_for(queue_idx);
            }
        }));

    Ok(pci_dev)
}

/// Build a virtio-vsock device and its PCI transport.
///
/// Ordering is load-bearing: the interrupt slot is cloned before the
/// device moves into the transport, and the `Weak` back-reference is
/// filled in after `VirtioPciDevice::new` returns.
pub fn virtio_vsock(
    socket_path: &Path,
    guest_cid: u64,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioVsock>>> {
    let vsock = VirtioVsock::new(
        socket_path,
        guest_cid,
        Arc::clone(ctx.physmap),
        log.clone(),
    )?;
    let cfg_size = vsock.config_size();
    let vsock_intr = vsock.interrupt();

    let msix = Arc::new(MsixTable::new(VSOCK_MSIX_VECTORS, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        vsock,
        bits::VIRTIO_DEV_TYPE_VSOCK,
        VSOCK_NUM_QUEUES,
        VSOCK_QUEUE_SIZE,
        cfg_size,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Only the receive queue completes out of band; the transport
    // raises for the transmit queue on its own.
    let pci_weak = Arc::downgrade(&pci_dev);
    *vsock_intr.lock().expect("virtio-vsock interrupt lock") =
        Some(Arc::new(move || {
            if let Some(dev) = pci_weak.upgrade() {
                dev.raise_queue_interrupt_for(VSOCK_RX_QUEUE);
            }
        }));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blk_config_path_only() {
        let (path, opts) = parse_blk_config("/dev/zvol/rdsk/tank/disk0");
        assert_eq!(path, "/dev/zvol/rdsk/tank/disk0");
        assert!(!opts.read_only);
        assert_eq!(opts.sector_size, 512);
        assert_eq!(opts.num_queues, None);
    }

    #[test]
    fn blk_config_options() {
        let (path, opts) = parse_blk_config(
            "/tmp/d.img,ro,nodelete,sectorsize=4096,num-queues=2",
        );
        assert_eq!(path, "/tmp/d.img");
        assert!(opts.read_only);
        assert!(opts.nodelete);
        assert_eq!(opts.sector_size, 4096);
        assert_eq!(opts.num_queues, Some(2));
    }

    #[test]
    fn viona_config_splits_name_and_promisc() {
        assert_eq!(parse_viona_config("vnic0"), ("vnic0", false));
        assert_eq!(parse_viona_config("vnic0,promiscphys"), ("vnic0", true));
        assert_eq!(
            parse_viona_config("vnic0,promiscphys=true"),
            ("vnic0", true)
        );
        // An unknown option must not turn promiscuous mode on.
        assert_eq!(parse_viona_config("vnic0,mtu=9000"), ("vnic0", false));
    }

    #[test]
    fn blk_config_rejects_out_of_range_values() {
        // Values outside the accepted range leave the defaults in place
        // rather than handing a bad geometry to the guest.
        let (_, opts) =
            parse_blk_config("/tmp/d.img,sectorsize=777,num-queues=99");
        assert_eq!(opts.sector_size, 512);
        assert_eq!(opts.num_queues, None);
    }
}
