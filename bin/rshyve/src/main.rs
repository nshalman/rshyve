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
use vmm_core::hdl::CreateOpts;
use vmm_core::intr_pins::LegacyPIC;
use vmm_core::machine::{Machine, MachineSetup};
use vmm_core::msr::MsrHandler;
use vmm_devices::acpi;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::uart::backend::SerialBackend;
use vmm_devices::uart::lpc::{self, LpcUart};
use vmm_devices::InputBroker;
use vmm_machine::{
    build_machine, devspec, find_lpc_device, host_tsc_frequency_hz,
    CreatedPciDevice, DeviceRegistry, HotplugEngine, HotplugEngines,
    MachineOpts, RegisteredDevice, RunOutcome, VcpuSetup,
};
use vmm_virtio::attach::{self, VirtioAttachCtx};

mod bootorder;
mod bootrom;
mod control;
mod host_state;
mod input;
mod mdata;
mod msix;
mod peercred;
mod reboot;
mod varstore;

use input::setup_ps2;
use reboot::reexec_for_reboot;

/// The registry, the AHCI-CD flag, and the plumbing a device needs to
/// be built. The context outlives startup so hotplug can build one
/// after the VM is running.
type PciDeviceInventory = (
    Arc<DeviceRegistry>,
    bool,
    Arc<vmm_machine::pci::OwnedPciCtx>,
);

/// Result of `create_machine`: the finalized Machine plus metadata needed
/// by later stages.
struct MachineCreation {
    machine: Machine,
    api_version: u32,
    num_cpus: u32,
    mem_size: usize,
    kernel_image: Option<vmm_boot::BootImage>,
    initrd_image: Option<direct::InitrdImage>,
    hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    varstore: Option<Arc<varstore::VarStore>>,
    /// TPM2 table and DSDT device description when --vtpm is enabled.
    tpm_acpi: Option<acpi::TpmAcpi>,
    /// The CPUID table every boot vCPU was given, or `None` for a pure
    /// host-passthrough VM. A CPU brought online later has to get the
    /// same one: two feature sets on one machine is a guest fault that
    /// is very hard to read back.
    cpuid: Option<vmm_core::cpuid::CpuidTable>,
    /// The baseline the CPUID table was built for. A migration
    /// describes the guest's features with it.
    cpu_baseline: vmm_core::cpuid::CpuBaseline,
    host_local_state: host_state::HostLocalState,
}

