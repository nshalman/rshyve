// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `-s` PCI device catalog: every driver argv or a hot-add can
//! name, and the checks that run over the whole set.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use slog::{info, warn, Logger};

use vmm_config::Cli;
use vmm_core::machine::Machine;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::InputBroker;
use vmm_machine::{devspec, CreatedPciDevice, DeviceRegistry};
use vmm_virtio::attach::{self, VirtioAttachCtx};

use crate::{bootorder, msix};

/// The registry, the AHCI-CD flag, and the plumbing a device needs to
/// be built. The context outlives startup so hotplug can build one
/// after the VM is running.
pub type PciDeviceInventory = (
    Arc<DeviceRegistry>,
    bool,
    Arc<vmm_machine::pci::OwnedPciCtx>,
);

/// Parse all `-s` PCI slot specifications and create the corresponding
/// devices. Returns the run-time registry, which owns every handle and
/// which the run, control and teardown paths all read. The AHCI flag
/// lets the control path reject migration without re-parsing the
/// original specifications.
pub fn init_pci_devices(
    machine: &Machine,
    chipset: &Arc<I440FxChipset>,
    cli: &Cli,
    num_vcpus: u32,
    input: &Arc<InputBroker>,
    log: &Logger,
) -> anyhow::Result<PciDeviceInventory> {
    let is_migrate = cli.migrate_listen.is_some();

    let specs = driver_specs(is_migrate, &cli.pci_slot)?;
    let has_ahci_cd = specs_have_ahci_cd(&specs);

    let owned = Arc::new(vmm_machine::pci::OwnedPciCtx::new(
        machine, chipset, input, log,
    ));
    let ctx = vmm_machine::PciDeviceCtx::borrow_from(&owned);
    let vm_name = &cli.vm_name;
    let attached =
        vmm_machine::attach_pci_devices(&ctx, &specs, &mut |spec, ctx| {
            parse_and_create_pci_device(
                spec, num_vcpus, is_migrate, vm_name, ctx,
            )
        })?;

    Ok((attached.registry, has_ahci_cd, owned))
}

/// The catalog closure, bound to this VM and handed to the hotplug
/// engine.
///
/// SECURITY: this is the same closure argv drives, so a control request
/// can name no driver and no path that a command line could not. It
/// does open a host file that a socket peer chose, which is authority
/// equal to argv. The peer gate in `vmm_core::unixsock::accept` is what
/// grants it: the peer must share this process's uid and be in this
/// zone or the global zone.
pub fn hotplug_factory(
    cli: &Cli,
    num_vcpus: u32,
) -> vmm_machine::HotplugFactory {
    let vm_name = cli.vm_name.clone();
    Box::new(move |spec, ctx| {
        // Never a migration destination: a hot-add happens on a running
        // VM, and the guest programs the new rings itself.
        parse_and_create_pci_device(spec, num_vcpus, false, &vm_name, ctx)
    })
}

/// The specs as the drivers see them, refused if this VM cannot take
/// them. The checks read the stripped specs, so a `bootindex=` in any
/// position cannot hide an option from them.
fn driver_specs(
    is_migrate_dest: bool,
    raw: &[String],
) -> anyhow::Result<Vec<String>> {
    let specs = raw
        .iter()
        .map(|spec| bootorder::strip_bootindex(spec).map(|(s, _)| s))
        .collect::<anyhow::Result<Vec<_>>>()?;
    reject_unmigratable_destination(is_migrate_dest, &specs)?;
    Ok(specs)
}

fn pci_spec_driver(spec: &str) -> Option<&str> {
    Some(devspec::parts(spec).driver).filter(|driver| !driver.is_empty())
}

/// True when any bootindex-stripped spec names the ahci-cd driver.
///
/// Read from the spec text alone. The attach seam is device-agnostic and
/// reports no driver names, so the flag cannot come from the handles it
/// returns.
fn specs_have_ahci_cd(specs: &[String]) -> bool {
    specs
        .iter()
        .any(|spec| pci_spec_driver(spec) == Some("ahci-cd"))
}

