// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! firehyve's device catalog: the map from a `-s slot,driver[,config]`
//! string to a concrete device.
//!
//! An unrecognized driver name is an error. Skipping it would produce a
//! VM that boots without the device the operator asked for. The one
//! exception is `IGNORED`: names the bhyve zone brand puts on every
//! zone that firehyve has no device for.

use slog::{info, warn};

use vmm_devices::pci::Bdf;
use vmm_machine::devspec;
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

/// Driver names the bhyve zone brand emits that firehyve has no device
/// for, and therefore accepts and leaves unattached.
///
/// `add_fbuf` in `boot.c` puts a frame buffer and an xhci tablet on
/// every zone unless VNC is turned off, and VNC is on by default. Both
/// serve a graphical console. firehyve boots a kernel directly onto a
/// serial console and has no frame buffer, so the slots stay empty.
/// Refusing them would mean an unmodified brand can never start this
/// binary.
const IGNORED: &[&str] = &["fbuf", "xhci"];

/// Names that attach nothing: the chipset owns two of them and the
/// brand's display devices are ignored. Neither reaches the registry.
fn attaches_nothing(driver: &str) -> bool {
    IGNORED.contains(&driver) || matches!(driver, "hostbridge" | "lpc")
}

/// The registry ids the argv must have produced, for the privilege
/// lock's open-device witness.
///
/// Every spec that attaches a device opened its backing file to get
/// into the registry, so an id missing there is a device not open.
pub(crate) fn witness_ids(specs: &[String]) -> Vec<String> {
    specs
        .iter()
        .filter(|spec| {
            split_spec(spec)
                .is_ok_and(|(_, driver, _)| !attaches_nothing(driver))
        })
        .map(|spec| devspec::device_id(spec))
        .collect()
}

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
    let (bdf, driver, config) = devspec::split(spec)?;
    if !SUPPORTED.contains(&driver) && !IGNORED.contains(&driver) {
        return Err(unsupported_driver(driver));
    }
    Ok((bdf, driver, config))
}

/// Build one PCI device from its spec string.
///
/// A vsock device also carries the control slot that answers the
/// `CONTROL` verb, so `control` collects one slot per vsock device. The
/// caller fills the slots once the device registry is complete.
///
/// Every attach here is the fallible one. The panicking `attach_device`
/// would take the whole VM down through a poisoned bus lock.
pub(crate) fn create_pci_device(
    spec: &str,
    num_vcpus: u32,
    ctx: &vmm_machine::PciDeviceCtx<'_>,
    control: &mut Vec<ControlSlot>,
) -> anyhow::Result<vmm_machine::CreatedPciDevice> {
    let (bdf, driver, config) = split_spec(spec)?;
    if IGNORED.contains(&driver) {
        warn!(ctx.log, "PCI device ignored";
            "bdf" => %bdf,
            "driver" => driver,
            "reason" => "firehyve has no frame buffer; the guest console \
                         is serial",
        );
        return Ok((None, None));
    }
    let actx = attach::VirtioAttachCtx {
        physmap: ctx.physmap,
        bus_pio: ctx.bus_pio,
        bus_mmio: ctx.bus_mmio,
        hdl: ctx.vmm_hdl,
    };

    match driver {
        "virtio-blk" | "virtio-blk-pci" => {
            anyhow::ensure!(
                !config.is_empty(),
                "virtio-blk requires a disk path"
            );
            let (path, opts) = devspec::parse_blk_config(config);
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_blk(
                std::path::Path::new(path),
                &opts,
                num_vcpus,
                lintr,
                &actx,
            )?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-blk attached";
                "bdf" => %bdf, "path" => config);
            Ok(vmm_machine::created(dev))
        }
        "virtio-net-viona" => {
            let lintr = ctx.chipset.route_lintr(&bdf);
            // firehyve is never a migration destination, so the in-kernel
            // interrupt poll thread starts immediately.
            let (vnic, promiscphys) = devspec::parse_viona_config(config);
            let dev = attach::virtio_viona(
                vnic,
                promiscphys,
                lintr,
                false,
                &actx,
                ctx.log,
            )?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-net-viona attached";
                "bdf" => %bdf, "vnic" => config);
            Ok(vmm_machine::created(dev))
        }
        "virtio-fs" => {
            use vmm_virtio::fs::clamp_queue_size;
            let opts = devspec::parse_fs_config(config)?;
            let queue_size = clamp_queue_size(opts.queue_size);
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev =
                attach::virtio_fs(&opts, queue_size, lintr, &actx, ctx.log)?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-fs attached";
                "bdf" => %bdf,
                "tag" => &opts.tag,
                "path" => %opts.path.display());
            Ok(vmm_machine::created(dev))
        }
        "virtio-vsock" => {
            let (path, cid) = devspec::parse_vsock_config(config)?;
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
            Ok(vmm_machine::created(dev))
        }
        "virtio-rnd" => {
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_rng(lintr, &actx);
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-rnd attached"; "bdf" => %bdf);
            Ok(vmm_machine::created(dev))
        }
        "virtio-console" => {
            let path = devspec::parse_console_config(config)?;
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
            Ok(vmm_machine::created(dev))
        }
        // The chipset creates both of these; accepting the names keeps a
        // bhyve-shaped command line working.
        "hostbridge" | "lpc" => Ok((None, None)),
        // Reachable only if SUPPORTED grows a name before its arm does.
        other => Err(unsupported_driver(other)),
    }
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
        assert!(split_spec("8,passthru,/dev/ppt0").is_err());
        // A name that merely resembles one the brand forces is still a
        // typo, not an ignored device.
        assert!(split_spec("6,fbufx").is_err());
        assert!(split_spec("7,xhc").is_err());
    }

    /// `boot.c` puts a frame buffer and an xhci tablet on every zone
    /// that has VNC on, which is the default. firehyve has no frame
    /// buffer, so refusing them means an unmodified brand can never
    /// start it.
    ///
    /// Mutation this kills: dropping `fbuf` or `xhci` from IGNORED, or
    /// folding them into SUPPORTED, which would claim a device that
    /// does not exist.
    #[test]
    fn the_display_devices_the_brand_emits_are_accepted_and_skipped() {
        for spec in ["30:0,fbuf,vga=off,unix=/tmp/vm.vnc", "30:1,xhci,tablet"] {
            let (_, driver, _) =
                split_spec(spec).unwrap_or_else(|e| panic!("{spec}: {e}"));
            assert!(IGNORED.contains(&driver), "{driver} is not ignored");
            assert!(
                !SUPPORTED.contains(&driver),
                "{driver} has no device, so it must not be advertised"
            );
        }
    }

    /// The lock's witness reads the ids the registry will hold, so a
    /// name that attaches nothing must not be expected there.
    #[test]
    fn the_witness_expects_every_device_that_attaches() {
        let specs: Vec<String> = [
            "0,hostbridge",
            "4,virtio-blk,/tmp/d.img",
            "7,virtio-net-viona,net0",
            "17,virtio-vsock,/tmp/v.sock,cid=3",
            "30:0,fbuf,vga=off",
            "30:1,xhci,tablet",
            "31,lpc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(
            witness_ids(&specs),
            vec!["virtio-blk@4", "virtio-net-viona@7", "virtio-vsock@17"]
        );
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