/// Build the VM, load bootrom/kernel images, finalize memory, and apply
/// CPU topology and baseline masking.
fn create_machine(cli: &Cli, log: &Logger) -> anyhow::Result<MachineCreation> {
    host_state::validate_startup(cli)?;
    // A misspelled hotplug key would otherwise leave the operator with
    // a VM that quietly has no window.
    cli.check_hotplug_options()?;

    let num_cpus = cli.num_cpus()?;
    let mem_size = cli.mem_size()?;

    // Validate the boot images and find the bootrom before the VM
    // exists. None of it touches guest memory, so a bad path now fails
    // without leaving a VM behind.
    let direct_boot_mode = cli.kernel.is_some();

    let kernel_image = if let Some(ref kpath) = cli.kernel {
        info!(log, "direct boot mode"; "kernel" => kpath.display().to_string());
        let img = vmm_boot::BootImage::open(kpath)?;
        info!(log, "kernel validated";
            "protocol" => format!("{:?}", img.protocol()),
            "entry" => format!("{:#x}", img.entry_point()),
        );
        Some(img)
    } else {
        None
    };

    let initrd_image = if let Some(ref ipath) = cli.initrd {
        Some(direct::InitrdImage::open(ipath)?)
    } else {
        None
    };

    let bootrom_spec = if direct_boot_mode {
        None
    } else {
        Some(bootrom::find_bootrom_spec(&cli.lpc)?)
    };
    if bootrom_spec.is_some() {
        bootrom::log_bootrom_diagnostics(&cli.lpc, log);
    }
    if let Some(spec) = &bootrom_spec {
        anyhow::ensure!(
            !(cli.migrate_listen.is_some() && spec.vars.is_some()),
            "--migrate-listen with a bootrom variable store is not supported: the varstore is host-file state that live migration does not carry",
        );
    }

    let version = version_string();
    let opts = MachineOpts {
        vm_name: &cli.vm_name,
        num_cpus,
        mem_size,
        create_opts: CreateOpts {
            force: true,
            use_reservoir: cli.use_reservoir(),
            track_dirty: !cli.no_track_dirty,
        },
        banner: "rshyve starting",
        version: &version,
    };

    // The ROM goes in through the pre-finalize hook because that is the
    // only window with mutable PhysMap access.
    let rom_to_load = bootrom_spec
        .as_ref()
        .filter(|_| cli.migrate_listen.is_none());
    let mut varfile: Option<(varstore::VarFile, u64)> = None;
    let built = build_machine(
        &opts,
        &mut |setup| {
            if let Some(spec) = rom_to_load {
                varfile = load_bootrom(setup, spec, log)?;
            }
            Ok(())
        },
        log,
    )?;
    let machine = built.machine;
    let api_version = built.api_version;

    // Apply CPU feature masking for migration compatibility
    let cpu_baseline: vmm_core::cpuid::CpuBaseline =
        cli.cpu_baseline.parse()?;
    if cpu_baseline != vmm_core::cpuid::CpuBaseline::Host {
        info!(log, "applying CPU baseline"; "baseline" => &cli.cpu_baseline);
    }

    // Both branches keep the table, not just its side effect: a vCPU
    // brought online later replays it.
    let (hyperv, cpuid) = if cli.hyperv {
        let tsc_freq_hz = cli.tsc_freq_hz.unwrap_or_else(|| {
            host_tsc_frequency_hz().unwrap_or_else(|| {
                warn!(log, "host TSC freq probe failed, reference-TSC page will be invalid; \
                            pass --tsc-freq-hz to override");
                0
            })
        });
        let features = vmm_hyperv::Features {
            reference_tsc: tsc_freq_hz != 0,
            reset: true,
            frequencies: true,
            tsc_freq_hz,
        };
        info!(log, "enabling Hyper-V enlightenments";
            "tsc_freq_hz" => tsc_freq_hz,
            "reference_tsc" => features.reference_tsc,
        );
        let hv = vmm_hyperv::HyperV::new(
            log.new(o!("module" => "hyperv")),
            machine.physmap().clone(),
            features,
        );
        let mut extra = Vec::new();
        hv.add_cpuid(&mut extra);
        let table = vmm_core::cpuid::apply_cpuid_table(
            machine.hdl(),
            num_cpus,
            cpu_baseline,
            &extra,
            log,
        )
        .context("failed to apply CPUID with Hyper-V leaves")?;
        (Some(hv), table)
    } else {
        let table = vmm_core::cpuid::apply_cpuid_table(
            machine.hdl(),
            num_cpus,
            cpu_baseline,
            &[],
            log,
        )
        .context("failed to apply CPU baseline")?;
        (None, table)
    };

    let tpm_acpi = if cli.vtpm {
        let state_dir = cli.vtpm_state_dir.clone().unwrap_or_else(|| {
            PathBuf::from("/var/db/rshyve")
                .join(&cli.vm_name)
                .join("tpm")
        });
        let tpm = vmm_tpm::Tpm::new(
            state_dir.clone(),
            log.new(o!("module" => "vtpm")),
        )
        .with_context(|| {
            format!(
                "failed to initialize vTPM (state_dir={})",
                state_dir.display(),
            )
        })?;

        // Register the CRB MMIO region. The handler clones an Arc to
        // the Crb device so the vCPU dispatch can fire reads/writes
        // without any locks held in the bus path.
        let crb = tpm.crb.clone();
        machine
            .bus_mmio()
            .register(
                vmm_tpm::CRB_BASE,
                vmm_tpm::CRB_REGION_LEN,
                Arc::new(move |offset, op| crb.handle(offset, op)),
            )
            .context("registering vTPM CRB MMIO region")?;
        info!(log, "vTPM CRB MMIO region registered";
            "base" => format!("{:#x}", vmm_tpm::CRB_BASE),
            "len" => format!("{:#x}", vmm_tpm::CRB_REGION_LEN),
        );
        if let Some(spec) = &bootrom_spec {
            if bootrom::should_warn_missing_tcg2(&spec.rom) {
                warn!(log, "vTPM requires TCG2-capable firmware; {} leaves the guest TPM uninitialized", spec.rom.display();
                    "recommended_firmware" => "BHYVE_UEFI_CODE.fd",
                );
            }
        }

        Some(acpi::TpmAcpi {
            table: vmm_tpm::build_tpm2_table(),
            device: acpi::TpmDevice {
                crb_base: u32::try_from(vmm_tpm::CRB_BASE)
                    .expect("CRB_BASE below 4 GiB"),
                crb_len: u32::try_from(vmm_tpm::CRB_REGION_LEN)
                    .expect("CRB_REGION_LEN fits u32"),
            },
        })
    } else {
        None
    };

    let varstore = varfile
        .map(|(vf, gpa)| -> anyhow::Result<Arc<varstore::VarStore>> {
            let vs = Arc::new(varstore::VarStore::new(
                vf,
                gpa,
                log.new(o!("module" => "varstore")),
            ));
            let gpa = vs.gpa();
            let len = vs.len();
            let handler = Arc::clone(&vs);
            machine
                .bus_mmio()
                .register(
                    gpa,
                    len as u64,
                    Arc::new(move |offset, op| handler.handle(offset, op)),
                )
                .context("registering UEFI variable store MMIO region")?;
            info!(log, "UEFI variable store registered";
                "base" => format!("{gpa:#x}"),
                "len" => format!("{len:#x}"));
            Ok(vs)
        })
        .transpose()?;
    let host_local_state =
        host_state::HostLocalState::new(tpm_acpi.is_some(), varstore.is_some());

    Ok(MachineCreation {
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
    })
}