/// `specs` must be bootindex-stripped.
fn reject_unmigratable_destination(
    is_migrate_dest: bool,
    specs: &[String],
) -> anyhow::Result<()> {
    if is_migrate_dest {
        if specs_have_ahci_cd(specs) {
            anyhow::bail!(
                "ahci-cd is not supported on a migration destination"
            );
        }
        // Without the source's shape the placeholder would show the
        // guest another disk.
        if let Some(spec) = specs
            .iter()
            .find(|spec| is_mantablock_nvme(spec) && !has_ring_shape(spec))
        {
            anyhow::bail!(
                "a MantaBlock disk on a migration destination needs \
                 ring-shape=; take the -s list from the source's \
                 migrate-config ({spec})"
            );
        }
    }
    Ok(())
}

pub(crate) fn is_mantablock_nvme(spec: &str) -> bool {
    let parts = devspec::parts(spec);
    parts.driver == "nvme" && parts.config.starts_with("mantablock=")
}

fn has_ring_shape(spec: &str) -> bool {
    devspec::parts(spec)
        .config
        .split(',')
        .any(|opt| opt.starts_with("ring-shape="))
}

/// The shape a ring disk must have, if its spec names one. A
/// destination must have one: `driver_specs` refuses that first, with a
/// clearer error.
fn ring_shape(
    opts: &devspec::MantaBlockOpts,
    is_migrate_dest: bool,
) -> anyhow::Result<Option<vmm_mantablock::RingShape>> {
    let shape = opts
        .ring_shape
        .as_deref()
        .map(str::parse::<vmm_mantablock::RingShape>)
        .transpose()
        .map_err(|e| anyhow::anyhow!("nvme: {e}"))?;
    anyhow::ensure!(
        shape.is_some() || !is_migrate_dest,
        "nvme: volume {} on a migration destination needs ring-shape=",
        opts.volume
    );
    // Only a destination may build a disk it does not attach.
    anyhow::ensure!(
        shape.is_none() || is_migrate_dest,
        "nvme: ring-shape= is for a migration destination \
         (--migrate-listen) only (volume {})",
        opts.volume
    );
    Ok(shape)
}

fn reject_ahci_hd<T>() -> anyhow::Result<T> {
    anyhow::bail!(
        "AHCI device type 'ahci-hd' is not implemented; use virtio-blk or nvme"
    );
}

