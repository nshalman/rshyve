// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vmm - New Rust-based bhyve VMM with live migration support.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::Context;
use clap::Parser;
use slog::{error, info, o, warn, Drain, Logger};

use vmm_config::Cli;
use vmm_core::exits::Suspend;
use vmm_core::hdl::CreateOpts;
use vmm_core::intr_pins::{LegacyPIC, NoOpPin};
use vmm_core::machine::{Builder, Machine, MachineSetup};
use vmm_devices::acpi;
use vmm_devices::acpi_pm::SuspendSink;
use vmm_devices::bhyve::{
    pmtimer::{BhyvePmTimer, PMBASE_DEFAULT},
    BhyveAtPic, BhyveAtPit, BhyveHpet, BhyveIoApic, BhyveRtc,
};
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::fwcfg::FwCfg;
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::pci::Bdf;
use vmm_devices::smbios;
use vmm_devices::uart::lpc::{self, LpcUart};
use vmm_devices::virtio::block::{VirtioBlock, VirtioBlockOpts};
use vmm_devices::virtio::pci::VirtioPciDevice;
use vmm_devices::virtio::viona::VirtioViona;
use vmm_devices::InputBroker;

mod bootorder;
mod bootrom;
mod control;
mod direct_boot;
mod host_state;
mod input;
mod mdata;
mod peercred;
mod reboot;
mod varstore;
mod vcpu_tasks;

use input::setup_ps2;
use reboot::{
    join_vcpus_bounded, quiesce_devices, reexec_for_reboot, RunOutcome,
};

type PciDeviceHandle = Arc<dyn vmm_devices::pci::PciDevice>;
type LifecycleHandle = Arc<dyn vmm_devices::Lifecycle>;
type PciDeviceInventory = (Vec<PciDeviceHandle>, Vec<LifecycleHandle>, bool);
type CreatedPciDevice = (Option<PciDeviceHandle>, Option<LifecycleHandle>);

/// Atomic flag set by the SIGTERM signal handler.
static SIGTERM_RECEIVED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Signal handler for SIGTERM — just sets the atomic flag.
/// The actual shutdown logic runs in a separate thread that polls this flag.
extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGTERM_RECEIVED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn setup_logger() -> Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!("component" => "vmm"))
}

/// Spawn a thread that drains the UART TX buffer and writes to stderr.
/// This is the simplest possible serial console — just print what the
/// guest writes to COM1.
// Retained as a simple diagnostic console path for backend bring-up.
#[allow(dead_code)]
fn spawn_serial_drain(
    uart: Arc<LpcUart>,
    log: Logger,
    output_path: Option<&std::path::Path>,
) -> JoinHandle<()> {
    // Open serial output file if specified, otherwise use stderr
    // (stderr avoids interleaving with interactive console input on stdout)
    let output: Box<dyn Write + Send> = if let Some(path) = output_path {
        match std::fs::File::create(path) {
            Ok(f) => Box::new(f),
            Err(e) => {
                warn!(log, "failed to open serial output file, using stderr";
                    "path" => path.display().to_string(), "error" => %e);
                Box::new(std::io::stderr())
            }
        }
    } else {
        Box::new(std::io::stderr())
    };
    let output = std::sync::Mutex::new(output);

    thread::Builder::new()
        .name("serial-drain".into())
        .spawn(move || {
            loop {
                match uart.output_byte() {
                    Some(b) => {
                        if let Ok(mut out) = output.lock() {
                            if let Err(e) = out.write_all(&[b]) {
                                eprintln!("[serial-drain] write error: {}", e);
                            }
                            let _ = out.flush();
                        }
                    }
                    None => {
                        // Poll more aggressively to keep up with high-speed serial
                        thread::sleep(std::time::Duration::from_micros(100));
                    }
                }
            }
        })
        .expect("failed to spawn serial drain thread")
}

/// Result of `create_machine`: the finalized Machine plus metadata needed
/// by later stages.
struct MachineCreation {
    machine: Machine,
    api_version: u32,
    num_cpus: u32,
    mem_size: usize,
    kernel_image: Option<direct_boot::KernelImage>,
    initrd_image: Option<direct_boot::InitrdImage>,
    hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    varstore: Option<Arc<varstore::VarStore>>,
    /// TPM2 table and DSDT device description when --vtpm is enabled.
    tpm_acpi: Option<acpi::TpmAcpi>,
    host_local_state: host_state::HostLocalState,
}