/// Load the bootrom image into guest memory (pre-finalize).
fn load_bootrom(
    setup: &mut MachineSetup,
    spec: &bootrom::BootromSpec,
    log: &Logger,
) -> anyhow::Result<Option<(varstore::VarFile, u64)>> {
    info!(log, "loading bootrom"; "path" => spec.rom.display().to_string());
    let rom = bootrom::BootromFile::open(&spec.rom)?;
    let varfile = spec
        .vars
        .as_deref()
        .map(varstore::VarFile::open)
        .transpose()?;
    let var_size = varfile.as_ref().map(|file| file.len()).unwrap_or(0);
    let layout = bootrom::compute_layout(rom.size(), var_size)?;
    bootrom::log_flash_layout(rom.path(), spec.vars.as_deref(), &layout, log);
    let hdl = setup.hdl().clone();
    bootrom::load_bootrom(setup.map_mut(), &hdl, &rom, &layout)?;
    varfile
        .map(|file| {
            let gpa = layout.var_gpa.context(
                "variable store layout is missing its guest address",
            )?;
            Ok((file, gpa))
        })
        .transpose()
}

/// Create and attach COM1/COM2 UARTs, set up serial I/O paths, and
/// optionally start the mdata agent on COM2.
fn setup_serial(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    cli: &Cli,
    log: &Logger,
) -> anyhow::Result<(Arc<LpcUart>, Arc<LpcUart>)> {
    let com1 =
        vmm_machine::attach_uart(machine, pic, lpc::COM1_BASE, lpc::COM1_IRQ);
    info!(log, "COM1 attached"; "port" => format!("{:#x}", lpc::COM1_BASE));
    let com2 =
        vmm_machine::attach_uart(machine, pic, lpc::COM2_BASE, lpc::COM2_IRQ);
    info!(log, "COM2 attached"; "port" => format!("{:#x}", lpc::COM2_BASE));

    // Parse COM port paths from -l flags
    let com1_path = find_lpc_device(&cli.lpc, "com1");
    let com2_path = find_lpc_device(&cli.lpc, "com2");

    // Set up COM1 serial I/O based on -l com1,<config>
    setup_serial_io(&com1, com1_path.as_deref(), "com1", log)?;

    // COM2 serves the backend the argv named, or the built-in mdata
    // agent when it named none.
    if let Some(ref path) = com2_path {
        setup_serial_io(&com2, Some(path.as_str()), "com2", log)?;
    } else {
        // Standalone mode: start built-in mdata agent using CLI flags.
        let mdata_store = Arc::new(mdata::build_store_from_zone_config(
            &cli.vm_name,
            cli.uuid.as_deref(),
            cli.mdata_nics.as_deref(),
            cli.mdata_resolvers.as_deref(),
            cli.mdata_ssh_keys.as_deref(),
            cli.mdata_root_pw.as_deref(),
        ));
        mdata::start_mdata_agent(com2.clone(), mdata_store, log.clone());
        info!(log, "mdata agent started on COM2");
    }

    Ok((com1, com2))
}

