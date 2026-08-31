// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! firehyve: a microVM built on the shared vmm crates.
//!
//! Direct-kernel boot, virtio devices only. No UEFI, no migration, no
//! control socket, no PCI passthrough.

mod console;
mod devices;

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;
use std::sync::Arc;

use slog::{error, info, Drain};

use vmm_config::Cli;

/// Kernel command line used when `--cmdline` is absent. It names
/// `ttyS0`, which is why COM1 is always attached.
const DEFAULT_CMDLINE: &str = "console=ttyS0 earlyprintk=serial";

fn setup_logger() -> slog::Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    slog::Logger::root(drain, slog::o!("component" => "firehyve"))
}

fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("FIREHYVE_GIT_COMMIT");
    format!("{version} ({commit})")
}

/// Reject every shared-grammar flag firehyve cannot honor, so an
/// operator learns at startup instead of from a guest that misbehaves.
fn validate_cli(cli: &Cli) -> anyhow::Result<()> {
    anyhow::ensure!(
        cli.migrate_listen.is_none(),
        "firehyve does not support --migrate-listen; live migration is rshyve only"
    );
    anyhow::ensure!(
        cli.control_socket.is_none(),
        "firehyve does not support --control-socket"
    );
    anyhow::ensure!(
        cli.json_config.is_none(),
        "firehyve does not support --json-config"
    );
    anyhow::ensure!(
        !cli.vtpm && cli.vtpm_state_dir.is_none(),
        "firehyve does not support --vtpm; vTPM is rshyve only"
    );
    anyhow::ensure!(
        !cli.hyperv && cli.tsc_freq_hz.is_none(),
        "firehyve does not support --hyperv; enlightenments are rshyve only"
    );
    anyhow::ensure!(
        cli.mdata_nics.is_none()
            && cli.mdata_resolvers.is_none()
            && cli.mdata_ssh_keys.is_none()
            && cli.mdata_root_pw.is_none(),
        "firehyve does not support the --mdata-* flags; there is no COM2 agent"
    );
    if let Some(entry) = cli.lpc.iter().find(|a| a.starts_with("bootrom,")) {
        anyhow::bail!("firehyve has no UEFI path; '-l {entry}' is rshyve only");
    }
    console::require_stdio_backend(&cli.lpc)?;
    // attach_pci_devices takes bootindex-stripped specs, and a
    // direct-booted kernel has no firmware boot order for an index to
    // steer. Accepting the token would silently drop it.
    if let Some(spec) = cli
        .pci_slot
        .iter()
        .find(|s| s.split(',').any(|f| f.starts_with("bootindex=")))
    {
        anyhow::bail!(
            "firehyve boots the kernel directly and has no boot order; \
             remove bootindex from '-s {spec}'"
        );
    }
    anyhow::ensure!(
        cli.kernel.is_some(),
        "firehyve has no UEFI path; --kernel is required"
    );
    Ok(())
}