/// Build the VM, load bootrom/kernel images, finalize memory, and apply
/// CPU topology and baseline masking.
fn create_machine(cli: &Cli, log: &Logger) -> anyhow::Result<MachineCreation> {
    host_state::validate_startup(cli)?;

    let num_cpus = cli.num_cpus()?;
    let mem_size = cli.mem_size()?;

    info!(log, "vmm starting";
        "version" => version_string(),
    );
    info!(log, "creating VM";
        "name" => &cli.vm_name,
        "cpus" => num_cpus,
        "memory_mb" => mem_size / (1024 * 1024),
        "track_dirty" => cli.track_dirty && !cli.no_track_dirty,
    );

    let track_dirty = cli.track_dirty && !cli.no_track_dirty;
    let opts = CreateOpts {
        force: true,
        use_reservoir: cli.wire_memory,
        track_dirty,
    };

    // Phase 1: Build VM with RAM
    let mut setup = Builder::new(&cli.vm_name)
        .opts(opts)
        .cpus(num_cpus)
        .memory(mem_size)
        .build()
        .context("failed to create VM")?;

    // Phase 2: Load kernel or bootrom
    let direct_boot_mode = cli.kernel.is_some();

    // Parse direct boot images before finalize (need mutable map access).
    // In direct boot mode, kernel loading happens after device init so
    // ACPI tables are ready. For bootrom mode, load ROM now.
    let kernel_image = if let Some(ref kpath) = cli.kernel {
        info!(log, "direct boot mode"; "kernel" => kpath.display().to_string());
        let ki = direct_boot::KernelImage::open(kpath)?;
        info!(log, "kernel validated";
            "entry" => format!("{:#x}", ki.entry_point_64()),
        );
        Some(ki)
    } else {
        None
    };

    let initrd_image = if let Some(ref ipath) = cli.initrd {
        Some(direct_boot::InitrdImage::open(ipath)?)
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
    let varfile = match (&bootrom_spec, cli.migrate_listen.is_none()) {
        (Some(spec), true) => load_bootrom(&mut setup, spec, log)?,
        _ => None,
    };

    // Finalize: wrap PhysMap in Arc, create vCPUs and buses
    let machine = setup.finalize();

    let api_version = match machine.hdl().api_version() {
        Ok(ver) => {
            info!(log, "VM ready";
                "api_version" => ver,
                "total_memory" => machine.total_mapped_memory(),
                "vcpus" => machine.num_cpus(),
            );
            ver
        }
        Err(e) => {
            anyhow::bail!("failed to query bhyve API version: {}", e);
        }
    };

    // Set CPU topology (required for kernel LAPIC/timer setup)
    // maxcpus must be 0 (kernel ignores it and uses VM_MAXCPU)
    machine
        .hdl()
        .set_topology(1, num_cpus as u16, 1, 0)
        .context("failed to set CPU topology")?;

    // Apply CPU feature masking for migration compatibility
    let cpu_baseline = vmm_core::cpuid::CpuBaseline::parse(&cli.cpu_baseline)
        .ok_or_else(|| {
        anyhow::anyhow!(
            "unknown CPU baseline '{}' (valid: host, avx2, no-avx512, sse42)",
            cli.cpu_baseline,
        )
    })?;
    if cpu_baseline != vmm_core::cpuid::CpuBaseline::Host {
        info!(log, "applying CPU baseline"; "baseline" => &cli.cpu_baseline);
    }

    let hyperv = if cli.hyperv {
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
        vmm_core::cpuid::apply_cpuid(
            machine.hdl(),
            num_cpus,
            cpu_baseline,
            &extra,
            log,
        )
        .context("failed to apply CPUID with Hyper-V leaves")?;
        Some(hv)
    } else {
        vmm_core::cpuid::apply_cpuid_baseline(
            machine.hdl(),
            num_cpus,
            cpu_baseline,
            log,
        )
        .context("failed to apply CPU baseline")?;
        None
    };

    let tpm_acpi = if cli.vtpm {
        let state_dir = cli.vtpm_state_dir.clone().unwrap_or_else(|| {
            PathBuf::from("/var/db/vmm").join(&cli.vm_name).join("tpm")
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
        host_local_state,
    })
}

/// Probe the host TSC frequency in Hz from CPUID leaf 0x16, falling
/// back to leaf 0x15 (TSC/core ratio).
///
/// Returns `None` on non-x86_64 builds or when the host does not
/// expose the frequency CPUID leaves.
fn host_tsc_frequency_hz() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        let leaf16 = std::arch::x86_64::__cpuid(0);
        if leaf16.eax >= 0x16 {
            let r = std::arch::x86_64::__cpuid(0x16);
            // EAX = base frequency in MHz.
            if r.eax != 0 {
                return Some(r.eax as u64 * 1_000_000);
            }
        }
        if leaf16.eax >= 0x15 {
            // Leaf 0x15: EBX/EAX = TSC/core ratio, ECX = nominal core
            // crystal Hz. Skip if any field is zero.
            let r = std::arch::x86_64::__cpuid(0x15);
            if r.eax != 0 && r.ebx != 0 && r.ecx != 0 {
                return Some(r.ecx as u64 * r.ebx as u64 / r.eax as u64);
            }
        }
        None
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
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

/// Chipset and kernel-emulated device handles that must be kept alive
/// for the VM's lifetime.
#[allow(dead_code)]
struct ChipsetDevices {
    chipset: Arc<I440FxChipset>,
    pic: Arc<LegacyPIC>,
    atpic: Arc<BhyveAtPic>,
    atpit: Arc<BhyveAtPit>,
    hpet: Arc<BhyveHpet>,
    ioapic: Arc<BhyveIoApic>,
    rtc: Arc<BhyveRtc>,
}

/// Create the i440fx chipset, legacy PIC, and all bhyve kernel-emulated
/// devices (ATPIC, ATPIT, HPET, IOAPIC, RTC). Writes memory size to
/// RTC NVRAM and sets RTC time.
fn init_chipset(
    machine: &Machine,
    log: &Logger,
) -> anyhow::Result<ChipsetDevices> {
    // Create legacy PIC for ISA interrupt delivery
    let pic = LegacyPIC::new(
        machine.hdl().clone(),
        log.new(slog::o!("component" => "legacy-pic")),
    );

    // Create i440fx chipset (host bridge + ISA bridge + PCI config ports)
    let chipset = I440FxChipset::create(
        machine.bus_pio(),
        Some(machine.hdl().clone()),
        log.new(slog::o!("component" => "chipset")),
    );
    info!(log, "chipset initialized"; "type" => "i440fx");

    // Create bhyve kernel-emulated devices.
    // These are emulated in-kernel; we just hold references for lifecycle.
    let atpic = BhyveAtPic::create(machine.hdl().clone());
    let atpit = BhyveAtPit::create(machine.hdl().clone());
    let hpet = BhyveHpet::create(machine.hdl().clone());
    let ioapic = BhyveIoApic::create(machine.hdl().clone());
    let rtc = BhyveRtc::create(machine.hdl().clone());

    // Write memory size to RTC NVRAM for UEFI discovery
    let lowmem = machine.mem_size().min(0xC000_0000) as u32;
    let highmem = if machine.mem_size() > 0xC000_0000 {
        (machine.mem_size() - 0xC000_0000) as u64
    } else {
        0
    };
    rtc.memsize_to_nvram(lowmem, highmem)
        .context("failed to write memory size to RTC NVRAM")?;

    // Set RTC time to current wall clock
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    machine
        .hdl()
        .rtc_settime(now)
        .context("failed to set RTC time")?;
    info!(log, "RTC NVRAM configured";
        "lowmem_mb" => lowmem / (1024 * 1024),
        "highmem_mb" => highmem / (1024 * 1024),
    );

    Ok(ChipsetDevices {
        chipset,
        pic,
        atpic,
        atpit,
        hpet,
        ioapic,
        rtc,
    })
}

/// Timer and power-management device handles kept alive for the VM's
/// lifetime.
#[allow(dead_code)]
struct TimerAndPmDevices {
    pmtimer: Arc<BhyvePmTimer>,
    acpi_pm: Arc<vmm_devices::acpi_pm::AcpiPm>,
    pvpanic: Arc<vmm_devices::pvpanic::PvPanic>,
    suspend_sink: Arc<dyn SuspendSink>,
}

/// Configure the ACPI PM timer, ISA IRQ trigger modes for SCI and PCI,
/// ACPI PM1 registers, and the pvpanic PIO device.
fn setup_timers_and_pm(
    machine: &Machine,
    log: &Logger,
) -> anyhow::Result<TimerAndPmDevices> {
    // Configure ACPI PM timer in kernel
    let pmtimer = BhyvePmTimer::create(machine.hdl().clone(), PMBASE_DEFAULT);
    pmtimer.attach().context("failed to configure PM timer")?;

    // Set SCI IRQ (9) to level-triggered mode in the kernel ATPIC.
    machine
        .hdl()
        .isa_set_irq_trigger(9, true)
        .context("failed to set SCI IRQ trigger mode")?;

    // Set PCI IRQs 10 and 11 to level-triggered (firmware assigns these
    // via PIR registers to PIRQ links A-D).
    machine
        .hdl()
        .isa_set_irq_trigger(10, true)
        .context("failed to set IRQ 10 trigger mode")?;
    machine
        .hdl()
        .isa_set_irq_trigger(11, true)
        .context("failed to set IRQ 11 trigger mode")?;
    info!(log, "PM timer attached"; "port" => format!("{:#x}", pmtimer.port()));

    // Create ACPI PM1 event/enable/control registers + reset handler
    let suspend_sink: Arc<dyn SuspendSink> =
        vmm_devices::acpi_pm::HdlSuspendSink::new(
            machine.hdl().clone(),
            log.new(slog::o!("component" => "acpi-pm")),
        );
    let acpi_pm = vmm_devices::acpi_pm::AcpiPm::create_and_attach(
        machine.bus_pio(),
        Arc::clone(&suspend_sink),
        log.new(slog::o!("component" => "acpi-pm")),
    )
    .context("failed to attach ACPI PM registers")?;
    info!(log, "ACPI PM registers attached"; "pmbase" => format!("{:#x}", 0x400));

    // pvpanic device — guest writes to port 0x505 on kernel panic
    let pvpanic = vmm_devices::pvpanic::PvPanic::new(log.clone());
    {
        let pvpanic = Arc::clone(&pvpanic);
        let handler: Arc<vmm_core::pio::PioFn> =
            Arc::new(move |_offset, rwo| match rwo {
                vmm_core::common::RWOp::Read(ro) => {
                    ro.write_u8(pvpanic.pio_read());
                }
                vmm_core::common::RWOp::Write(wo) => {
                    pvpanic.pio_write(wo.read_u8());
                }
            });
        if let Err(e) = machine.bus_pio().register(
            vmm_devices::pvpanic::PVPANIC_IOPORT,
            1,
            handler,
        ) {
            slog::warn!(log, "failed to register pvpanic PIO handler"; "error" => %e);
        }
    }

    Ok(TimerAndPmDevices {
        pmtimer,
        acpi_pm,
        pvpanic,
        suspend_sink,
    })
}

/// Create and attach COM1/COM2 UARTs, set up serial I/O paths, and
/// optionally start the mdata agent on COM2.
fn setup_serial(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    cli: &Cli,
    log: &Logger,
) -> anyhow::Result<(Arc<LpcUart>, Arc<LpcUart>)> {
    // Create COM1 UART (IRQ 4, base port 0x3F8)
    let com1_irq: Arc<dyn vmm_core::intr_pins::IntrPin> =
        match pic.pin_handle(lpc::COM1_IRQ) {
            Some(pin) => pin,
            None => Arc::new(NoOpPin),
        };
    let com1 = LpcUart::new(com1_irq);
    com1.attach(machine.bus_pio(), lpc::COM1_BASE);
    info!(log, "COM1 attached"; "port" => format!("{:#x}", lpc::COM1_BASE));

    // Create COM2 UART (IRQ 3, base port 0x2F8)
    let com2_irq: Arc<dyn vmm_core::intr_pins::IntrPin> =
        match pic.pin_handle(lpc::COM2_IRQ) {
            Some(pin) => pin,
            None => Arc::new(NoOpPin),
        };
    let com2 = LpcUart::new(com2_irq);
    com2.attach(machine.bus_pio(), lpc::COM2_BASE);
    info!(log, "COM2 attached"; "port" => format!("{:#x}", lpc::COM2_BASE));

    // Parse COM port paths from -l flags
    let com1_path = find_lpc_device(&cli.lpc, "com1");
    let com2_path = find_lpc_device(&cli.lpc, "com2");

    // Set up COM1 serial I/O based on -l com1,<config>
    setup_serial_io(&com1, com1_path.as_deref(), "com1", log)?;

    // Set up COM2:
    // - "socket,/path" → Unix socket for host-side metadata agent
    // - absent/no --mdata-nics → built-in mdata agent
    // - other path → raw file I/O
    if let Some(ref path) = com2_path {
        if path.starts_with("socket,") {
            // Production zone: create Unix socket so the host-side
            // metadata agent (vmadmd) can connect and serve sdc:nics,
            // SSH keys, etc. to the guest via COM2.
            setup_serial_io(&com2, Some(path.as_str()), "com2", log)?;
        } else {
            setup_serial_io(&com2, Some(path.as_str()), "com2", log)?;
        }
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

/// Set up fw_cfg device with E820 memory map, ACPI tables (both
/// table-loader protocol and legacy GPA), SMBIOS tables, and boot order.
/// Returns the FwCfg handle (must be kept alive for the VM's lifetime).
fn setup_fwcfg_and_acpi(
    machine: &Machine,
    cli: &Cli,
    num_cpus: u32,
    mem_size: usize,
    tpm_acpi: Option<acpi::TpmAcpi>,
    log: &Logger,
) -> anyhow::Result<Arc<FwCfg>> {
    // Create fw_cfg device and attach to PIO bus
    let fwcfg = FwCfg::new();
    fwcfg.attach(machine.bus_pio());
    info!(log, "fw_cfg attached"; "selector_port" => "0x510", "data_port" => "0x511");

    // Add CPU count to fw_cfg (OVMF uses these for SMP initialization)
    let nb_cpus = (num_cpus as u16).to_le_bytes().to_vec();
    fwcfg.insert_legacy(0x05, nb_cpus.clone()); // NB_CPUS
    fwcfg.insert_legacy(0x0F, nb_cpus); // MAX_CPUS

    // Generate E820 memory map and add to fw_cfg
    let e820_data = acpi::build_e820(machine.mem_size());
    fwcfg.insert_named("etc/e820", e820_data);
    info!(log, "E820 memory map added to fw_cfg";
        "mem_size" => machine.mem_size(),
    );

    // Generate ACPI tables with table-loader protocol support.
    // The layout produces two separate blobs (tables + rsdp) with
    // blob-relative offsets that the UEFI firmware patches via the
    // etc/table-loader commands. When --vtpm is on, the TPM2 ACPI
    // table is appended (between SPCR and RSDT) so OVMF surfaces
    // the device to the guest.
    let acpi_layout =
        acpi::generate_acpi_layout_with_tpm2(num_cpus, None, tpm_acpi.clone());
    info!(log, "ACPI layout generated for table-loader protocol";
        "tables_size" => acpi_layout.tables.len(),
        "rsdp_size" => acpi_layout.rsdp.len(),
        "num_cpus" => num_cpus,
        "with_tpm2" => tpm_acpi.is_some(),
    );

    // Register the ACPI blobs as fw_cfg named entries
    fwcfg.insert_named("etc/acpi/tables", acpi_layout.tables.clone());
    info!(log, "ACPI fw_cfg entry added"; "name" => "etc/acpi/tables",
        "size" => acpi_layout.tables.len());
    fwcfg.insert_named("etc/acpi/rsdp", acpi_layout.rsdp.clone());
    info!(log, "ACPI fw_cfg entry added"; "name" => "etc/acpi/rsdp",
        "size" => acpi_layout.rsdp.len());

    // Generate and register the table-loader command array
    let table_loader = acpi::generate_table_loader(&acpi_layout);
    let loader_cmds = table_loader.len() / 128;
    fwcfg.insert_named("etc/table-loader", table_loader);
    info!(log, "ACPI table-loader registered";
        "commands" => loader_cmds,
    );

    // Add boot order — tell firmware which device to boot from.
    // Format matches bhyve's pci_emul.c: /pci@i0cf8/pci@<slot>,<func>
    fwcfg.insert_named("bootorder", bootorder::build_bootorder(&cli.pci_slot)?);
    info!(log, "boot order configured via fw_cfg");

    // Write legacy GPA-based ACPI tables to guest memory. Threading the
    // same TPM description here keeps the BIOS-scan and UEFI views in sync.
    write_legacy_acpi_tables(machine, num_cpus, tpm_acpi, log);

    // Generate and write SMBIOS tables
    write_smbios_tables(machine, &fwcfg, cli, num_cpus, mem_size, log);

    Ok(fwcfg)
}

/// Generate legacy GPA-based ACPI tables and place their RSDP in the BIOS
/// scan area for compatibility with both UEFI and BIOS discovery paths.
fn write_legacy_acpi_tables(
    machine: &Machine,
    num_cpus: u32,
    tpm_acpi: Option<acpi::TpmAcpi>,
    log: &Logger,
) {
    let acpi_tables = acpi::generate_acpi_tables_with_tpm2(num_cpus, tpm_acpi);

    // Write RSDP at 0xE0000 for easier scan discovery
    let rsdp_size = 36;
    if acpi_tables.data.len() >= rsdp_size {
        if let Err(e) = machine
            .memctx()
            .write(0xE0000, &acpi_tables.data[..rsdp_size])
        {
            warn!(log, "failed to write RSDP at 0xE0000"; "error" => %e);
        } else {
            info!(log, "RSDP written at 0xE0000");
        }
    }

    // Write full tables to bhyve legacy base for non-UEFI/direct-mapped scenarios.
    if let Err(e) = machine
        .memctx()
        .write(acpi::BHYVE_ACPI_BASE, &acpi_tables.data)
    {
        warn!(log, "failed to write ACPI tables to guest memory (may be OK if region not mapped)";
            "gpa" => format!("{:#x}", acpi::BHYVE_ACPI_BASE),
            "error" => %e,
        );
    } else {
        info!(log, "ACPI tables written to guest memory";
            "gpa" => format!("{:#x}", acpi::BHYVE_ACPI_BASE),
            "size" => acpi_tables.data.len(),
        );
    }
}

/// Generate SMBIOS tables, add to fw_cfg, and write to the legacy scan
/// area in guest memory.
fn write_smbios_tables(
    machine: &Machine,
    fwcfg: &FwCfg,
    cli: &Cli,
    num_cpus: u32,
    mem_size: usize,
    log: &Logger,
) {
    let smbios_config = if let Some(ref b_flag) = cli.smbios {
        let mut cfg = smbios::parse_smbios_flag(b_flag);
        cfg.vm_name = cli.vm_name.clone();
        cfg.uuid = cli.uuid.clone();
        cfg.num_cpus = num_cpus;
        cfg.memory_mb = (mem_size / (1024 * 1024)) as u64;
        cfg
    } else {
        smbios::SmbiosConfig {
            vm_name: cli.vm_name.clone(),
            uuid: cli.uuid.clone(),
            num_cpus,
            memory_mb: (mem_size / (1024 * 1024)) as u64,
            manufacturer: None,
            product: None,
            version: None,
            serial: None,
            sku: None,
            family: None,
        }
    };
    let (smbios_anchor, smbios_tables) =
        smbios::generate_smbios(&smbios_config);
    fwcfg.insert_named("etc/smbios/smbios-anchor", smbios_anchor.clone());
    fwcfg.insert_named("etc/smbios/smbios-tables", smbios_tables.clone());
    info!(log, "SMBIOS tables added to fw_cfg";
        "anchor_size" => smbios_anchor.len(),
        "tables_size" => smbios_tables.len(),
    );

    // Write SMBIOS tables to guest memory in the legacy scan area.
    // Linux scans 0xF0000-0xFFFFF for the "_SM_" entry point signature.
    // Write tables first (at 0xF1000), then the entry point (at 0xF0000)
    // with the table address patched in.
    {
        const SMBIOS_EP_GPA: u64 = 0xF0000;
        const SMBIOS_TABLE_GPA: u64 = 0xF1000;

        // Write structure tables
        if let Err(e) = machine.memctx().write(SMBIOS_TABLE_GPA, &smbios_tables)
        {
            warn!(log, "failed to write SMBIOS tables to guest memory"; "error" => %e);
        }

        // Patch the entry point with the actual table address
        let mut ep = smbios_anchor;
        if ep.len() >= 0x1F {
            // 0x18: Structure table address (4 bytes, little-endian)
            ep[0x18..0x1C]
                .copy_from_slice(&(SMBIOS_TABLE_GPA as u32).to_le_bytes());

            // Recompute intermediate checksum (bytes 0x10..0x1F)
            ep[0x15] = 0;
            let sum: u8 =
                ep[0x10..0x1F].iter().fold(0u8, |a, &b| a.wrapping_add(b));
            ep[0x15] = 0u8.wrapping_sub(sum);

            // Recompute entry point checksum (bytes 0x00..0x10)
            ep[0x04] = 0;
            let sum: u8 =
                ep[0x00..0x10].iter().fold(0u8, |a, &b| a.wrapping_add(b));
            ep[0x04] = 0u8.wrapping_sub(sum);
        }

        if let Err(e) = machine.memctx().write(SMBIOS_EP_GPA, &ep) {
            warn!(log, "failed to write SMBIOS entry point to guest memory"; "error" => %e);
        } else {
            info!(log, "SMBIOS written to guest memory";
                "entry_point" => format!("{:#x}", SMBIOS_EP_GPA),
                "tables" => format!("{:#x}", SMBIOS_TABLE_GPA),
            );
        }
    }
}

/// Parse all `-s` PCI slot specifications and create the corresponding
/// devices. Returns PCI handles (must be kept alive) and lifecycle handles
/// for pause/resume orchestration. The AHCI flag lets the control path reject
/// migration without re-parsing the original specifications.
fn init_pci_devices(
    machine: &Machine,
    chipset: &I440FxChipset,
    cli: &Cli,
    num_vcpus: u32,
    input: &Arc<InputBroker>,
    log: &Logger,
) -> anyhow::Result<PciDeviceInventory> {
    let mut pci_devices: Vec<PciDeviceHandle> = Vec::new();
    let mut lifecycle_devices: Vec<LifecycleHandle> = Vec::new();
    let mut has_ahci_cd = false;
    let is_migrate = cli.migrate_listen.is_some();
    eprintln!(
        "[vmm] init_pci_devices: migrate_listen={:?} is_migrate={}",
        cli.migrate_listen, is_migrate
    );

    reject_unmigratable_destination(is_migrate, &cli.pci_slot)?;

    for slot_spec in &cli.pci_slot {
        let (device_spec, _) = bootorder::strip_bootindex(slot_spec)?;
        match parse_and_create_pci_device(
            &device_spec,
            num_vcpus,
            chipset,
            machine.bus_pio(),
            machine.bus_mmio(),
            machine.physmap(),
            machine.hdl(),
            machine.segids(),
            cli.migrate_listen.is_some(),
            &cli.vm_name,
            input,
            log,
        ) {
            Ok((Some(pci), lifecycle)) => {
                has_ahci_cd |= pci_spec_driver(&device_spec) == Some("ahci-cd");
                pci_devices.push(pci);
                if let Some(lc) = lifecycle {
                    lifecycle_devices.push(lc);
                }
            }
            Ok((None, _)) => {} // recognized but no Arc to hold
            Err(e) => {
                error!(log, "failed to create PCI device";
                    "spec" => slot_spec,
                    "error" => format!("{:#}", e),
                );
                return Err(e);
            }
        }
    }

    Ok((pci_devices, lifecycle_devices, has_ahci_cd))
}

fn pci_spec_driver(spec: &str) -> Option<&str> {
    spec.split(',').nth(1)
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

/// If direct-booting a kernel, load it into guest RAM and return the
/// entry point address.
fn load_direct_boot_kernel(
    machine: &Machine,
    cli: &Cli,
    kernel_image: &Option<direct_boot::KernelImage>,
    initrd_image: &Option<direct_boot::InitrdImage>,
    mem_size: usize,
    log: &Logger,
) -> anyhow::Result<()> {
    if let Some(ref kernel) = kernel_image {
        let cmdline = cli
            .cmdline
            .as_deref()
            .unwrap_or("console=ttyS0 earlyprintk=serial");
        let acpi_rsdp_addr = acpi::BHYVE_ACPI_BASE;
        let entry = direct_boot::load_kernel(
            machine.memctx(),
            kernel,
            cmdline,
            initrd_image.as_ref(),
            mem_size,
            acpi_rsdp_addr,
        )?;
        info!(log, "kernel loaded for direct boot";
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
                // Restore virtio queue state
                let migq: Vec<_> = queue_states
                    .iter()
                    .map(|qs| vmm_devices::lifecycle::VirtioMigrateQueue {
                        queue_idx: qs.queue_idx,
                        queue_size: qs.queue_size,
                        desc_addr: qs.desc_addr,
                        avail_addr: qs.avail_addr,
                        used_addr: qs.used_addr,
                        enabled: qs.enabled,
                        features: qs.features,
                        avail_idx: qs.avail_idx,
                        used_idx: qs.used_idx,
                        msix_addr: qs.msix_addr,
                        msix_data: qs.msix_data,
                        msix_vector: qs.msix_vector,
                        pci_state: qs.pci_state.clone().map(|pci| {
                            vmm_devices::lifecycle::VirtioMigratePciState {
                                command: pci.command,
                                bar_addrs: pci.bar_addrs,
                            }
                        }),
                    })
                    .collect();
                for dev in &devs {
                    dev.restore_virtio_state(&migq);
                }
                // Restore NVMe controller state
                for nvme in nvme_states {
                    use vmm_devices::lifecycle::{
                        NvmeMigrateCq, NvmeMigrateSq, NvmeMigrateState,
                    };
                    let mig = NvmeMigrateState {
                        cc: nvme.cc,
                        csts: nvme.csts,
                        aqa: nvme.aqa,
                        asq_base: nvme.asq_base,
                        acq_base: nvme.acq_base,
                        admin_sq: nvme.admin_sq.as_ref().map(|sq| {
                            NvmeMigrateSq {
                                base: sq.base,
                                size: sq.size,
                                head: sq.head,
                                tail: sq.tail,
                                cq_id: sq.cq_id,
                            }
                        }),
                        admin_cq: nvme.admin_cq.as_ref().map(|cq| {
                            NvmeMigrateCq {
                                base: cq.base,
                                size: cq.size,
                                head: cq.head,
                                tail: cq.tail,
                                phase: cq.phase,
                                iv: cq.iv,
                            }
                        }),
                        io_sqs: nvme
                            .io_sqs
                            .iter()
                            .map(|sq| {
                                sq.as_ref().map(|sq| NvmeMigrateSq {
                                    base: sq.base,
                                    size: sq.size,
                                    head: sq.head,
                                    tail: sq.tail,
                                    cq_id: sq.cq_id,
                                })
                            })
                            .collect(),
                        io_cqs: nvme
                            .io_cqs
                            .iter()
                            .map(|cq| {
                                cq.as_ref().map(|cq| NvmeMigrateCq {
                                    base: cq.base,
                                    size: cq.size,
                                    head: cq.head,
                                    tail: cq.tail,
                                    phase: cq.phase,
                                    iv: cq.iv,
                                })
                            })
                            .collect(),
                        num_io_queues: nvme.num_io_queues,
                        pci_command: nvme.pci_command,
                        pci_bar_addrs: nvme.pci_bar_addrs,
                        msix_entries: nvme.msix_entries.clone(),
                        msix_enabled: nvme.msix_enabled,
                    };
                    for dev in &devs {
                        dev.restore_nvme_state(&mig);
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

/// Normal (non-migration) vCPU activation: activate all vCPUs, set
/// capabilities, and configure the BSP for either direct-boot or UEFI.
fn activate_vcpus(
    machine: &Machine,
    cli: &Cli,
    kernel_image: &Option<direct_boot::KernelImage>,
    log: &Logger,
) -> anyhow::Result<()> {
    // Activate all vCPUs and set capabilities
    for vcpu in machine.vcpus() {
        vcpu.activate().with_context(|| {
            format!("failed to activate vCPU {}", vcpu.id())
        })?;

        if cli.vmexit_on_hlt {
            machine
                .hdl()
                .set_capability(
                    vcpu.id(),
                    bhyve_api::vm_cap_type::VM_CAP_HALT_EXIT as i32,
                    1,
                )
                .with_context(|| {
                    format!("failed to set HALT_EXIT on vCPU {}", vcpu.id())
                })?;
        }
    }

    // Set up BSP based on boot mode
    if let Some(ref kernel) = kernel_image {
        if let Some(bsp) = machine.vcpus().first() {
            direct_boot::setup_direct_boot_bsp(bsp, kernel.entry_point_64())
                .context("failed to set up BSP for direct boot")?;
        }
        info!(log, "BSP configured for direct boot");
    } else {
        // UEFI boot: only the BSP gets reset + RIP set.
        // APs must NOT be reset — they start in INIT state and
        // wait for SIPI from the BSP. Calling reboot_state() on
        // APs disrupts the kernel's INIT/SIPI handling and causes
        // massive SMP boot delays.
        if let Some(bsp) = machine.vcpus().first() {
            bsp.setup_bsp().context("failed to set up BSP")?;
        }
    }

    Ok(())
}

/// Spawn per-vCPU threads and return the event channel and join handles.
fn spawn_vcpus(
    machine: &Machine,
    num_cpus: u32,
    api_version: u32,
    hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    log: &Logger,
) -> (
    mpsc::Receiver<vcpu_tasks::VcpuEvent>,
    Vec<JoinHandle<()>>,
    Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
) {
    info!(log, "starting vCPU threads");

    // Create per-vCPU metrics
    let vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>> = (0
        ..num_cpus)
        .map(|_| Arc::new(vmm_devices::metrics::VcpuMetrics::new()))
        .collect();

    // Spawn vCPU threads and run until suspend/error
    let (event_rx, handles) = vcpu_tasks::spawn_vcpu_threads(
        machine.vcpus(),
        machine.bus_pio(),
        machine.bus_mmio(),
        machine.hdl().clone(),
        api_version,
        &vcpu_metrics,
        hyperv,
        log,
    );

    (event_rx, handles, vcpu_metrics)
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

/// Optionally spawn the control socket listener thread.
fn spawn_control_socket(
    machine: &Machine,
    cli: &Cli,
    num_cpus: u32,
    mem_size: usize,
    vcpu_metrics: &[Arc<vmm_devices::metrics::VcpuMetrics>],
    devices: Vec<Arc<dyn vmm_devices::Lifecycle>>,
    has_ahci_cd: bool,
    host_local_state: host_state::HostLocalState,
    log: &Logger,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(sock_path) = cli.control_socket.as_ref() else {
        return Ok(None);
    };
    let ctrl = control::VmController::new(
        machine.hdl().clone(),
        machine.physmap().clone(),
        cli.vm_name.clone(),
        num_cpus,
        mem_size,
        vcpu_metrics.to_vec(),
        devices,
        has_ahci_cd,
        cli.pci_slot.clone(),
        cli.lpc.clone(),
        cli.cpu_baseline.clone(),
        host_local_state,
        log.clone(),
    );
    let (_handle, path) =
        control::spawn_control_thread(sock_path, ctrl, log.clone())?;
    info!(log, "control socket listening"; "path" => path.display().to_string());
    Ok(Some(path))
}

/// Install the SIGTERM signal handler and spawn a poller thread that
/// triggers ACPI poweroff when the signal is received.
fn install_sigterm_handler(machine: &Machine, log: &Logger) {
    let log_sig = log.clone();
    // libc::signal returns SIG_ERR on failure; safe to ignore
    let _ = unsafe {
        libc::signal(
            libc::SIGTERM,
            sigterm_handler as *const () as libc::sighandler_t,
        )
    };
    // Spawn a thread to wait for the signal flag
    let hdl_sig = machine.hdl().clone();
    thread::Builder::new()
        .name("sigterm-handler".into())
        .spawn(move || {
            loop {
                thread::sleep(std::time::Duration::from_millis(500));
                if SIGTERM_RECEIVED.load(std::sync::atomic::Ordering::Relaxed) {
                    info!(
                        log_sig,
                        "SIGTERM received, triggering ACPI poweroff"
                    );
                    // best-effort: if suspend fails, VM is already gone
                    let _ = hdl_sig.suspend(
                        bhyve_api::vm_suspend_how::VM_SUSPEND_POWEROFF as u32,
                        0,
                    );
                    break;
                }
            }
        })
        .expect("failed to spawn sigterm handler thread");
}

/// Wait for vCPU events, join all threads, clean up control socket and
/// destroy the VM.
fn run_event_loop(
    machine: &Machine,
    event_rx: mpsc::Receiver<vcpu_tasks::VcpuEvent>,
    handles: Vec<JoinHandle<()>>,
    control_cleanup: Option<PathBuf>,
    devices: &[Arc<dyn vmm_devices::Lifecycle>],
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    // Wait for the first vCPU event
    let outcome = match event_rx.recv() {
        Ok(vcpu_tasks::VcpuEvent::Suspended { vcpu_id, kind }) => {
            info!(log, "VM suspended";
                "vcpu" => vcpu_id,
                "kind" => ?kind,
            );
            Ok(match kind {
                Suspend::Reset => RunOutcome::Reboot,
                Suspend::PowerOff => RunOutcome::PowerOff,
                Suspend::Halt => RunOutcome::Halt,
                Suspend::TripleFault(src) => RunOutcome::TripleFault(src),
            })
        }
        Ok(vcpu_tasks::VcpuEvent::Error {
            vcpu_id,
            error: err,
        }) => {
            error!(log, "vCPU error";
                "vcpu" => vcpu_id,
                "error" => &err,
            );
            Ok(RunOutcome::GuestFault)
        }
        Err(_) => {
            warn!(log, "all vCPU threads exited without events");
            Ok(RunOutcome::Halt)
        }
    };

    // A wedged backend must not prevent release of the bhyve instance.
    let _report = quiesce_devices(devices, log);

    if !join_vcpus_bounded(handles, std::time::Duration::from_secs(5)) {
        warn!(log, "vCPU threads did not exit; destroying VM anyway");
    }

    // Clean up control socket (best-effort; may already be removed)
    if let Some(ref path) = control_cleanup {
        let _ = std::fs::remove_file(path);
    }

    // Clean up VM
    if let Err(e) = machine.hdl().destroy() {
        warn!(log, "failed to destroy VM"; "error" => %e);
    }

    info!(log, "VM shutdown complete");
    outcome
}

fn run(cli: Cli, log: &Logger) -> anyhow::Result<RunOutcome> {
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
        host_local_state,
    } = creation;

    // Phase 2: Initialize chipset and kernel-emulated devices
    let chipset_devs = init_chipset(&machine, log)?;

    // Phase 3: Timers, PM, pvpanic
    let _timer_pm_devs = setup_timers_and_pm(&machine, log)?;

    let input = Arc::new(InputBroker::default());

    // Phase 4: Serial ports and mdata agent
    let (_com1, _com2) = setup_serial(&machine, &chipset_devs.pic, &cli, log)?;
    let _ps2 = setup_ps2(
        &machine,
        &chipset_devs.pic,
        Arc::clone(&_timer_pm_devs.suspend_sink),
        &input,
        log,
    );

    // Phase 5: fw_cfg, ACPI tables, SMBIOS, E820, boot order
    let _fwcfg = setup_fwcfg_and_acpi(
        &machine,
        &cli,
        num_cpus,
        mem_size,
        tpm_acpi.clone(),
        log,
    )?;

    // Phase 6: PCI devices
    let (_pci_devices, mut lifecycle_devices, has_ahci_cd) = init_pci_devices(
        &machine,
        &chipset_devs.chipset,
        &cli,
        num_cpus,
        &input,
        log,
    )?;
    if let Some(vs) = varstore.clone() {
        lifecycle_devices.push(vs as Arc<dyn vmm_devices::Lifecycle>);
    }

    // Phase 7: Direct boot kernel loading (after ACPI tables are ready)
    load_direct_boot_kernel(
        &machine,
        &cli,
        &kernel_image,
        &initrd_image,
        mem_size,
        log,
    )?;

    // Phase 8: vCPU activation (migration or normal)
    if let Some(ref listen_addr) = cli.migrate_listen {
        run_migration_destination(
            &machine,
            listen_addr,
            num_cpus,
            mem_size,
            &lifecycle_devices,
            log,
        )?;
    } else {
        activate_vcpus(&machine, &cli, &kernel_image, log)?;
    }

    // Phase 9: Spawn vCPU threads
    let (event_rx, handles, vcpu_metrics) =
        spawn_vcpus(&machine, num_cpus, api_version, hyperv.clone(), log);

    // Phase 10: Post-migration timer kick + delayed virtio interrupt
    if cli.migrate_listen.is_some() {
        post_migration_timer_kick(&machine, log);

        // Re-inject virtio interrupts periodically after vCPUs start.
        // The initial post-restore interrupt fires before resume but
        // may not be sufficient for virtio-blk (the guest's blk driver
        // may need multiple interrupt cycles to fully process stale
        // used ring entries and resume I/O).
        let devs = lifecycle_devices.clone();
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

    // Phase 11: Control socket
    let control_cleanup = spawn_control_socket(
        &machine,
        &cli,
        num_cpus,
        mem_size,
        &vcpu_metrics,
        lifecycle_devices.clone(),
        has_ahci_cd,
        host_local_state,
        log,
    )?;

    // Phase 12: Signal handling
    install_sigterm_handler(&machine, log);

    // Phase 13: Main event loop and cleanup
    run_event_loop(
        &machine,
        event_rx,
        handles,
        control_cleanup,
        &lifecycle_devices,
        log,
    )
}

/// Create a virtio-blk PCI device from a config string.
fn create_virtio_blk_device(
    config: &str,
    bdf: Bdf,
    num_vcpus: u32,
    chipset: &I440FxChipset,
    physmap: &Arc<vmm_core::mem::PhysMap>,
    bus_pio: &Arc<vmm_core::pio::PioBus>,
    bus_mmio: &Arc<vmm_core::mmio::MmioBus>,
    vmm_hdl: &Arc<vmm_core::hdl::VmmHdl>,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioBlock>>> {
    let (path, opts) = parse_blk_config(config);

    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .read(true)
        .write(!opts.read_only)
        .open(path)
        .with_context(|| format!("failed to open disk: {}", path))?;
    // Default num_queues: min(vcpus, 4). A zvol has one SPA sync pipeline
    // so beyond ~4 queues there's no throughput gain. Caller can override
    // via `num-queues=N` (1..=8) on the `-s` config string.
    let num_queues: u16 = opts
        .num_queues
        .unwrap_or_else(|| std::cmp::min(num_vcpus as u16, 4).max(1));
    let blk = VirtioBlock::new(file, &opts, Arc::clone(physmap), num_queues)
        .with_context(|| {
            format!("failed to create virtio-blk for: {}", path)
        })?;

    let lintr = chipset.route_lintr(&bdf);
    let cfg_size = blk.config_size();
    let blk_intr = Arc::clone(&blk.interrupt);

    // MSI-X: one vector per queue + one for config change
    let num_msix_vectors = num_queues + 1;
    let msix = Arc::new(MsixTable::new(num_msix_vectors, Arc::clone(vmm_hdl)));

    let pci_dev = VirtioPciDevice::new(
        blk,
        vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_BLOCK,
        num_queues as usize,
        128, // queue size
        cfg_size,
        lintr,
        Arc::clone(physmap),
        Arc::clone(bus_pio),
        Arc::clone(bus_mmio),
        Some(msix),
    );

    // Wire interrupt: workers call raise_queue_interrupt_for() to
    // route via MSI-X vector when enabled, fallback to legacy INTx.
    {
        let pci_weak = Arc::downgrade(&pci_dev);
        *blk_intr.lock().expect("lock") = Some(Arc::new(move || {
            if let Some(dev) = pci_weak.upgrade() {
                dev.raise_queue_interrupt_for(0);
            }
        }));
    }

    chipset.attach_device(bdf, pci_dev.clone());
    Ok(pci_dev)
}

fn parse_ahci_cd_path(config: &str) -> anyhow::Result<&str> {
    if config.is_empty() {
        anyhow::bail!("ahci-cd requires an ISO path");
    }
    let path = config.split(',').next().unwrap_or(config);
    if let Some(opt) = config.split(',').skip(1).find(|o| !o.is_empty()) {
        // Rejecting unknown options prevents a read-only device from
        // appearing to honor a writable configuration.
        anyhow::bail!("ahci-cd takes no options, got '{}'", opt);
    }
    Ok(path)
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
fn parse_and_create_pci_device(
    spec: &str,
    num_vcpus: u32,
    chipset: &I440FxChipset,
    bus_pio: &Arc<vmm_core::pio::PioBus>,
    bus_mmio: &Arc<vmm_core::mmio::MmioBus>,
    physmap: &Arc<vmm_core::mem::PhysMap>,
    vmm_hdl: &Arc<vmm_core::hdl::VmmHdl>,
    segids: &vmm_core::mem::SegidAlloc,
    is_migrate_dest: bool,
    vm_name: &str,
    input: &Arc<InputBroker>,
    log: &Logger,
) -> anyhow::Result<CreatedPciDevice> {
    let parts: Vec<&str> = spec.splitn(3, ',').collect();
    if parts.len() < 2 {
        anyhow::bail!(
            "invalid PCI slot spec '{}': need slot,driver[,config]",
            spec
        );
    }

    let bdf = parse_bdf(parts[0]).ok_or_else(|| {
        anyhow::anyhow!("invalid BDF '{}' in spec '{}'", parts[0], spec)
    })?;
    let driver = parts[1];
    let config = parts.get(2).copied().unwrap_or("");

    match driver {
        "virtio-blk" | "virtio-blk-pci" => {
            if config.is_empty() {
                anyhow::bail!("virtio-blk requires a disk path");
            }
            let pci_dev = create_virtio_blk_device(
                config, bdf, num_vcpus, chipset, physmap, bus_pio, bus_mmio,
                vmm_hdl,
            )?;
            info!(log, "virtio-blk attached";
                "bdf" => %bdf,
                "path" => config,
                "msix" => "2 vectors",
            );
            Ok((
                Some(pci_dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(pci_dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "virtio-net-viona" => {
            if config.is_empty() {
                anyhow::bail!("virtio-net-viona requires a VNIC name");
            }
            // Parse config: vnic_name[,promiscphys=true]
            let (vnic_name, promiscphys) = if let Some(comma) = config.find(',')
            {
                let name = &config[..comma];
                let promisc = config[comma + 1..].split(',').any(|opt| {
                    opt == "promiscphys" || opt == "promiscphys=true"
                });
                (name, promisc)
            } else {
                (config, false)
            };

            use std::os::fd::AsRawFd;
            let viona = VirtioViona::new(vnic_name, vmm_hdl.as_raw_fd())
                .with_context(|| {
                    format!(
                        "failed to create viona device for VNIC: {}",
                        vnic_name
                    )
                })?;

            if promiscphys {
                viona.set_promisc(true).with_context(|| {
                    format!("failed to set promiscphys on {}", vnic_name)
                })?;
            }

            // MSI-X: one vector per queue (rx + tx) + config change
            let num_viona_queues =
                vmm_devices::virtio::viona::NET_NUM_QUEUES as u16;
            let msix = Arc::new(MsixTable::new(
                num_viona_queues + 1,
                Arc::clone(vmm_hdl),
            ));

            // Start interrupt polling BEFORE moving viona into PCI device.
            // The polling thread needs the viona fd, and we pass a per-queue
            // callback that routes via the correct MSI-X vector.
            let intr_hook: Arc<
                std::sync::Mutex<
                    Option<std::sync::Weak<VirtioPciDevice<VirtioViona>>>,
                >,
            > = Arc::new(std::sync::Mutex::new(None));
            let hook_clone = Arc::clone(&intr_hook);
            let raise_fn: Arc<dyn Fn(u16) + Send + Sync> =
                Arc::new(move |queue_idx: u16| {
                    if let Some(dev) = hook_clone
                        .lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .and_then(|weak| weak.upgrade())
                    {
                        dev.raise_queue_interrupt_for(queue_idx);
                    }
                });
            // On migrate dest, store the poll parameters but don't start
            // the thread yet — rings haven't been programmed. The thread
            // will be started by start_poll_deferred() after restore.
            if is_migrate_dest {
                viona.store_poll_fn(
                    raise_fn,
                    log.new(slog::o!("component" => "viona-intr")),
                );
            } else {
                viona.start_intr_poll(
                    raise_fn,
                    log.new(slog::o!("component" => "viona-intr")),
                );
            }

            let lintr = chipset.route_lintr(&bdf);

            let pci_dev = VirtioPciDevice::new(
                viona,
                vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_NET,
                vmm_devices::virtio::viona::NET_NUM_QUEUES,
                vmm_devices::virtio::viona::NET_QUEUE_SIZE,
                vmm_devices::virtio::viona::NET_CONFIG_SIZE,
                lintr,
                Arc::clone(physmap),
                Arc::clone(bus_pio),
                Arc::clone(bus_mmio),
                Some(msix),
            );

            // Now wire up the interrupt hook with a weak reference
            if let Ok(mut hook) = intr_hook.lock() {
                *hook = Some(Arc::downgrade(&pci_dev));
            }

            chipset.attach_device(bdf, pci_dev.clone());
            info!(log, "virtio-net-viona attached";
                "bdf" => %bdf,
                "vnic" => vnic_name,
                "promiscphys" => promiscphys,
                "msix" => "3 vectors",
            );
            Ok((
                Some(pci_dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(pci_dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "nvme" => {
            if config.is_empty() {
                anyhow::bail!("nvme requires a disk path");
            }
            let (path, blk_opts) = parse_blk_config(config);
            use std::fs::OpenOptions;
            let file = OpenOptions::new()
                .read(true)
                .write(!blk_opts.read_only)
                .open(path)
                .with_context(|| format!("failed to open disk: {}", path))?;

            let nvme = vmm_devices::nvme::NvmeController::new(
                file,
                blk_opts.read_only,
                bdf.dev(), // unique instance ID per slot
                Arc::clone(physmap),
                Arc::clone(vmm_hdl),
                Arc::clone(bus_mmio),
            );

            chipset.attach_device(bdf, nvme.clone());
            info!(log, "nvme attached";
                "bdf" => %bdf,
                "path" => path,
            );
            Ok((
                Some(nvme.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(nvme as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "virtio-rnd" => {
            let rng = vmm_devices::virtio::VirtioRng::new();
            let lintr = chipset.route_lintr(&bdf);
            // MSI-X: 1 queue + config change = 2 vectors
            let msix = Arc::new(MsixTable::new(2, Arc::clone(vmm_hdl)));
            let pci_dev = VirtioPciDevice::new(
                rng,
                vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_RNG,
                1,  // 1 queue
                64, // queue size
                0,  // no device-specific config
                lintr,
                Arc::clone(physmap),
                Arc::clone(bus_pio),
                Arc::clone(bus_mmio),
                Some(msix),
            );
            chipset.attach_device(bdf, pci_dev.clone());
            info!(log, "virtio-rnd attached"; "bdf" => %bdf, "msix" => "2 vectors");
            Ok((
                Some(pci_dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(pci_dev as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "fbuf" => {
            let segid = segids.alloc().ok_or_else(|| {
                anyhow::anyhow!(
                    "no free memory segment for fbuf (kernel limit is {} per VM)",
                    vmm_core::mem::VM_MAX_MEMSEGS
                )
            })?;

            // Parse config: vga=off,unix=/path[,password=pwd][,w=N][,h=N]
            let mut vnc_path =
                PathBuf::from("/var/run/vmm").join(vm_name).join("vnc.sock");
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

            let fbuf = vmm_devices::fbuf::Framebuffer::new(
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
            chipset.attach_device(bdf, fbuf.clone());
            info!(log, "fbuf attached";
                "bdf" => %bdf,
                "vnc" => vnc_path.display().to_string(),
                "resolution" => format!("{}x{}", width, height),
            );
            Ok((
                Some(fbuf.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(fbuf as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "xhci" => {
            // xHCI USB controller with tablet device
            // Config is just "tablet" (the attached USB device)
            let lintr = chipset.route_lintr(&bdf);
            let intr_pin = lintr.map(|(_, pin)| pin);
            let xhci = vmm_devices::xhci::XhciController::new(
                Arc::clone(physmap),
                Arc::clone(bus_mmio),
                intr_pin,
            );
            chipset.attach_device(bdf, xhci.clone());
            input.set_pointer(xhci.clone());
            info!(log, "xhci tablet attached"; "bdf" => %bdf);
            Ok((
                Some(xhci.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(xhci as Arc<dyn vmm_devices::Lifecycle>),
            ))
        }
        "passthru" => {
            if config.is_empty() {
                anyhow::bail!(
                    "passthru requires a device path (e.g., /dev/ppt0)"
                );
            }
            // Config is just the path to /dev/pptN
            let path = config.split(',').next().unwrap_or(config);
            let dev = vmm_devices::passthru::PciPassthru::new(
                path,
                Arc::clone(vmm_hdl),
            )
            .with_context(|| {
                format!("failed to init passthru device: {}", path)
            })?;
            chipset.attach_device(bdf, dev.clone());
            info!(log, "passthru attached";
                "bdf" => %bdf,
                "path" => path,
            );
            Ok((Some(dev as Arc<dyn vmm_devices::pci::PciDevice>), None))
        }
        "ahci-cd" => {
            let path = parse_ahci_cd_path(config)?;
            let media = vmm_devices::ahci::media::IsoMedia::open(
                std::path::Path::new(path),
            )
            .with_context(|| format!("failed to open ISO: {}", path))?;
            let lintr = chipset.route_lintr(&bdf);
            let ahci = vmm_devices::ahci::AhciCtrl::new(
                media,
                Arc::clone(physmap),
                Arc::clone(bus_mmio),
                lintr.map(|(_, pin)| pin),
                log.new(slog::o!("component" => "ahci-cd")),
            );
            chipset.attach_device(bdf, ahci.clone());
            info!(log, "ahci-cd attached"; "bdf" => %bdf, "path" => path);
            Ok((
                Some(ahci.clone() as Arc<dyn vmm_devices::pci::PciDevice>),
                Some(ahci as Arc<dyn vmm_devices::Lifecycle>),
            ))
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

/// Parse a virtio-blk config string: `path[,nodelete][,sectorsize=N][,ro]`
///
/// Returns the disk path and options struct.
fn parse_blk_config(config: &str) -> (&str, VirtioBlockOpts) {
    let mut opts = VirtioBlockOpts::default();
    let mut path = config;

    // The first comma-separated token is the path; the rest are options.
    // But paths can't contain commas, so split on comma is safe.
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
                _ => {} // ignore unknown options (forward compat)
            }
        }
    }

    (path, opts)
}

/// Parse a BDF string: "dev", "dev:func", or "bus:dev:func"
fn parse_bdf(s: &str) -> Option<Bdf> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            let dev: u8 = parts[0].parse().ok()?;
            Bdf::new(0, dev, 0)
        }
        2 => {
            let dev: u8 = parts[0].parse().ok()?;
            let func: u8 = parts[1].parse().ok()?;
            Bdf::new(0, dev, func)
        }
        3 => {
            let bus: u8 = parts[0].parse().ok()?;
            let dev: u8 = parts[1].parse().ok()?;
            let func: u8 = parts[2].parse().ok()?;
            Bdf::new(bus, dev, func)
        }
        _ => None,
    }
}

/// Extract a device config from `-l` flags (e.g., `com1,/dev/zconsole`).
fn find_lpc_device(lpc_args: &[String], device: &str) -> Option<String> {
    let prefix = format!("{},", device);
    for arg in lpc_args {
        if let Some(config) = arg.strip_prefix(&prefix) {
            return Some(config.to_string());
        }
    }
    None
}

/// Set up serial I/O for a COM port.
///
/// Supports:
/// - `stdio` or absent → stdout/stdin
/// - `socket,/path` → Unix domain socket (host metadata agent connects)
/// - `/dev/zconsole` or any path → open as read/write device
fn setup_serial_io(
    uart: &Arc<LpcUart>,
    config: Option<&str>,
    name: &str,
    log: &Logger,
) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    // Handle "socket,/path" — create a Unix domain socket listener
    // that the host-side metadata agent connects to.
    if let Some(path) = config.and_then(|c| c.strip_prefix("socket,")) {
        setup_serial_socket(uart, path, name, log)?;
        return Ok(());
    }

    match config {
        None | Some("stdio") => {
            // stdout for TX, stdin for RX
            uart.set_tx_sink(Box::new(|b: u8| {
                if let Err(e) = std::io::stdout().write_all(&[b]) {
                    eprintln!("[serial] stdout write error: {}", e);
                }
                let _ = std::io::stdout().flush();
            }));

            let uart_input = uart.clone();
            thread::Builder::new()
                .name(format!("{name}-input"))
                .spawn(move || {
                    let stdin = std::io::stdin();
                    let mut buf = [0u8; 1];
                    loop {
                        match stdin.lock().read(&mut buf) {
                            Ok(0) => break,
                            Ok(_) => {
                                uart_input.input_byte(buf[0]);
                            }
                            Err(_) => break,
                        }
                    }
                })
                .expect("failed to spawn serial input thread");
            info!(log, "{name} connected to stdio");
        }
        Some(path) => {
            // Open using libc::open() like C-bhyve to ensure proper
            // pty initialization on illumos. Rust's File::open may
            // not trigger the same STREAMS module setup.
            let c_path = std::ffi::CString::new(path).unwrap();
            let fd = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                {
                    // Match C-bhyve: cfmakeraw + CLOCAL, blocking mode.
                    // C-bhyve opens with O_NONBLOCK but then leaves it
                    // in blocking mode (the O_NONBLOCK is only for the
                    // open() call to avoid carrier wait). Blocking writes
                    // to zconsole work because zoneadmd holds the master
                    // side open, preventing buffer-full stalls.
                    #[cfg(target_os = "illumos")]
                    unsafe {
                        let mut termios: libc::termios = std::mem::zeroed();
                        if libc::tcgetattr(fd, &mut termios) == 0 {
                            libc::cfmakeraw(&mut termios);
                            termios.c_cflag |= libc::CLOCAL;
                            libc::tcsetattr(fd, libc::TCSANOW, &termios);
                        }
                        // Keep O_NONBLOCK set. On illumos, open() with
                        // O_NONBLOCK is transient, but we explicitly set
                        // it for writes: zconsole writes must not block
                        // the vCPU when no console client is connected
                        // (pty buffer full). The read path uses poll()
                        // so EAGAIN is handled correctly.
                        libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                    }

                    let is_tty = unsafe { libc::isatty(fd) } != 0;
                    eprintln!(
                        "[serial] fd={} isatty={} path={}",
                        fd, is_tty, path
                    );

                    // TX: dup the fd for writes
                    // Guest-reset re-exec must not inherit backend fds.
                    let write_fd =
                        unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                    // TX: write directly from vCPU using non-blocking write.
                    let tx_err_count = std::sync::atomic::AtomicU32::new(0);
                    uart.set_tx_sink(Box::new(move |b: u8| {
                        let ret = unsafe {
                            libc::write(
                                write_fd,
                                &b as *const u8 as *const _,
                                1,
                            )
                        };
                        if ret < 0 {
                            let n = tx_err_count.fetch_add(
                                1,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            if n < 3 {
                                let err = std::io::Error::last_os_error();
                                eprintln!(
                                    "[serial-tx] write error: {} (fd={})",
                                    err, write_fd
                                );
                            }
                        }
                    }));

                    // RX: dup the fd for reading. We use poll() with a
                    // blocking fd — do NOT set O_NONBLOCK because dup'd
                    // fds share the file status flags and nonblock would
                    // affect the write path too (causing dropped console data).
                    let uart_input = uart.clone();
                    // Guest-reset re-exec must not inherit backend fds.
                    let read_fd_dup =
                        unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                    thread::Builder::new()
                        .name(format!("{name}-input"))
                        .spawn(move || {
                            let mut buf = [0u8; 64];
                            loop {
                                // Poll for readable data
                                let mut pfd = libc::pollfd {
                                    fd: read_fd_dup,
                                    events: libc::POLLIN,
                                    revents: 0,
                                };
                                let ret = unsafe { libc::poll(&mut pfd, 1, 1000) };
                                if ret < 0 {
                                    let err = std::io::Error::last_os_error();
                                    if err.kind() == std::io::ErrorKind::Interrupted { continue; }
                                    break;
                                }
                                if ret == 0 { continue; } // timeout
                                if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 { break; }
                                if pfd.revents & libc::POLLIN != 0 {
                                    let n = unsafe {
                                        libc::read(read_fd_dup, buf.as_mut_ptr() as *mut _, buf.len())
                                    };
                                    if n <= 0 {
                                        if n < 0 {
                                            let err = std::io::Error::last_os_error();
                                            if err.kind() == std::io::ErrorKind::WouldBlock { continue; }
                                        }
                                        break;
                                    }
                                    for byte in buf.iter().take(n as usize) {
                                        uart_input.input_byte(*byte);
                                    }
                                }
                            }
                        })
                        .expect("failed to spawn serial input thread");
                    info!(log, "{name} connected to {path}");
                }
            } else {
                warn!(log, "failed to open {name} device";
                    "path" => path,
                    "error" => std::io::Error::last_os_error(),
                );
            }
        }
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

    // Check for --version before full CLI parse so it works
    // even when required args are missing.
    if args.iter().any(|arg| {
        arg.as_os_str() == std::ffi::OsStr::new("--version")
            || arg.as_os_str() == std::ffi::OsStr::new("-V")
    }) {
        println!("vmm {}", version_string());
        return ExitCode::SUCCESS;
    }

    // Register USDT probes before any device setup.
    // Non-fatal: /dev/dtrace/helper may not exist inside zones.
    if let Err(e) = usdt::register_probes() {
        eprintln!("[vmm] USDT probe registration skipped: {e}");
    }

    let cli = Cli::parse();
    let log = setup_logger();

    match run(cli, &log) {
        Ok(RunOutcome::Reboot) => {
            reexec_for_reboot(&exe, &args, &SIGTERM_RECEIVED, &log)
        }
        // Preserve vmadm/SIGTERM success semantics instead of adopting
        // bhyve's distinct clean-shutdown exit codes.
        Ok(RunOutcome::PowerOff | RunOutcome::Halt) => ExitCode::SUCCESS,
        Ok(RunOutcome::TripleFault(src)) => {
            error!(log, "VM triple fault"; "source_vcpu" => src);
            ExitCode::from(3)
        }
        // Keep a guest-caused fatal exit distinct from an internal VMM error.
        Ok(RunOutcome::GuestFault) => ExitCode::from(2),
        Err(e) => {
            slog::error!(log, "fatal error"; "error" => format!("{:#}", e));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ahci_hd_is_rejected() {
        let error = reject_ahci_hd::<()>().unwrap_err();
        let message = error.to_string();

        assert!(message.contains("not implemented"));
        assert!(!message.contains("use virtio-blk instead"));
    }

    #[test]
    fn ahci_cd_requires_path() {
        let error = parse_ahci_cd_path("").unwrap_err();

        assert_eq!(error.to_string(), "ahci-cd requires an ISO path");
    }

    #[test]
    fn ahci_cd_rejects_options() {
        let error = parse_ahci_cd_path("/x.iso,rw").unwrap_err();

        assert_eq!(error.to_string(), "ahci-cd takes no options, got 'rw'");
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
