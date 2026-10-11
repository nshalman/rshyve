// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vmm - New Rust-based bhyve VMM with live migration support.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use anyhow::Context;
use slog::{info, o, warn, Logger};

use vmm_boot::direct;
use vmm_config::Cli;
use vmm_core::machine::Machine;
use vmm_core::msr::MsrHandler;
use vmm_core::unixsock::peercred::{install_policy, ListenerOwner, PeerPolicy};
use vmm_devices::InputBroker;
use vmm_machine::privilege::{OpenWitness, PrivPlan, RunAs};
use vmm_machine::{
    devspec, DeviceRegistry, HotplugEngine, HotplugEngines, RegisteredDevice,
    RunOutcome, VcpuSetup,
};

mod bootorder;
mod bootrom;
mod control;
mod create;
mod host_state;
mod input;
mod mdata;
mod msix;
mod pcidev;
mod reboot;
mod secret;
mod serial;
mod varstore;

use create::{create_machine, MachineCreation};
use input::setup_ps2;
use reboot::reexec_for_reboot;

/// Kernel command line used when neither `--cmdline` nor
/// `--cmdline-base64` is given. It names `ttyS0`, which is why COM1 is
/// always attached.
use vmm_config::DEFAULT_CMDLINE;

/// The guest kernel command line, from whichever spelling was given.
///
/// Both spellings resolve through `Cli::kernel_cmdline`, which is also
/// where the length bound and the NUL and control-byte checks live.
/// Reading `cli.cmdline` here instead would accept
/// `--cmdline-base64` and then boot the guest without it.
fn resolved_cmdline(cli: &Cli) -> anyhow::Result<String> {
    Ok(cli
        .kernel_cmdline()?
        .unwrap_or_else(|| DEFAULT_CMDLINE.to_string()))
}

/// If direct-booting a kernel, load it into guest RAM and return the
/// entry point address.
fn load_direct_boot_kernel(
    machine: &Machine,
    cli: &Cli,
    kernel_image: &Option<vmm_boot::BootImage>,
    initrd_image: &Option<direct::InitrdImage>,
    mem_size: usize,
    acpi_tables: vmm_machine::AcpiTables,
    log: &Logger,
) -> anyhow::Result<()> {
    if let Some(ref kernel) = kernel_image {
        let cmdline = resolved_cmdline(cli)?;
        let acpi_rsdp_addr = acpi_tables.rsdp_addr();
        let entry = kernel.load(
            machine.memctx(),
            &cmdline,
            initrd_image.as_ref(),
            mem_size,
            acpi_rsdp_addr,
        )?;
        info!(log, "kernel loaded for direct boot";
            "protocol" => format!("{:?}", kernel.protocol()),
            "entry" => format!("{:#x}", entry),
            "cmdline" => cmdline,
        );
    }
    Ok(())
}

/// Every device a migration carries state for, with the PCI address
/// that identifies it on the wire.
fn migrate_devices(
    registry: &DeviceRegistry,
) -> Vec<(
    vmm_migrate::codec::DeviceIdentity,
    Arc<dyn vmm_devices::Lifecycle>,
)> {
    registry
        .list()
        .into_iter()
        .filter_map(|slot| {
            let bdf = slot.bdf?;
            let device = slot.lifecycle?;
            Some((
                vmm_migrate::codec::DeviceIdentity {
                    bdf: bdf.into(),
                    kind: device.type_name().to_string(),
                },
                device,
            ))
        })
        .collect()
}

/// The spec check in pcidev already refused these. This one reads the
/// devices themselves, so a driver that check does not know about is
/// refused too.
fn refuse_unmigratable_devices(
    registry: &DeviceRegistry,
) -> anyhow::Result<()> {
    match control::device_blockers(registry) {
        Some(why) => anyhow::bail!(why),
        None => Ok(()),
    }
}