/// Parse all `-s` PCI slot specifications and create the corresponding
/// devices. Returns the run-time registry, which owns every handle and
/// which the run, control and teardown paths all read. The AHCI flag
/// lets the control path reject migration without re-parsing the
/// original specifications.
fn init_pci_devices(
    machine: &Machine,
    chipset: &Arc<I440FxChipset>,
    cli: &Cli,
    num_vcpus: u32,
    input: &Arc<InputBroker>,
    log: &Logger,
) -> anyhow::Result<PciDeviceInventory> {
    let is_migrate = cli.migrate_listen.is_some();

    reject_unmigratable_destination(is_migrate, &cli.pci_slot)?;

    let mut specs: Vec<String> = Vec::with_capacity(cli.pci_slot.len());
    for slot_spec in &cli.pci_slot {
        let (device_spec, _) = bootorder::strip_bootindex(slot_spec)?;
        specs.push(device_spec);
    }
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
/// equal to argv. The peercred check in `control::listener` is what
/// grants it: the peer must share this process's uid and be in this
/// zone or the global zone.
fn hotplug_factory(cli: &Cli, num_vcpus: u32) -> vmm_machine::HotplugFactory {
    let vm_name = cli.vm_name.clone();
    Box::new(move |spec, ctx| {
        // Never a migration destination: a hot-add happens on a running
        // VM, and the guest programs the new rings itself.
        parse_and_create_pci_device(spec, num_vcpus, false, &vm_name, ctx)
    })
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

fn reject_unmigratable_destination(
    is_migrate_dest: bool,
    specs: &[String],
) -> anyhow::Result<()> {
    if is_migrate_dest {
        if let Some(driver) = specs.iter().find_map(|spec| {
            let driver = pci_spec_driver(spec)?;
            matches!(driver, "fbuf" | "ahci-cd").then_some(driver)
        }) {
            anyhow::bail!(
                "{} is not supported on a migration destination",
                driver
            );
        }
    }
    Ok(())
}

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

/// Handle migration destination mode: activate vCPUs, listen for a
/// migration connection, and import VM state from the source.
fn run_migration_destination(
    machine: &Machine,
    listen_addr: &str,
    num_cpus: u32,
    mem_size: usize,
    devices: &[Arc<dyn vmm_devices::Lifecycle>],
    log: &Logger,
) -> anyhow::Result<()> {
    info!(log, "migration destination mode"; "listen" => listen_addr);

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

    // Listen for migration connection
    let hdl = machine.hdl().clone();
    let physmap = machine.physmap().clone();
    let migrate_mem_size = mem_size as u64;
    let status = Arc::new(std::sync::Mutex::new(
        vmm_migrate::MigrationStatus::default(),
    ));

    let is_unix =
        listen_addr.starts_with('/') || listen_addr.ends_with(".sock");

    if is_unix {
        // Synchronous Unix socket -- no tokio needed (works in bhyve zones).
        info!(log, "connecting to migration socket"; "path" => listen_addr);
        let stream = {
            let mut retries = 0;
            loop {
                match std::os::unix::net::UnixStream::connect(listen_addr) {
                    Ok(s) => break s,
                    Err(_) if retries < 30 => {
                        retries += 1;
                        thread::sleep(std::time::Duration::from_millis(500));
                    }
                    Err(e) => anyhow::bail!("connect to {listen_addr}: {e}"),
                }
            }
        };
        info!(log, "connected to migration socket");
        // Build restore callback that reprograms device queue state
        // on destination devices (virtio rings + NVMe controller).
        let devs: Vec<Arc<dyn vmm_devices::Lifecycle>> = devices.to_vec();
        let restore_fn: vmm_migrate::sync_destination::DeviceRestoreFn =
            Box::new(move |queue_states, nvme_states| {
                // Every device sees every entry and takes its own.
                let queues = vmm_devices::DeviceMigrateState::VirtioQueues(
                    queue_states.to_vec(),
                );
                for dev in &devs {
                    dev.restore_migrate_state(&queues);
                }
                for nvme in nvme_states {
                    let state =
                        vmm_devices::DeviceMigrateState::Nvme(nvme.clone());
                    for dev in &devs {
                        dev.restore_migrate_state(&state);
                    }
                }
            });

        vmm_migrate::sync_destination::run_destination_sync(
            stream,
            hdl,
            &vmm_core::mem::MemCtx::new(physmap),
            num_cpus,
            migrate_mem_size,
            status,
            log,
            Some(restore_fn),
        )
        .context("migration import failed")?;
    } else {
        // Async TCP path -- for GZ or manual testing.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create tokio runtime")?;
        let addr = listen_addr.to_string();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .context("bind failed")?;
            info!(log, "waiting for migration"; "addr" => &addr);
            let (stream, peer) =
                listener.accept().await.context("accept failed")?;
            info!(log, "migration connection"; "peer" => %peer);
            vmm_migrate::destination::run_destination(
                vmm_migrate::MigrateStream::Tcp(stream),
                hdl.clone(),
                &vmm_core::mem::MemCtx::new(physmap.clone()),
                num_cpus,
                migrate_mem_size,
                status,
                log,
            )
            .await
            .context("migration failed")
        })?;
    }

    info!(log, "migration import complete, starting vCPUs");

    // Run state was restored per-vCPU during import_device_state.
    // No need to force VRS_RUN here — vCPUs in HLT/INIT stay
    // in that state and will be resumed by the kernel when needed.
    // Interrupt injection happens AFTER vCPU threads are spawned
    // (see below). Injecting before spawn loses the kick.
    Ok(())
}