/// The phase sequence. Ordering is load-bearing: ACPI tables are written
/// before the kernel is loaded because the loader is handed the RSDP
/// address, and the SIGTERM handler is installed after the vCPU threads
/// exist so a power-off reaches a running VM.
fn run(
    cli: Cli,
    log: &slog::Logger,
) -> anyhow::Result<vmm_machine::RunOutcome> {
    // Phase 1: reject what this binary cannot honor.
    validate_cli(&cli)?;

    // Phase 2: create the VM. No bootrom, so nothing goes into guest
    // memory before finalize.
    let num_cpus = cli.num_cpus()?;
    let mem_size = cli.mem_size()?;
    let version = version_string();
    let opts = vmm_machine::MachineOpts {
        vm_name: &cli.vm_name,
        num_cpus,
        mem_size,
        create_opts: vmm_core::hdl::CreateOpts {
            force: true,
            use_reservoir: cli.wire_memory,
            // Dirty tracking exists to serve migration, which this
            // binary does not have.
            track_dirty: false,
        },
        banner: "firehyve starting",
        version: &version,
    };
    let mut pre_finalize = vmm_machine::no_pre_finalize;
    let build = vmm_machine::build_machine(&opts, &mut pre_finalize, log)?;
    let machine = build.machine;

    // CPUID stays outside build_machine because rshyve folds Hyper-V
    // leaves in here. firehyve has none, so this is the baseline only.
    let baseline = vmm_core::cpuid::CpuBaseline::parse(&cli.cpu_baseline)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown CPU baseline '{}' (valid: host, avx2, no-avx512, sse42)",
                cli.cpu_baseline,
            )
        })?;
    vmm_core::cpuid::apply_cpuid_baseline(
        machine.hdl(),
        num_cpus,
        baseline,
        log,
    )
    .map_err(|e| anyhow::anyhow!("failed to apply CPU baseline: {e}"))?;

    // Phase 3: chipset and kernel-emulated devices.
    let chipset_devs = vmm_machine::init_chipset(&machine, log)?;

    // Phase 4: timers, PM, pvpanic.
    let _timer_pm = vmm_machine::setup_timers_and_pm(&machine, log)?;

    // Phase 5: serial console, then the COM2 marker channel.
    let _com1 = console::setup_console(&machine, &chipset_devs.pic, &cli, log)?;
    let _com2 = console::setup_com2(&machine, &chipset_devs.pic, log)?;

    // Phase 6: fw_cfg, ACPI, SMBIOS. No boot order, no TPM2 table.
    let (_fwcfg, acpi) = vmm_machine::setup_fwcfg_and_acpi(
        &machine,
        &cli,
        num_cpus,
        mem_size,
        Vec::new(),
        None,
        log,
    )?;

    // Phase 7: PCI devices. An unknown driver name is an error, not a
    // silent omission.
    let input = Arc::new(vmm_devices::InputBroker::default());
    let ctx = vmm_machine::PciDeviceCtx::new(
        &machine,
        &chipset_devs.chipset,
        &input,
        log,
    );
    let mut factory = |spec: &str, ctx: &vmm_machine::PciDeviceCtx<'_>| {
        devices::create_pci_device(spec, num_cpus, ctx)
    };
    let attachment =
        vmm_machine::attach_pci_devices(&ctx, &cli.pci_slot, &mut factory)?;

    // Phase 8: load the kernel, then enter the BSP at its entry point.
    let kernel_path = cli
        .kernel
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--kernel is required"))?;
    // The protocol comes out of the file, so there is no --boot flag.
    // Reading the image and copying it into guest memory are timed
    // apart: they are the two largest items on the boot path and they
    // have different fixes, so one number for both hides which is
    // moving.
    let t_read = std::time::Instant::now();
    let kernel = vmm_boot::BootImage::open(kernel_path)?;
    let initrd = cli
        .initrd
        .as_deref()
        .map(vmm_boot::InitrdImage::open)
        .transpose()?;
    let read_us = t_read.elapsed().as_micros() as u64;

    let cmdline = cli.cmdline.as_deref().unwrap_or(DEFAULT_CMDLINE);
    let t_load = std::time::Instant::now();
    let entry = kernel.load(
        machine.memctx(),
        cmdline,
        initrd.as_ref(),
        mem_size,
        acpi.rsdp_addr(),
    )?;
    info!(log, "kernel loaded";
        "protocol" => format!("{:?}", kernel.protocol()),
        "entry" => format!("{entry:#x}"),
        "read_us" => read_us,
        "load_us" => t_load.elapsed().as_micros() as u64,
        "cmdline" => cmdline,
    );

    // `BspEntry::Custom` only borrows the closure, so it needs a name.
    let setup_bsp = |bsp: &vmm_core::vcpu::Vcpu| kernel.setup_bsp(bsp);
    vmm_machine::activate_vcpus(
        &machine,
        cli.vmexit_on_hlt,
        vmm_machine::BspEntry::Custom(&setup_bsp),
        log,
    )?;

    // Phase 9: run.
    let (event_rx, handles, _metrics) = vmm_machine::spawn_vcpus(
        &machine,
        num_cpus,
        build.api_version,
        None,
        log,
    );
    vmm_machine::install_sigterm_handler(&machine, log);
    vmm_machine::run_event_loop(
        &machine,
        event_rx,
        handles,
        None,
        &attachment.lifecycle_devices,
        log,
    )
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();

    // Answer --version before clap so it works with required args absent.
    if args.iter().any(|arg| {
        arg.as_os_str() == OsStr::new("--version")
            || arg.as_os_str() == OsStr::new("-V")
    }) {
        println!("firehyve {}", version_string());
        return ExitCode::SUCCESS;
    }

    // Non-fatal: /dev/dtrace/helper may not exist inside a zone.
    if let Err(e) = usdt::register_probes() {
        eprintln!("[firehyve] USDT probe registration skipped: {e}");
    }

    let cli = Cli::parse_named("firehyve");
    let log = setup_logger();

    match run(cli, &log) {
        // A reboot exits cleanly: firehyve has no re-exec path, matching
        // C bhyve's VM_SUSPEND_RESET handling.
        Ok(
            vmm_machine::RunOutcome::Reboot
            | vmm_machine::RunOutcome::PowerOff
            | vmm_machine::RunOutcome::Halt,
        ) => ExitCode::SUCCESS,
        Ok(vmm_machine::RunOutcome::TripleFault(src)) => {
            error!(log, "VM triple fault"; "source_vcpu" => src);
            ExitCode::from(3)
        }
        // Keep a guest-caused fatal exit distinct from an internal error.
        Ok(vmm_machine::RunOutcome::GuestFault) => ExitCode::from(2),
        Err(e) => {
            error!(log, "fatal error"; "error" => format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::FromArgMatches;

    fn cli_from(args: &[&str]) -> Cli {
        let matches = Cli::command_named("firehyve")
            .try_get_matches_from(args)
            .expect("argv parses");
        Cli::from_arg_matches(&matches).expect("from_arg_matches")
    }

    #[test]
    fn kernel_is_required() {
        let cli = cli_from(&["firehyve", "guest"]);
        let err = validate_cli(&cli).expect_err("no --kernel");
        assert!(err.to_string().contains("--kernel"), "got: {err}");
    }

    #[test]
    fn migration_flags_are_rejected() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "--migrate-listen",
            "0.0.0.0:4567",
            "guest",
        ]);
        let err = validate_cli(&cli).expect_err("migration is rshyve only");
        assert!(err.to_string().contains("--migrate-listen"), "got: {err}");
    }

    #[test]
    fn control_socket_is_rejected() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "--control-socket",
            "/tmp/ctl.sock",
            "guest",
        ]);
        let err = validate_cli(&cli).expect_err("no control socket");
        assert!(err.to_string().contains("--control-socket"), "got: {err}");
    }

    #[test]
    fn bootrom_lpc_entry_is_rejected() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "-l",
            "bootrom,/usr/share/bhyve/BHYVE_UEFI.fd",
            "guest",
        ]);
        let err = validate_cli(&cli).expect_err("no UEFI path");
        assert!(err.to_string().contains("bootrom"), "got: {err}");
    }

    #[test]
    fn a_bootindex_token_is_rejected() {
        // attach_pci_devices takes bootindex-stripped specs, and a
        // direct-booted kernel has no firmware boot order to steer.
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "-s",
            "4,virtio-blk,/tmp/d.img,bootindex=1",
            "guest",
        ]);
        let err = validate_cli(&cli).expect_err("no boot order");
        assert!(err.to_string().contains("bootindex"), "got: {err}");
    }

    #[test]
    fn a_console_backend_other_than_stdio_is_rejected() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "-l",
            "com1,/dev/zconsole",
            "guest",
        ]);
        let err = validate_cli(&cli).expect_err("stdio only");
        assert!(err.to_string().contains("/dev/zconsole"), "got: {err}");
    }

    #[test]
    fn a_plain_direct_boot_command_line_validates() {
        let cli = cli_from(&[
            "firehyve",
            "-c",
            "2",
            "-m",
            "512M",
            "-s",
            "4,virtio-blk,/tmp/d.img",
            "-l",
            "com1,stdio",
            "--kernel",
            "/tmp/vmlinux",
            "guest",
        ]);
        validate_cli(&cli).expect("accepted");
    }
}