/// Handle migration destination mode: activate vCPUs, take a migration
/// connection, and import VM state from the source.
fn run_migration_destination(
    machine: &Machine,
    listen_addr: &str,
    num_cpus: u32,
    mem_size: usize,
    registry: &DeviceRegistry,
    cpu_baseline: vmm_core::cpuid::CpuBaseline,
    hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    log: &Logger,
) -> anyhow::Result<()> {
    info!(log, "migration destination mode"; "listen" => listen_addr);

    refuse_unmigratable_devices(registry)?;

    // Activate and reset vCPUs. The reset initializes the VMCS/VMCB
    // host-state fields. The migration import then overwrites the
    // guest-state fields with the source's values.
    for vcpu in machine.vcpus() {
        vcpu.activate().with_context(|| {
            format!("failed to activate vCPU {}", vcpu.id())
        })?;
        vcpu.reboot_state()
            .with_context(|| format!("failed to reset vCPU {}", vcpu.id()))?;
    }

    let devices = migrate_devices(registry);
    let config = vmm_migrate::destination::DestConfig {
        num_cpus,
        mem_size: mem_size as u64,
        cpu_baseline,
        devices: devices.iter().map(|(ident, _)| ident.clone()).collect(),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        // The control socket's migrate-dest carries the override. A VM
        // started this way has no operator to ask mid-migration.
        allow_cpu_feature_mismatch: false,
    };

    let by_bdf = devices.clone();
    let for_resume: Vec<Arc<dyn vmm_devices::Lifecycle>> =
        registry.lifecycle_devices();
    let (pause_vm, resume_vm) = vmm_migrate::hdl_pause_hooks(machine.hdl());
    let hooks = vmm_migrate::destination::DestHooks {
        restore: Box::new(move |bdf, state| {
            let device = by_bdf
                .iter()
                .find(|(ident, _)| ident.bdf == bdf)
                .map(|(_, device)| device)
                .ok_or_else(|| {
                    vmm_devices::lifecycle::DeviceStateError::Invalid(format!(
                        "no device at {bdf}"
                    ))
                })?;
            device.restore_migrate_state(state)
        }),
        resume: Box::new(move || {
            for dev in &for_resume {
                dev.resume();
            }
        }),
        hyperv: Box::new(move |state| match hyperv.as_ref() {
            Some(hyperv) => {
                hyperv.import_state(state);
                true
            }
            // The payload carries an enlightenment this VM has no
            // handler for, so the guest would find its hypercall page
            // gone. A mismatch, not a silent drop.
            None => false,
        }),
        pause_vm,
        resume_vm,
        // No control plane exists yet to cancel this import.
        gate: Box::new(vmm_migrate::NoStop),
    };

    let status = Arc::new(std::sync::Mutex::new(
        vmm_migrate::MigrationStatus::default(),
    ));
    let memctx = vmm_core::mem::MemCtx::new(machine.physmap().clone());
    let hdl = machine.hdl().clone();

    // One driver over either stream. A Unix socket is the only kind
    // that works inside a bhyve zone, where /dev/poll is unavailable;
    // the GZ agent bridges it to the peer.
    if listen_addr.starts_with('/') || listen_addr.ends_with(".sock") {
        let stream = connect_migration_socket(listen_addr, log)?;
        vmm_migrate::destination::run_destination(
            stream, hdl, &memctx, config, hooks, status, log,
        )
        .context("migration import failed")?;
    } else {
        let listener = std::net::TcpListener::bind(listen_addr)
            .with_context(|| format!("bind {listen_addr}"))?;
        info!(log, "waiting for migration"; "addr" => listen_addr);
        let (stream, peer) = listener.accept().context("accept failed")?;
        info!(log, "migration connection"; "peer" => %peer);
        vmm_migrate::destination::run_destination(
            stream, hdl, &memctx, config, hooks, status, log,
        )
        .context("migration import failed")?;
    }

    // The run state was restored per vCPU during the import. A vCPU in
    // HLT or INIT stays there and the kernel resumes it when needed.
    // The interrupt kick happens after the vCPU threads are spawned:
    // one injected before the spawn is lost.
    info!(log, "migration import complete, starting vCPUs");
    Ok(())
}