/// After migration, inject a timer interrupt to restart the scheduler tick
/// in the guest (nohz/tickless mode workaround).
fn post_migration_timer_kick(machine: &Machine, log: &Logger) {
    thread::sleep(std::time::Duration::from_millis(200));
    for vcpu in machine.vcpus() {
        match machine.hdl().lapic_irq(vcpu.id(), 0xEC) {
            Ok(()) => {
                info!(log, "post-migration timer kick"; "vcpu" => vcpu.id())
            }
            Err(e) => {
                warn!(log, "timer kick failed"; "vcpu" => vcpu.id(), "error" => %e)
            }
        }
    }
}

/// What the control socket leaves behind for the rest of `run`.
#[derive(Default)]
struct ControlSocket {
    /// The socket file to remove at teardown.
    cleanup: Option<PathBuf>,
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
    has_ahci_cd: bool,
    hotplug: HotplugEngines,
    host_local_state: host_state::HostLocalState,
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
        has_ahci_cd,
        hotplug,
        cli.pci_slot.clone(),
        cli.lpc.clone(),
        cpu_baseline,
        host_local_state,
        log.clone(),
    );
    let stop = control::sigterm_stop(Arc::clone(&ctrl));
    let (_handle, path) =
        control::spawn_control_thread(sock_path, ctrl, log.clone())?;
    info!(log, "control socket listening"; "path" => path.display().to_string());
    Ok(ControlSocket {
        cleanup: Some(path),
        stop: Some(stop),
    })
}