/// Parse a `-s` PCI slot specification and create the device.
///
/// Format: `[bus:]dev[:func],driver[,config...]`
/// Examples:
///   `4,virtio-blk,/dev/zvol/rdsk/zones/uuid/disk0`
///   `0:5:0,virtio-blk,/path/to/disk.raw`
///
/// Every attach here is the fallible one. The panicking `attach_device`
/// would take the whole VM down through a poisoned bus lock, and this
/// runs on a control thread as well as on the startup path.
fn parse_and_create_pci_device(
    spec: &str,
    num_vcpus: u32,
    is_migrate_dest: bool,
    vm_name: &str,
    ctx: &vmm_machine::PciDeviceCtx<'_>,
) -> anyhow::Result<CreatedPciDevice> {
    let chipset = ctx.chipset;
    let bus_pio = ctx.bus_pio;
    let bus_mmio = ctx.bus_mmio;
    let physmap = ctx.physmap;
    let vmm_hdl = ctx.vmm_hdl;
    let segids = ctx.segids;
    let input = ctx.input;
    let log = ctx.log;

    let virtio_ctx = VirtioAttachCtx {
        physmap,
        bus_pio,
        bus_mmio,
        hdl: vmm_hdl,
    };

    let (bdf, driver, config) = devspec::split(spec)?;

    match driver {
        "virtio-blk" | "virtio-blk-pci" => {
            anyhow::ensure!(
                !config.is_empty(),
                "virtio-blk requires a disk path"
            );
            let (path, blk_opts) = devspec::parse_blk_config(config);
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_blk(
                std::path::Path::new(path),
                &blk_opts,
                num_vcpus,
                lintr,
                &virtio_ctx,
                log,
            )?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-blk attached";
                "bdf" => %bdf,
                "path" => config,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "virtio-net-viona" => {
            let (vnic_name, promiscphys) = devspec::parse_viona_config(config);

            // On a migrate destination the rings are not programmed yet, so
            // the poll thread waits for start_poll_deferred() after restore.
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_viona(
                vnic_name,
                promiscphys,
                lintr,
                is_migrate_dest,
                &virtio_ctx,
                log,
            )?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-net-viona attached";
                "bdf" => %bdf,
                "vnic" => vnic_name,
                "promiscphys" => promiscphys,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "nvme" => match devspec::parse_nvme_config(config)? {
            devspec::NvmeSpec::File(path, blk_opts) => {
                let file = vmm_devices::blkdev::open_disk(
                    std::path::Path::new(path),
                    blk_opts.read_only,
                )
                .with_context(|| format!("failed to open disk: {}", path))?;

                let nvme = vmm_storage::nvme::NvmeController::new_with_logger(
                    file,
                    blk_opts.read_only,
                    bdf.dev(), // unique instance ID per slot
                    Arc::clone(physmap),
                    Arc::clone(vmm_hdl),
                    Arc::clone(bus_mmio),
                    log.new(slog::o!(
                        "component" => "nvme",
                        "path" => path.to_string(),
                    )),
                )
                .with_context(|| {
                    format!("failed to create nvme for: {path}")
                })?;

                chipset.try_attach_device(bdf, nvme.clone())?;
                info!(log, "nvme attached";
                    "bdf" => %bdf,
                    "path" => path,
                );
                Ok(vmm_machine::created(nvme))
            }
            devspec::NvmeSpec::MantaBlock(opts) => {
                let shape = ring_shape(&opts, is_migrate_dest)?;
                let nvme = mantablock_nvme(&opts, shape, bdf.dev(), ctx)?;
                chipset.try_attach_device(bdf, nvme.clone())?;
                info!(log, "nvme attached";
                    "bdf" => %bdf,
                    "volume" => %opts.volume,
                    "socket" => %opts.socket.display(),
                );
                Ok(vmm_machine::created(nvme))
            }
        },
        "virtio-console" => {
            let path = devspec::parse_console_config(config)?;
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_console(
                std::path::Path::new(path),
                lintr,
                &virtio_ctx,
                log,
            )?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-console attached";
                "bdf" => %bdf,
                "socket" => path,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "virtio-fs" => {
            use vmm_virtio::fs::clamp_queue_size;
            let opts = devspec::parse_fs_config(config)?;
            let queue_size = clamp_queue_size(opts.queue_size);
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev =
                attach::virtio_fs(&opts, queue_size, lintr, &virtio_ctx, log)?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-fs attached";
                "bdf" => %bdf,
                "tag" => &opts.tag,
                "path" => %opts.path.display(),
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "virtio-rnd" => {
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_rng(lintr, &virtio_ctx);
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-rnd attached";
                "bdf" => %bdf,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "fbuf" => {
            let segid = segids.alloc().ok_or_else(|| {
                anyhow::anyhow!(
                    "no free memory segment for fbuf (kernel limit is {} per VM)",
                    vmm_core::mem::VM_MAX_MEMSEGS
                )
            })?;

            let FbufConfig {
                vnc_path,
                vnc_password,
                width,
                height,
            } = parse_fbuf_config(config, vm_name, log)?;

            let fbuf = vmm_hid::fbuf::Framebuffer::new(
                width,
                height,
                &vnc_path,
                vnc_password,
                Arc::clone(bus_mmio),
                Arc::clone(vmm_hdl),
                segid,
                Arc::clone(input),
                log.new(slog::o!("component" => "fbuf")),
            )?;
            chipset.try_attach_device(bdf, fbuf.clone())?;
            info!(log, "fbuf attached";
                "bdf" => %bdf,
                "vnc" => vnc_path.display().to_string(),
                "resolution" => format!("{}x{}", width, height),
            );
            Ok(vmm_machine::created(fbuf))
        }
        "xhci" => {
            // xHCI USB controller with tablet device
            // Config is just "tablet" (the attached USB device)
            let lintr = chipset.route_lintr(&bdf);
            let intr_pin = lintr.map(|(_, pin)| pin);
            let xhci = vmm_hid::xhci::XhciController::new(
                Arc::clone(physmap),
                Arc::clone(bus_mmio),
                intr_pin,
            );
            chipset.try_attach_device(bdf, xhci.clone())?;
            input.set_pointer(xhci.clone());
            info!(log, "xhci tablet attached"; "bdf" => %bdf);
            Ok(vmm_machine::created(xhci))
        }
        "passthru" => {
            if config.is_empty() {
                anyhow::bail!(
                    "passthru requires a device path (e.g., /dev/ppt0)"
                );
            }
            // Config is just the path to /dev/pptN
            let path = config.split(',').next().unwrap_or(config);
            let dev = vmm_passthru::PciPassthru::new(
                path,
                Arc::clone(vmm_hdl),
                Arc::clone(bus_pio),
                log.clone(),
            )
            .with_context(|| {
                format!("failed to init passthru device: {}", path)
            })?;
            chipset.try_attach_device(bdf, dev.clone())?;
            info!(log, "passthru attached";
                "bdf" => %bdf,
                "path" => path,
            );
            Ok((Some(dev as Arc<dyn vmm_devices::pci::PciDevice>), None))
        }
        "ahci-cd" => {
            let path = devspec::parse_ahci_cd_path(config)?;
            let media = vmm_storage::ahci::media::IsoMedia::open(
                std::path::Path::new(path),
            )
            .with_context(|| format!("failed to open ISO: {}", path))?;
            let lintr = chipset.route_lintr(&bdf);
            let ahci = vmm_storage::ahci::AhciCtrl::new(
                media,
                Arc::clone(physmap),
                Arc::clone(bus_mmio),
                lintr.map(|(_, pin)| pin),
                log.new(slog::o!("component" => "ahci-cd")),
            );
            chipset.try_attach_device(bdf, ahci.clone())?;
            info!(log, "ahci-cd attached"; "bdf" => %bdf, "path" => path);
            Ok(vmm_machine::created(ahci))
        }
        // A separate arm prevents an unsupported disk from silently becoming
        // a read-only ATAPI device with different guest-visible semantics.
        "ahci-hd" => reject_ahci_hd(),
        "hostbridge" => {
            // Already created by chipset, skip
            Ok((None, None))
        }
        "lpc" => {
            // Already created by chipset, skip
            Ok((None, None))
        }
        other => {
            warn!(log, "unknown PCI driver, skipping";
                "driver" => other,
                "bdf" => %bdf,
            );
            Ok((None, None))
        }
    }
}

/// An NVMe controller on a MantaBlock volume.
///
/// With a `shape`, which only a migration destination has, nothing
/// attaches here: the source holds the volume until its commit, and the
/// orchestrator moves the volume after it. The disk attaches before the
/// guest runs. Any other VM attaches now.
fn mantablock_nvme(
    opts: &devspec::MantaBlockOpts,
    shape: Option<vmm_mantablock::RingShape>,
    instance_id: u8,
    ctx: &vmm_machine::PciDeviceCtx<'_>,
) -> anyhow::Result<Arc<vmm_storage::nvme::NvmeController>> {
    let log = ctx.log.new(slog::o!(
        "component" => "nvme",
        "volume" => opts.volume.to_string(),
    ));
    let mut cfg = vmm_mantablock::Config::new(
        opts.socket.clone(),
        opts.volume,
        log.clone(),
    );
    cfg.allow_volatile = opts.allow_volatile;
    if let Some(us) = opts.spin_us {
        cfg.spin = Duration::from_micros(u64::from(us));
    }
    let (backend, config) = match shape {
        Some(shape) => {
            info!(log, "nvme: the volume attaches after the migration \
                commits"; "shape" => %shape);
            (
                vmm_mantablock::deferred_factory(cfg, shape),
                shape.nvme_config(),
            )
        }
        None => {
            let conn = vmm_mantablock::connect(cfg).with_context(|| {
                format!(
                    "nvme: cannot attach volume {} at {}",
                    opts.volume,
                    opts.socket.display()
                )
            })?;
            if !conn.durable() {
                warn!(
                    log,
                    "nvme: the engine's writes are not durable, so the \
                    guest sees a volatile write cache"
                );
            }
            let config = conn.nvme_config();
            (conn.into_handover_factory(), config)
        }
    };
    vmm_storage::nvme::NvmeController::with_backend(
        backend,
        config,
        false,
        instance_id,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.vmm_hdl),
        Arc::clone(ctx.bus_mmio),
        log,
    )
}

/// What an `fbuf` spec asks for.
#[derive(Debug)]
struct FbufConfig {
    vnc_path: PathBuf,
    vnc_password: Option<String>,
    width: u16,
    height: u16,
}

/// Parse `vga=off,unix=/path[,password=pw][,password-file=/path][,w=N][,h=N]`.
///
/// A key that does not parse is refused rather than replaced by the
/// default: `w=abc` used to boot a 1024x768 framebuffer and say nothing,
/// so an operator's typo looked like the VMM ignoring them.
fn parse_fbuf_config(
    config: &str,
    vm_name: &str,
    log: &Logger,
) -> anyhow::Result<FbufConfig> {
    let mut parsed = FbufConfig {
        vnc_path: PathBuf::from("/var/run/rshyve")
            .join(vm_name)
            .join("vnc.sock"),
        vnc_password: None,
        width: 1024,
        height: 768,
    };
    let mut password_in_argv = false;

    for opt in config.split(',') {
        if let Some(p) = opt.strip_prefix("unix=") {
            parsed.vnc_path = PathBuf::from(p);
        } else if let Some(p) = opt.strip_prefix("password-file=") {
            parsed.vnc_password =
                Some(crate::secret::read_file(std::path::Path::new(p), log)?);
        } else if let Some(p) = opt.strip_prefix("password=") {
            parsed.vnc_password = Some(p.to_string());
            password_in_argv = true;
        } else if let Some(w) = opt.strip_prefix("w=") {
            parsed.width = parse_fbuf_dimension("w", w)?;
        } else if let Some(h) = opt.strip_prefix("h=") {
            parsed.height = parse_fbuf_dimension("h", h)?;
        } else if !opt.is_empty() && opt != "vga=off" {
            anyhow::bail!("fbuf: '{opt}' is not an fbuf option");
        }
    }

    if password_in_argv {
        crate::secret::warn_argv_exposure(
            log,
            "fbuf password=",
            "fbuf password-file=",
        );
    }
    Ok(parsed)
}

fn parse_fbuf_dimension(key: &str, value: &str) -> anyhow::Result<u16> {
    let parsed: u16 = value
        .parse()
        .with_context(|| format!("fbuf: {key}={value} is not 1..=65535"))?;
    if parsed == 0 {
        anyhow::bail!("fbuf: {key}=0 describes no framebuffer");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn an_fbuf_size_that_does_not_parse_is_refused() {
        // Silently booting 1024x768 made a typo look like the VMM
        // ignoring the operator.
        for spec in ["w=abc", "h=abc", "w=100000", "w=0", "h=0", "w="] {
            let error = parse_fbuf_config(spec, "vm", &null_log())
                .expect_err("a size that does not parse must be refused");
            assert!(format!("{error:#}").contains("fbuf"), "{spec}: {error:#}");
        }
    }

    #[test]
    fn an_fbuf_spec_is_parsed_whole() {
        let parsed = parse_fbuf_config(
            "vga=off,unix=/tmp/vnc.sock,password=pw,w=800,h=600",
            "vm",
            &null_log(),
        )
        .expect("a complete spec");

        assert_eq!(parsed.vnc_path, PathBuf::from("/tmp/vnc.sock"));
        assert_eq!(parsed.vnc_password.as_deref(), Some("pw"));
        assert_eq!((parsed.width, parsed.height), (800, 600));
    }

    #[test]
    fn an_fbuf_spec_defaults_to_the_vm_socket_path() {
        let parsed =
            parse_fbuf_config("", "testvm", &null_log()).expect("no options");

        assert_eq!(
            parsed.vnc_path,
            PathBuf::from("/var/run/rshyve/testvm/vnc.sock"),
        );
        assert_eq!((parsed.width, parsed.height), (1024, 768));
    }

    #[test]
    fn an_unknown_fbuf_option_is_refused() {
        let error = parse_fbuf_config("widht=800", "vm", &null_log())
            .expect_err("a misspelled key");

        assert!(format!("{error:#}").contains("widht=800"), "{error:#}");
    }

    #[test]
    fn ahci_hd_is_rejected() {
        let error = reject_ahci_hd::<()>().unwrap_err();
        let message = error.to_string();

        assert!(message.contains("not implemented"), "{message}");
        // The refusal has to name what to use instead, or an operator
        // is left with a driver name and no next step.
        assert!(message.contains("virtio-blk"), "{message}");
    }

    #[test]
    fn specs_have_ahci_cd_reads_the_driver_field() {
        let specs = vec![
            "4,virtio-blk,/dev/rdsk/test".to_string(),
            "5,ahci-cd,/x.iso".to_string(),
        ];

        assert!(specs_have_ahci_cd(&specs));
        assert!(!specs_have_ahci_cd(&specs[..1]));
        // A path that merely mentions the driver name is not a match.
        assert!(!specs_have_ahci_cd(&["4,nvme,/ahci-cd".to_string()]));
    }

    #[test]
    fn reject_unmigratable_destination_rejects_ahci_cd() {
        let specs = vec!["5,ahci-cd,/x.iso".to_string()];

        let error = reject_unmigratable_destination(true, &specs).unwrap_err();
        assert!(error.to_string().contains("ahci-cd"));
    }

    #[test]
    fn a_triton_vm_is_accepted_on_a_migration_destination() {
        // The -s list the bhyve brand gives every VM, plus a ring disk.
        let raw: Vec<String> = [
            "0,hostbridge,model=i440fx",
            "3:0,virtio-net-viona,net0",
            "4:0,virtio-rnd",
            "7,nvme,mantablock=/mb/v.sock,volume=x,\
             ring-shape=1:256:4096:1:8:16:8:2:d,bootindex=1",
            "30:0,fbuf,vga=off,unix=/tmp/vm.vnc,password=cGFzcw==",
            "30:1,xhci,tablet",
            "31,lpc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let specs = driver_specs(true, &raw).expect("a destination");
        assert_eq!(specs.len(), raw.len());
    }

    #[test]
    fn a_mantablock_destination_needs_the_source_shape() {
        // bootindex= in either position must not hide the ring option.
        for ring in [
            "5,nvme,mantablock=/s,volume=x,bootindex=1",
            "5,nvme,bootindex=1,mantablock=/s,volume=x",
        ] {
            let raw = vec!["4,nvme,/disk".to_string(), ring.to_string()];
            let error = driver_specs(true, &raw).unwrap_err();
            assert!(
                error.to_string().contains("ring-shape="),
                "{ring}: {error}"
            );
            assert!(driver_specs(false, &raw).is_ok());
        }
        let shaped = vec![
            "5,nvme,mantablock=/s,volume=x,ring-shape=1:256:4096:1:8:16:8:2:d"
                .to_string(),
        ];
        assert!(driver_specs(true, &shaped).is_ok());
    }

    fn opts(shape: Option<&str>) -> devspec::MantaBlockOpts {
        devspec::MantaBlockOpts {
            socket: "/s".into(),
            volume: "6f1c2a8e-0000-4000-8000-000000000007"
                .parse()
                .expect("a UUID"),
            allow_volatile: false,
            spin_us: None,
            ring_shape: shape.map(str::to_string),
        }
    }

    #[test]
    fn a_ring_shape_is_parsed_strictly_and_required_on_a_destination() {
        let none = opts(None);
        let error = ring_shape(&none, true).expect_err("a destination");
        assert!(error.to_string().contains(&none.volume.to_string()));
        assert_eq!(ring_shape(&none, false).expect("not a destination"), None);

        let good = opts(Some("1:256:4096:1:8:16:8:2:d"));
        let shape = ring_shape(&good, true).expect("a shape").expect("some");
        // Elsewhere it would build a disk that never attaches.
        assert!(ring_shape(&good, false).is_err());
        assert_eq!(shape.to_string(), "1:256:4096:1:8:16:8:2:d");
        assert!(ring_shape(&opts(Some("1:256")), true).is_err());
    }

    #[test]
    fn reject_unmigratable_destination_allows_normal_specs() {
        let specs = vec![
            "4,virtio-blk,/dev/rdsk/test".to_string(),
            "5,nvme,/tmp/fbuf".to_string(),
        ];

        assert!(reject_unmigratable_destination(true, &specs).is_ok());
    }
}