/// Connect to the agent's socket, waiting for it to appear.
///
/// The agent and the VMM start together, so the socket may not exist
/// for the first moments.
fn connect_migration_socket(
    path: &str,
    log: &Logger,
) -> anyhow::Result<std::os::unix::net::UnixStream> {
    const RETRIES: u32 = 30;
    info!(log, "connecting to migration socket"; "path" => path);
    for _ in 0..RETRIES {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(stream) => {
                info!(log, "connected to migration socket");
                return Ok(stream);
            }
            Err(_) => thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
    anyhow::bail!("no migration socket at {path} after {RETRIES} tries")
}

/// What the control socket leaves behind for the rest of `run`.
#[derive(Default)]
struct ControlSocket {
    /// The listener. Dropping it stops the accept loop.
    listener: Option<control::ControlListener>,
    /// How SIGTERM stops the VM. `None` without a control socket,
    /// which is the only kind of VM that cannot be paused.
    stop: Option<Arc<dyn vmm_machine::signal::GuestStop>>,
}

/// Optionally spawn the control socket listener thread.
fn spawn_control_socket(
    machine: &Machine,
    cli: &Cli,
    num_cpus: u32,
    max_cpus: u32,
    mem_size: usize,
    cpu_baseline: vmm_core::cpuid::CpuBaseline,
    vcpu_metrics: &[Arc<vmm_devices::metrics::VcpuMetrics>],
    registry: Arc<DeviceRegistry>,
    hotplug: HotplugEngines,
    migration: control::MigrationInputs,
    log: &Logger,
) -> anyhow::Result<ControlSocket> {
    let Some(sock_path) = cli.control_socket.as_ref() else {
        return Ok(ControlSocket::default());
    };
    let ctrl = control::VmController::new(
        machine.hdl().clone(),
        machine.physmap().clone(),
        cli.vm_name.clone(),
        num_cpus,
        max_cpus,
        mem_size,
        vcpu_metrics.to_vec(),
        registry,
        hotplug,
        cli.pci_slot.clone(),
        cli.lpc.clone(),
        cpu_baseline,
        migration,
        log.clone(),
    );
    let stop = control::sigterm_stop(Arc::clone(&ctrl));
    let listener = control::spawn_control_thread(sock_path, ctrl, log.clone())?;
    info!(log, "control socket listening";
        "path" => listener.path().display().to_string());
    Ok(ControlSocket {
        listener: Some(listener),
        stop: Some(stop),
    })
}

/// What `run` hands `main`.
///
/// The hot-added specs travel with the outcome because only `run` can
/// rshyve keeps its inherited privileges unless told otherwise: the
/// control socket, migration and the vTPM all open host files after
/// start, and the drop would break them.
const PRIV_DROP_DEFAULT: bool = false;

/// The privilege lock rshyve was asked for, checked against the
/// features the lock would break.
#[derive(Debug, PartialEq, Eq)]
struct PrivRequest {
    drop: bool,
    run_as: Option<RunAs>,
}

/// Read `--priv-drop` and refuse it beside a feature that opens host
/// files after start. Pure, so the rule is testable without a VM.
fn priv_request(cli: &Cli) -> anyhow::Result<PrivRequest> {
    let drop = cli.priv_drop_or(PRIV_DROP_DEFAULT);
    if !drop {
        anyhow::ensure!(
            cli.run_as_uid.is_none() && cli.run_as_gid.is_none(),
            "--run-as-uid and --run-as-gid need --priv-drop"
        );
        return Ok(PrivRequest { drop, run_as: None });
    }
    // virtio-fs serves FUSE from this process and opens host files for
    // the life of the VM, which the floor has no file_write for.
    // firehyve refuses the same combination.
    if let Some(spec) = cli
        .pci_slot
        .iter()
        .find(|spec| devspec::parts(spec).driver == "virtio-fs")
    {
        anyhow::bail!("virtio-fs cannot run under the privilege lock; remove '-s {spec}'");
    }
    let blockers: Vec<&str> = [
        ("--control-socket", cli.control_socket.is_some()),
        ("--migrate-listen", cli.migrate_listen.is_some()),
        ("--vtpm", cli.vtpm),
        ("--mdata-root-pw-file", cli.mdata_root_pw_file.is_some()),
    ]
    .into_iter()
    .filter_map(|(flag, set)| set.then_some(flag))
    .collect();
    anyhow::ensure!(
        blockers.is_empty(),
        "--priv-drop cannot be combined with {}: each opens host files \
         after the lock",
        blockers.join(", ")
    );
    let run_as = cli.run_as_or(None)?.map(|(uid, gid)| RunAs { uid, gid });
    Ok(PrivRequest { drop, run_as })
}

/// What the argv attached, for the lock's open-device witness.
fn witness_ids(specs: &[String]) -> Vec<String> {
    specs
        .iter()
        .filter(|spec| {
            // The same grammar the attachment parser reads, which trims
            // the driver. An untrimmed compare here would put the
            // chipset of `-s "0, hostbridge"` into the witness, and the
            // lock would then refuse to start a VM whose argv is legal.
            !matches!(devspec::parts(spec).driver, "hostbridge" | "lpc" | "")
        })
        .map(|spec| devspec::device_id(spec))
        .collect()
}

/// see the registry, and only `main` has argv to rebuild.
struct RunResult {
    outcome: RunOutcome,
    /// `-s` specs for the devices an operator added while the VM ran.
    hotplug_specs: Vec<String>,
    /// Whether the privilege lock ran. A locked process has no
    /// `proc_exec`, so the reboot path must not try to re-exec itself.
    locked: bool,
}

fn run(cli: Cli, log: &Logger) -> anyhow::Result<RunResult> {
    // The inherited set is read before any device opens; the lock runs
    // after the last one. Off by default here, and said so.
    let privilege = priv_request(&cli)?;
    let plan =
        if privilege.drop {
            Some(PrivPlan::begin_from_process().map_err(|e| {
                anyhow::anyhow!("privilege drop cannot start: {e}")
            })?)
        } else {
            info!(log, "privilege drop: off";
            "reason" => "rshyve default; pass --priv-drop to lock");
            None
        };
    // Before the first socket is bound, naming the uid the process
    // ends at.
    let owner = privilege.run_as.map(|r| ListenerOwner {
        uid: r.uid,
        gid: r.gid,
    });
    if install_policy(PeerPolicy::for_process(owner)?).is_err() {
        anyhow::bail!("the peer policy was installed twice");
    }

    // Read once. The MADT slot count, the CPU register file and the
    // hot-add engine all read this, so the guest cannot be told about
    // a slot no engine can fill.
    let vm_opts = vmm_machine::VmOpts::from_cli(&cli)?;
    // The hot-add window is built before the VM exists, so a window an
    // operator cannot use costs no boot.
    let mem_window = vmm_machine::mem_hotplug_window(&vm_opts)?;

    // Phase 1: Create VM, load bootrom/kernel, finalize memory
    let creation = create_machine(&cli, log)?;
    let MachineCreation {
        machine,
        api_version,
        num_cpus,
        mem_size,
        kernel_image,
        initrd_image,
        hyperv,
        varstore,
        tpm_acpi,
        cpuid,
        cpu_baseline,
        host_local_state,
    } = creation;
    let max_cpus = vm_opts.max_cpus;

    // Phase 2: Initialize chipset and kernel-emulated devices
    let chipset_devs = vmm_machine::init_chipset(&machine, log)?;

    // Phase 3: Timers, PM, pvpanic
    let timer_pm_devs = vmm_machine::setup_timers_and_pm(
        &machine,
        vmm_machine::HotplugOpts::from_vm(&vm_opts),
        log,
    )?;

    let input = Arc::new(InputBroker::default());

    // Phase 4: Serial ports and mdata agent
    let (_com1, _com2) =
        serial::setup_serial(&machine, &chipset_devs.pic, &cli, log)?;
    let _ps2 = setup_ps2(
        &machine,
        &chipset_devs.pic,
        Arc::clone(&timer_pm_devs.suspend_sink),
        &input,
        log,
    );

    // Phase 5: fw_cfg, ACPI tables, SMBIOS, E820, boot order
    let (_fwcfg, acpi_tables) = vmm_machine::setup_fwcfg_and_acpi(
        &machine,
        &vm_opts,
        num_cpus,
        mem_size,
        bootorder::build_bootorder(&cli.pci_slot)?,
        tpm_acpi.clone(),
        log,
    )?;

    // Phase 6: PCI devices
    let (registry, has_ahci_cd, pci_ctx) = pcidev::init_pci_devices(
        &machine,
        &chipset_devs.chipset,
        &cli,
        num_cpus,
        &input,
        log,
    )?;
    // The PM phase runs before the registry exists, so its handles are
    // pushed here. Without this the GPE0 block takes no part in reset
    // and its migration payload is never asked for.
    vmm_machine::register_pm_devices(&registry, &timer_pm_devs)
        .context("failed to register the PM devices")?;

    if let Some(vs) = varstore.clone() {
        // The firmware phase builds the variable store, so it never
        // passes through the `-s` factory and has to be registered here.
        registry
            .insert(RegisteredDevice::new(
                "varstore",
                None,
                None,
                Some(vs as Arc<dyn vmm_devices::Lifecycle>),
                None,
            ))
            .context("failed to register the UEFI variable store")?;
    }

    // Phase 7: Direct boot kernel loading (after ACPI tables are ready)
    load_direct_boot_kernel(
        &machine,
        &cli,
        &kernel_image,
        &initrd_image,
        mem_size,
        acpi_tables,
        log,
    )?;

    // Phase 8: vCPU activation (migration or normal)
    if let Some(ref listen_addr) = cli.migrate_listen {
        run_migration_destination(
            &machine,
            listen_addr,
            num_cpus,
            mem_size,
            &registry,
            cpu_baseline,
            hyperv.clone(),
            log,
        )?;
    } else if let Some(ref kernel) = kernel_image {
        // The closure is a named local because `BspEntry::Custom` only
        // borrows it.
        let setup_bsp = |bsp: &vmm_core::vcpu::Vcpu| kernel.setup_bsp(bsp);
        vmm_machine::activate_vcpus(
            &machine,
            cli.vmexit_on_hlt,
            vmm_machine::BspEntry::Custom(&setup_bsp),
            log,
        )?;
    } else {
        vmm_machine::activate_vcpus(
            &machine,
            cli.vmexit_on_hlt,
            vmm_machine::BspEntry::Firmware,
            log,
        )?;
    }

    // The guest can run now, so tell the brand. `vmadm create` waits on
    // this rename, not on the process, and gives up after 300 s. C
    // bhyve does it at the same point, bhyverun.c:915.
    vmm_machine::provision::mark_provisioned(log);

    // Phase 8b: lock, when asked. Every device is open and the brand
    // has its rename.
    if let Some(plan) = plan.as_ref() {
        let witness = OpenWitness {
            vmm_open: true,
            expected_devices: witness_ids(&cli.pci_slot),
            present_devices: registry
                .list()
                .into_iter()
                .map(|device| device.id)
                .collect(),
        };
        let report = plan
            .lock(&witness, privilege.run_as)
            .map_err(|e| anyhow::anyhow!("privilege drop failed: {e}"))?;
        info!(log, "privilege drop: on"; "sets" => report.summary());
    }

    // Phase 9: Spawn vCPU threads. The fleet, not the fixed set,
    // because a CPU added later joins the same event channel and the
    // same roster.
    let fleet = vmm_machine::spawn_vcpu_fleet(
        &machine,
        num_cpus,
        api_version,
        hyperv.clone().map(|hv| hv as Arc<dyn MsrHandler>),
        log,
    )
    .context("failed to start the boot vCPU threads")?;
    let vcpu_metrics = fleet.metrics.clone();

    // Phase 10: wake the guest's virtio drivers once the vCPUs run.
    //
    // The restore raises an interrupt per live queue, but that one
    // fires before the vCPUs start and a driver that is not running
    // cannot take it. This second one reaches a running driver, which
    // is what makes it process the caught-up used ring and post new
    // receive buffers.
    if cli.migrate_listen.is_some() {
        let devs = registry.lifecycle_devices();
        let kick_log = log.clone();
        if let Err(e) = thread::Builder::new()
            .name("post-migrate-virtio-kick".into())
            .spawn(move || {
                thread::sleep(std::time::Duration::from_millis(100));
                for dev in &devs {
                    dev.post_restore_kick();
                }
            })
        {
            // The guest keeps its rings; it may just not notice the
            // entries waiting in them until it kicks a queue itself.
            warn!(kick_log, "could not start the post-migration kick thread";
                "error" => %e);
        }
    }

    // Phase 11: Hotplug engines. Each owns the thread that answers the
    // guest, so no teardown ever runs on a vCPU thread.
    let pci_hotplug = timer_pm_devs.hotplug.as_ref().map(|regs| {
        HotplugEngine::start(
            Arc::clone(&registry),
            pci_ctx,
            Arc::clone(&regs.pci),
            pcidev::hotplug_factory(&cli, num_cpus),
            log.new(o!("component" => "hotplug")),
        )
    });
    let cpu_hotplug = vmm_machine::start_cpu_hotplug(
        timer_pm_devs.hotplug.as_ref(),
        VcpuSetup {
            max_cpus,
            boot_cpus: num_cpus,
            vmexit_on_hlt: cli.vmexit_on_hlt,
            cpuid: cpuid.clone(),
        },
        &fleet,
        log,
    );
    let hotplug = HotplugEngines {
        pci: pci_hotplug,
        cpu: cpu_hotplug,
        mem: vmm_machine::start_mem_hotplug(
            mem_window,
            &machine,
            timer_pm_devs.hotplug.as_ref(),
            log,
        )?,
    };

    // Phase 12: Control socket
    let control_socket = spawn_control_socket(
        &machine,
        &cli,
        num_cpus,
        max_cpus,
        mem_size,
        cpu_baseline,
        &vcpu_metrics,
        Arc::clone(&registry),
        hotplug.clone(),
        control::MigrationInputs {
            has_ahci_cd,
            hyperv: hyperv.clone(),
            host_local_state,
        },
        log,
    )?;

    // Phase 13: Signal handling. The control-plane stop is preferred
    // because only it knows whether the VM is parked on the kernel's
    // pause, where a suspend alone is never seen.
    let stop = control_socket.stop.clone().unwrap_or_else(|| {
        Arc::new(vmm_machine::signal::HdlStop::new(machine.hdl().clone()))
    });
    vmm_machine::signal::install_sigterm_handler_with(stop, log);

    // Phase 14: Main event loop and cleanup
    let outcome = vmm_machine::run_fleet_event_loop(
        &machine,
        fleet,
        &hotplug,
        control_socket
            .listener
            .as_ref()
            .map(|listener| listener.path().to_path_buf()),
        &registry,
        log,
    );

    // Stop taking commands before the engines go: a request that lands
    // after the VM is destroyed has nothing left to act on.
    drop(control_socket);
    hotplug.shutdown();

    // Read after the engine stops, so a teardown that was in flight has
    // finished and cannot leave a departed device in the list.
    let hotplug_specs = vmm_machine::hotplug_specs(&registry);
    outcome.map(|outcome| RunResult {
        outcome,
        hotplug_specs,
        locked: privilege.drop,
    })
}

/// How long the exit path waits for the console writer thread. Bounded
/// so a consumer that stopped reading cannot hold the VMM open.
const CONSOLE_FLUSH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Version string including git commit hash (set by build.rs).
fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("VMM_GIT_COMMIT");
    format!("{version} ({commit})")
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let exe = std::env::current_exe().unwrap_or_else(|_| {
        args.first().map(PathBuf::from).unwrap_or_default()
    });

    if vmm_machine::entry::version_requested(&args) {
        println!("rshyve {}", version_string());
        return ExitCode::SUCCESS;
    }
    vmm_machine::entry::register_probes("rshyve");

    let cli = Cli::parse_named("rshyve");
    let log = vmm_machine::entry::setup_logger("rshyve");

    let result = run(cli, &log);
    // The console writer thread holds the guest's last bytes. Both the
    // reexec below and a plain exit would drop them.
    if !vmm_devices::uart::backend::flush_stdout(CONSOLE_FLUSH_TIMEOUT) {
        warn!(log, "console output was still draining at exit");
    }
    match result {
        // A locked process cannot re-exec: the floor has no proc_exec,
        // so the call would fail and the guest's reset would read as a
        // crash. Report the reset instead and let whatever started this
        // process start it again.
        Ok(RunResult {
            outcome: RunOutcome::Reboot,
            locked: true,
            ..
        }) => {
            info!(
                log,
                "guest reset under the privilege lock; exiting for a restart"
            );
            vmm_machine::entry::exit_code(RunOutcome::Reboot, &log)
        }
        Ok(RunResult {
            outcome: RunOutcome::Reboot,
            hotplug_specs,
            ..
        }) => reexec_for_reboot(
            &exe,
            &args,
            &hotplug_specs,
            vmm_machine::sigterm_flag(),
            &log,
        ),
        Ok(RunResult { outcome, .. }) => {
            vmm_machine::entry::exit_code(outcome, &log)
        }
        Err(e) => {
            slog::error!(log, "fatal error"; "error" => format!("{:#}", e));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::FromArgMatches;

    fn cli_from(args: &[&str]) -> Cli {
        let matches = Cli::command_named("rshyve")
            .try_get_matches_from(args)
            .expect("argv parses");
        Cli::from_arg_matches(&matches).expect("from_arg_matches")
    }

    #[test]
    fn the_drop_is_off_by_default_for_rshyve() {
        let cli = cli_from(&["rshyve", "guest"]);
        assert_eq!(
            priv_request(&cli).unwrap(),
            PrivRequest {
                drop: false,
                run_as: None
            }
        );
    }

    #[test]
    fn the_drop_can_be_asked_for_with_or_without_an_identity() {
        let cli = cli_from(&["rshyve", "--priv-drop", "guest"]);
        assert_eq!(
            priv_request(&cli).unwrap(),
            PrivRequest {
                drop: true,
                run_as: None
            }
        );
        let cli = cli_from(&[
            "rshyve",
            "--priv-drop",
            "--run-as-uid",
            "5000",
            "--run-as-gid",
            "5000",
            "guest",
        ]);
        assert_eq!(
            priv_request(&cli).unwrap().run_as,
            Some(RunAs {
                uid: 5000,
                gid: 5000
            })
        );
    }

    /// Each of these opens a host file after start, which the lock
    /// forbids. Refused up front and named, not left to fail later.
    #[test]
    fn the_drop_is_refused_beside_a_feature_it_would_break() {
        for (flag, extra) in [
            ("--control-socket", vec!["--control-socket", "/tmp/c.sock"]),
            ("--migrate-listen", vec!["--migrate-listen", "0.0.0.0:1"]),
            ("--vtpm", vec!["--vtpm"]),
        ] {
            let mut argv = vec!["rshyve", "--priv-drop"];
            argv.extend(extra);
            argv.push("guest");
            let err = priv_request(&cli_from(&argv)).expect_err(flag);
            assert!(err.to_string().contains(flag), "{flag}: {err}");
        }
    }

    #[test]
    fn the_witness_skips_the_chipset_names() {
        let specs: Vec<String> =
            ["0,hostbridge", "4,virtio-blk,/d.img", "31,lpc"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(witness_ids(&specs), vec!["virtio-blk@4"]);
    }

    /// The attachment parser trims the driver, so the witness must too:
    /// a chipset named with a space would otherwise be expected in the
    /// registry it never enters, and the lock would refuse a legal argv.
    #[test]
    fn a_spaced_chipset_stays_out_of_the_witness() {
        let specs = vec![
            "0, hostbridge".to_string(),
            "31, lpc".to_string(),
            "4, virtio-blk,/dev/zvol/rdsk/z/disk0".to_string(),
        ];
        assert_eq!(witness_ids(&specs), vec!["virtio-blk@4"]);
    }

    /// virtio-fs opens host files for the life of the VM, which the
    /// floor has no file_write for. firehyve refuses the same pair.
    #[test]
    fn virtio_fs_is_refused_under_the_lock() {
        let cli = Cli::try_parse_named(
            "rshyve",
            ["rshyve", "--priv-drop", "-s", "5,virtio-fs,/export", "guest"],
        )
        .expect("argv parses");
        let error = priv_request(&cli).expect_err("virtio-fs must be refused");
        assert!(
            error.to_string().contains("virtio-fs"),
            "the refusal names the device: {error}"
        );
    }

    /// The bhyve zone brand splits its only pass-through attr on
    /// whitespace, so a command line with spaces can only reach this
    /// binary base64-encoded. Reading `cli.cmdline` directly accepts
    /// the flag and boots the guest without it.
    ///
    /// Mutation this kills: `cli.cmdline.as_deref().unwrap_or(DEFAULT)`
    /// in place of the resolver, which returns the default here.
    #[test]
    fn the_base64_command_line_reaches_the_guest() {
        let cli = cli_from(&[
            "rshyve",
            "--cmdline-base64",
            "cm9vdD0vZGV2L3ZkYTEgcnc=",
            "vm",
        ]);
        assert_eq!(
            resolved_cmdline(&cli).expect("resolves"),
            "root=/dev/vda1 rw",
        );
    }

    /// Mutation this kills: dropping the resolver's `?` so a command
    /// line that fails validation is passed to the loader anyway.
    #[test]
    fn a_control_byte_in_the_command_line_stops_the_boot() {
        let cli =
            cli_from(&["rshyve", "--cmdline", "root=/dev/vda1\x07", "vm"]);
        resolved_cmdline(&cli).expect_err("a control byte must be refused");
    }

    /// Without a command line the guest still has to be told where its
    /// console is, or a direct boot produces no output at all.
    #[test]
    fn no_command_line_at_all_names_the_serial_console() {
        let cli = cli_from(&["rshyve", "vm"]);
        assert_eq!(resolved_cmdline(&cli).expect("resolves"), DEFAULT_CMDLINE);
        assert!(DEFAULT_CMDLINE.contains("ttyS0"), "console is not ttyS0");
    }

    /// The binary must not carry its own copy of the boot images.
    /// This coerces only if the parameter types are the ones `vmm_boot`
    /// owns, so a reintroduced binary-local module is a compile error,
    /// not a silent second copy.
    ///
    /// The paths stay fully qualified so it fails on a type mismatch,
    /// not on name resolution. The import added alongside makes that
    /// qualification redundant under this workspace's
    /// `unused_qualifications` lint, so allow it here.
    #[test]
    #[allow(unused_qualifications)]
    fn boot_types_come_from_vmm_boot() {
        let _load: fn(
            &Machine,
            &Cli,
            &Option<vmm_boot::BootImage>,
            &Option<vmm_boot::direct::InitrdImage>,
            usize,
            vmm_machine::AcpiTables,
            &Logger,
        ) -> anyhow::Result<()> = load_direct_boot_kernel;

        // BSP setup travels with the image, so no caller can pair one
        // protocol's entry point with the other's register state.
        let _bsp: fn(
            &vmm_boot::BootImage,
            &vmm_core::vcpu::Vcpu,
        ) -> anyhow::Result<()> = vmm_boot::BootImage::setup_bsp;
    }

    struct Blocks(Option<&'static str>);

    impl vmm_devices::Lifecycle for Blocks {
        fn type_name(&self) -> &'static str {
            "nvme"
        }

        fn migration_blocker(&self) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    fn registry_of(blocker: Option<&'static str>) -> DeviceRegistry {
        let registry = DeviceRegistry::new();
        registry
            .insert(RegisteredDevice::new(
                "nvme@5",
                vmm_machine::parse_bdf("5"),
                None,
                Some(Arc::new(Blocks(blocker))
                    as Arc<dyn vmm_devices::Lifecycle>),
                None,
            ))
            .expect("insert");
        registry
    }

    #[test]
    fn a_destination_refuses_a_device_that_cannot_migrate() {
        let error = refuse_unmigratable_devices(&registry_of(Some("a ring")))
            .expect_err("a blocker");
        assert!(error.to_string().contains("nvme@5: a ring"), "{error}");
        refuse_unmigratable_devices(&registry_of(None)).expect("no blocker");
    }

    #[test]
    fn bootorder_indexes_ahci_cd() {
        let specs = vec!["5,ahci-cd,/x.iso,bootindex=1".to_string()];

        assert_eq!(
            bootorder::build_bootorder(&specs).unwrap(),
            b"/pci@i0cf8/pci@5,0\n"
        );
    }
}