/// What `run` hands `main`.
///
/// The hot-added specs travel with the outcome because only `run` can
/// see the registry, and only `main` has argv to rebuild.
struct RunResult {
    outcome: RunOutcome,
    /// `-s` specs for the devices an operator added while the VM ran.
    hotplug_specs: Vec<String>,
}

fn run(cli: Cli, log: &Logger) -> anyhow::Result<RunResult> {
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
    let (_com1, _com2) = setup_serial(&machine, &chipset_devs.pic, &cli, log)?;
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
    let (registry, has_ahci_cd, pci_ctx) = init_pci_devices(
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
            &registry.lifecycle_devices(),
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
    // bhyve does it at the same point, bhyverun.c:915. It must also
    // stay ahead of any privilege drop, since the rename needs file
    // access.
    vmm_machine::provision::mark_provisioned(log);

    // Phase 9: Spawn vCPU threads. The fleet, not the fixed set,
    // because a CPU added later joins the same event channel and the
    // same roster.
    let fleet = vmm_machine::spawn_vcpu_fleet(
        &machine,
        num_cpus,
        api_version,
        hyperv.clone().map(|hv| hv as Arc<dyn MsrHandler>),
        log,
    );
    let vcpu_metrics = fleet.metrics.clone();

    // Phase 10: Post-migration timer kick + delayed virtio interrupt
    if cli.migrate_listen.is_some() {
        post_migration_timer_kick(&machine, log);

        // Re-inject virtio interrupts periodically after vCPUs start.
        // The initial post-restore interrupt fires before resume but
        // may not be sufficient for virtio-blk (the guest's blk driver
        // may need multiple interrupt cycles to fully process stale
        // used ring entries and resume I/O).
        let devs = registry.lifecycle_devices();
        thread::Builder::new()
            .name("post-migrate-virtio-kick".into())
            .spawn(move || {
                for _ in 0..10 {
                    thread::sleep(std::time::Duration::from_millis(100));
                    for dev in &devs {
                        dev.post_restore_kick();
                    }
                }
            })
            .ok();
    }

    // Phase 11: Hotplug engines. Each owns the thread that answers the
    // guest, so no teardown ever runs on a vCPU thread.
    let pci_hotplug = timer_pm_devs.hotplug.as_ref().map(|regs| {
        HotplugEngine::start(
            Arc::clone(&registry),
            pci_ctx,
            Arc::clone(&regs.pci),
            hotplug_factory(&cli, num_cpus),
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
        has_ahci_cd,
        hotplug.clone(),
        host_local_state,
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
        control_socket.cleanup.clone(),
        &registry,
        log,
    );

    // The control thread is never joined, so its handles would
    // otherwise keep the drain threads alive past the event loop.
    hotplug.shutdown();

    // Read after the engine stops, so a teardown that was in flight has
    // finished and cannot leave a departed device in the list.
    let hotplug_specs = vmm_machine::hotplug_specs(&registry);
    outcome.map(|outcome| RunResult {
        outcome,
        hotplug_specs,
    })
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
        "nvme" => {
            if config.is_empty() {
                anyhow::bail!("nvme requires a disk path");
            }
            let (path, blk_opts) = devspec::parse_blk_config(config);
            use std::fs::OpenOptions;
            let file = OpenOptions::new()
                .read(true)
                .write(!blk_opts.read_only)
                .open(path)
                .with_context(|| format!("failed to open disk: {}", path))?;

            let nvme = vmm_storage::nvme::NvmeController::new(
                file,
                blk_opts.read_only,
                bdf.dev(), // unique instance ID per slot
                Arc::clone(physmap),
                Arc::clone(vmm_hdl),
                Arc::clone(bus_mmio),
            );

            chipset.try_attach_device(bdf, nvme.clone())?;
            info!(log, "nvme attached";
                "bdf" => %bdf,
                "path" => path,
            );
            Ok(vmm_machine::created(nvme))
        }
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

            // Parse config: vga=off,unix=/path[,password=pwd][,w=N][,h=N]
            let mut vnc_path = PathBuf::from("/var/run/rshyve")
                .join(vm_name)
                .join("vnc.sock");
            let mut vnc_password = None;
            let mut width: u16 = 1024;
            let mut height: u16 = 768;
            for opt in config.split(',') {
                if let Some(p) = opt.strip_prefix("unix=") {
                    vnc_path = PathBuf::from(p);
                } else if let Some(p) = opt.strip_prefix("password=") {
                    vnc_password = Some(p.to_string());
                } else if let Some(w) = opt.strip_prefix("w=") {
                    width = w.parse().unwrap_or(1024);
                } else if let Some(h) = opt.strip_prefix("h=") {
                    height = h.parse().unwrap_or(768);
                }
                // vga=off is the default, ignore
            }

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
            let dev = vmm_passthru::PciPassthru::new(path, Arc::clone(vmm_hdl))
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

/// Set up serial I/O for a COM port.
///
/// Supports:
/// - `stdio` or absent → stdout/stdin
/// - `socket,/path` → Unix domain socket (host metadata agent connects)
/// - `/dev/zconsole` or any path → open as read/write device
///
/// The last two forms of backend live in `vmm_devices::uart::backend`,
/// so this binary and firehyve serve a zone console the same way.
fn setup_serial_io(
    uart: &Arc<LpcUart>,
    config: Option<&str>,
    name: &str,
    log: &Logger,
) -> anyhow::Result<()> {
    // Handle "socket,/path" — create a Unix domain socket listener
    // that the host-side metadata agent connects to.
    if let Some(path) = config.and_then(|c| c.strip_prefix("socket,")) {
        setup_serial_socket(uart, path, name, log)?;
        return Ok(());
    }

    let backend = match config {
        None | Some("stdio") => SerialBackend::Stdio,
        Some(path) => SerialBackend::Device(path),
    };
    // A console that cannot be opened is not worth failing a boot for:
    // the guest still runs, and the warning names the path.
    if let Err(e) = vmm_devices::uart::backend::attach(uart, backend, name, log)
    {
        warn!(log, "serial backend unusable"; "port" => name,
            "error" => %e);
    }
    Ok(())
}

/// Set up a Unix domain socket backend for a COM port.
///
/// Creates a listening socket at `path`, accepts one connection at a
/// time, and bridges it to the UART. The host-side metadata agent
/// connects to this socket to serve the SmartOS metadata protocol
/// (sdc:nics, SSH keys, etc.) to the guest via COM2.
fn setup_serial_socket(
    uart: &Arc<LpcUart>,
    path: &str,
    name: &str,
    log: &Logger,
) -> std::io::Result<()> {
    use std::io::{Read, Write};

    let listener = vmm_core::unixsock::bind_restricted(
        std::path::Path::new(path),
        vmm_core::unixsock::SocketPolicy::default(),
    )?;
    info!(log, "{name} socket listening"; "path" => path);

    // TX: buffer writes until a client connects. Use a shared
    // optional stream protected by a mutex.
    let tx_stream: Arc<
        std::sync::Mutex<Option<std::os::unix::net::UnixStream>>,
    > = Arc::new(std::sync::Mutex::new(None));
    let tx_clone = Arc::clone(&tx_stream);
    uart.set_tx_sink(Box::new(move |b: u8| {
        if let Ok(guard) = tx_clone.lock() {
            if let Some(ref stream) = *guard {
                // best-effort write; drop bytes if client disconnected
                let _ = (&*stream).write_all(&[b]);
            }
        }
    }));

    // Accept loop: each connection gets bridged to the UART.
    // When a client disconnects, wait for the next one.
    let uart_rx = uart.clone();
    let tx_accept = Arc::clone(&tx_stream);
    let name_owned = name.to_string();
    let log_clone = log.clone();
    thread::Builder::new()
        .name(format!("{name}-socket"))
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        info!(log_clone, "{name_owned} client connected");
                        // Install this stream for TX
                        if let Ok(mut guard) = tx_accept.lock() {
                            *guard = stream.try_clone().ok();
                        }
                        // RX: read from client → UART input
                        // Back-pressure: if the RX FIFO is full, wait
                        // for the guest to drain it before pushing more
                        // bytes.  c-bhyve does this via mevent_disable/
                        // enable; we use a simple retry loop instead.
                        let mut reader = stream;
                        let mut buf = [0u8; 64];
                        loop {
                            match reader.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    for &b in &buf[..n] {
                                        while !uart_rx.input_byte(b) {
                                            thread::sleep(
                                                std::time::Duration::from_micros(100),
                                            );
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        info!(log_clone, "{name_owned} client disconnected");
                        // Clear TX stream so bytes are dropped until next client
                        if let Ok(mut guard) = tx_accept.lock() {
                            *guard = None;
                        }
                    }
                    Err(e) => {
                        slog::warn!(log_clone, "{name_owned} accept error";
                            "error" => %e);
                    }
                }
            }
        })
        .expect("failed to spawn serial socket thread");

    Ok(())
}

/// Version string including git commit hash (set by build.rs).
fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("VMM_GIT_COMMIT");
    format!("{version} ({commit})")
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let exe =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from(&args[0]));

    if vmm_machine::entry::version_requested(&args) {
        println!("rshyve {}", version_string());
        return ExitCode::SUCCESS;
    }
    vmm_machine::entry::register_probes("rshyve");

    let cli = Cli::parse_named("rshyve");
    let log = vmm_machine::entry::setup_logger("rshyve");

    match run(cli, &log) {
        Ok(RunResult {
            outcome: RunOutcome::Reboot,
            hotplug_specs,
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

    #[test]
    fn one_cpu_ceiling_feeds_the_tables_the_registers_and_the_engine() {
        // The MADT slot count, the CPU register file and the hot-add
        // engine have to describe one machine. All three read
        // VmOpts::max_cpus: the tables through setup_fwcfg_and_acpi, the
        // register file through HotplugOpts::from_vm, and the engine
        // through the VcpuSetup in run(). A hand-built HotplugOpts here
        // would let the register file describe a different machine, so
        // the phase sequence must not contain one.
        let src = include_str!("main.rs");
        let end = src.find("#[cfg(test)]").expect("test module attribute");
        let code = &src[..end];

        assert!(!code.contains("HotplugOpts {"), "a hand-built HotplugOpts");
        // One read, so every other use of the ceiling in this file
        // flows from the one local it is read into.
        assert_eq!(code.matches("VmOpts::from_cli(").count(), 1);
        assert_eq!(code.matches("cli.max_cpus()").count(), 0);
    }

    #[test]
    fn ahci_hd_is_rejected() {
        let error = reject_ahci_hd::<()>().unwrap_err();
        let message = error.to_string();

        assert!(message.contains("not implemented"));
        assert!(!message.contains("use virtio-blk instead"));
    }

    #[test]
    fn specs_have_ahci_cd_reads_the_driver_field() {
        let specs = vec![
            "4,virtio-blk,/dev/dsk/test".to_string(),
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
    fn reject_unmigratable_destination_still_rejects_fbuf() {
        let fbuf = vec!["30,fbuf,unix=/tmp/vm.vnc".to_string()];

        let err = reject_unmigratable_destination(true, &fbuf).unwrap_err();
        assert_eq!(
            err.to_string(),
            "fbuf is not supported on a migration destination"
        );
        assert!(reject_unmigratable_destination(false, &fbuf).is_ok());
    }

    #[test]
    fn reject_unmigratable_destination_allows_normal_specs() {
        let specs = vec![
            "4,virtio-blk,/dev/dsk/test".to_string(),
            "5,nvme,/tmp/fbuf".to_string(),
        ];

        assert!(reject_unmigratable_destination(true, &specs).is_ok());
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
