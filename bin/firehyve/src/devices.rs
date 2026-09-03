// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! firehyve's device catalog: the map from a `-s slot,driver[,config]`
//! string to a concrete device.
//!
//! An unrecognized driver name is an error. Skipping it would produce a
//! VM that boots without the device the operator asked for.

use std::sync::Arc;

use slog::info;

use vmm_devices::pci::Bdf;
use vmm_virtio::attach;
use vmm_virtio::vsock::control::ControlSlot;

/// Driver names this binary implements, plus the two the chipset
/// already creates. One list so the parse gate and the operator-facing
/// error can never disagree.
const SUPPORTED: &[&str] = &[
    "virtio-blk",
    "virtio-blk-pci",
    "virtio-net-viona",
    "virtio-rnd",
    "virtio-console",
    "virtio-fs",
    "virtio-vsock",
    "hostbridge",
    "lpc",
];

fn unsupported_driver(driver: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "firehyve does not support PCI driver '{driver}' (supported: {})",
        SUPPORTED.join(", ")
    )
}

/// Split a device spec into its address, driver name, and config.
///
/// Kept separate from device construction so the driver-name contract is
/// testable without a live VM.
pub(crate) fn split_spec(spec: &str) -> anyhow::Result<(Bdf, &str, &str)> {
    let parts: Vec<&str> = spec.splitn(3, ',').collect();
    anyhow::ensure!(
        parts.len() >= 2,
        "invalid PCI slot spec '{spec}': need slot,driver[,config]"
    );

    let bdf = vmm_machine::parse_bdf(parts[0]).ok_or_else(|| {
        anyhow::anyhow!("invalid BDF '{}' in spec '{}'", parts[0], spec)
    })?;
    let driver = parts[1];
    if !SUPPORTED.contains(&driver) {
        return Err(unsupported_driver(driver));
    }

    Ok((bdf, driver, parts.get(2).copied().unwrap_or("")))
}

/// Build one PCI device from its spec string.
///
/// A vsock device also carries the control slot that answers the
/// `CONTROL` verb, so `control` collects one slot per vsock device. The
/// caller fills the slots once the device registry is complete.
///
/// Every attach here is the fallible one. The panicking `attach_device`
/// would take the whole VM down through a poisoned bus lock, and this
/// runs on a control thread as well as on the startup path.
pub(crate) fn create_pci_device(
    spec: &str,
    num_vcpus: u32,
    ctx: &vmm_machine::PciDeviceCtx<'_>,
    control: &mut Vec<ControlSlot>,
) -> anyhow::Result<vmm_machine::CreatedPciDevice> {
    let (bdf, driver, config) = split_spec(spec)?;
    let actx = attach::VirtioAttachCtx {
        physmap: ctx.physmap,
        bus_pio: ctx.bus_pio,
        bus_mmio: ctx.bus_mmio,
        hdl: ctx.vmm_hdl,
    };

    match driver {
        "virtio-blk" | "virtio-blk-pci" => {
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_blk(config, num_vcpus, lintr, &actx)?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-blk attached";
                "bdf" => %bdf, "path" => config);
            Ok((
                Some(dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "virtio-net-viona" => {
            let lintr = ctx.chipset.route_lintr(&bdf);
            // firehyve is never a migration destination, so the in-kernel
            // interrupt poll thread starts immediately.
            let dev =
                attach::virtio_viona(config, lintr, false, &actx, ctx.log)?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-net-viona attached";
                "bdf" => %bdf, "vnic" => config);
            Ok((
                Some(dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "virtio-fs" => {
            use vmm_virtio::fs::{clamp_queue_size, parse_fs_config};
            let opts = parse_fs_config(config)?;
            let queue_size = clamp_queue_size(opts.queue_size);
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev =
                attach::virtio_fs(&opts, queue_size, lintr, &actx, ctx.log)?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-fs attached";
                "bdf" => %bdf,
                "tag" => &opts.tag,
                "path" => %opts.path.display());
            Ok((
                Some(dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "virtio-vsock" => {
            use vmm_virtio::vsock::device::parse_vsock_config;
            let (path, cid) = parse_vsock_config(config)?;
            let lintr = ctx.chipset.route_lintr(&bdf);
            let (dev, slot) = vmm_virtio::vsock::control::attach_with_control(
                &path, cid, lintr, &actx, ctx.log,
            )?;
            control.push(slot);
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-vsock attached";
                "bdf" => %bdf,
                "cid" => cid,
                "socket" => %path.display());
            Ok((
                Some(dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "virtio-rnd" => {
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_rng(lintr, &actx);
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-rnd attached"; "bdf" => %bdf);
            Ok((
                Some(dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "virtio-console" => {
            let path = vmm_virtio::console::parse_console_config(config)?;
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_console(
                std::path::Path::new(path),
                lintr,
                &actx,
                ctx.log,
            )?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-console attached";
                "bdf" => %bdf, "socket" => path);
            Ok((
                Some(dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        // The chipset creates both of these; accepting the names keeps a
        // bhyve-shaped command line working.
        "hostbridge" | "lpc" => Ok((None, None)),
        // Reachable only if SUPPORTED grows a name before its arm does.
        other => Err(unsupported_driver(other)),
    }
}

/// The catalog closure, bound to this VM and handed to the hotplug
/// engine.
///
/// SECURITY: the same closure the boot-time `-s` pass drives, so a
/// control request can name no driver and no path that a command line
/// could not. See the trust note in `crate::control`.
pub(crate) fn hotplug_factory(num_cpus: u32) -> vmm_machine::HotplugFactory {
    Box::new(move |spec, ctx| {
        // A vsock device carries the control slot this interface is
        // served on, and a slot is armed once, at startup. Adding one
        // later would hand the guest a socket that answers nothing, so
        // it is refused before anything is built.
        let (_, driver, _) = split_spec(spec)?;
        anyhow::ensure!(
            driver != "virtio-vsock",
            "virtio-vsock cannot be added at run time: its CONTROL slot \
             is armed at startup"
        );
        let mut slots = Vec::new();
        let created = create_pci_device(spec, num_cpus, ctx, &mut slots)?;
        debug_assert!(slots.is_empty(), "only vsock yields a control slot");
        Ok(created)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_driver_is_a_hard_error() {
        let err =
            split_spec("4,nvme,/tmp/d.img").expect_err("nvme is rshyve only");
        let msg = err.to_string();
        assert!(msg.contains("nvme"), "got: {msg}");
        assert!(
            msg.contains("virtio-blk"),
            "error should list what works: {msg}"
        );
    }

    #[test]
    fn the_error_names_every_accepted_driver() {
        // The list an operator is shown must stay the list the catalog
        // accepts, including the aliases.
        let err = split_spec("4,virtio-blkk,/tmp/d.img").expect_err("typo");
        let msg = err.to_string();
        for name in SUPPORTED {
            assert!(msg.contains(name), "'{name}' missing from: {msg}");
        }
    }

    #[test]
    fn a_typo_is_rejected_rather_than_skipped() {
        // The tolerant catch-all in rshyve yields a VM with a missing
        // disk; firehyve refuses to start instead.
        assert!(split_spec("4,virtio-blkk,/tmp/d.img").is_err());
        assert!(split_spec("6,fbuf").is_err());
        assert!(split_spec("7,xhci").is_err());
        assert!(split_spec("8,passthru,/dev/ppt0").is_err());
    }

    #[test]
    fn supported_drivers_split_into_bdf_driver_config() {
        let (bdf, driver, config) =
            split_spec("4,virtio-blk,/tmp/d.img,ro").expect("accepted");
        assert_eq!(bdf.dev(), 4);
        assert_eq!(driver, "virtio-blk");
        assert_eq!(config, "/tmp/d.img,ro");

        let (_, driver, config) =
            split_spec("7,virtio-net-viona,net0").expect("accepted");
        assert_eq!(driver, "virtio-net-viona");
        assert_eq!(config, "net0");

        let (_, driver, config) = split_spec("5,virtio-rnd").expect("accepted");
        assert_eq!(driver, "virtio-rnd");
        assert_eq!(config, "");
    }

    #[test]
    fn every_supported_name_is_accepted() {
        // Guards the catalog against a name that the error message
        // advertises but the parser rejects.
        for name in SUPPORTED {
            let spec = format!("4,{name},/tmp/d.img");
            let (_, driver, _) =
                split_spec(&spec).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(driver, *name);
        }
    }

    #[test]
    fn chipset_owned_names_are_accepted() {
        // A bhyve-shaped argv carries these; the chipset already made
        // them, so they parse and attach nothing.
        assert!(split_spec("0,hostbridge").is_ok());
        assert!(split_spec("31,lpc").is_ok());
    }

    #[test]
    fn malformed_specs_are_rejected() {
        let err = split_spec("4").expect_err("no driver");
        assert!(err.to_string().contains("slot,driver"), "got: {err}");
        let err = split_spec("notaslot,virtio-rnd").expect_err("bad BDF");
        assert!(err.to_string().contains("invalid BDF"), "got: {err}");
    }
}
