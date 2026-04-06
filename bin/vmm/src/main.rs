// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vmm - New Rust-based bhyve VMM with live migration support.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use anyhow::Context;
use clap::Parser;
use slog::{info, warn, error, o, Drain, Logger};

use vmm_config::Cli;
use vmm_core::hdl::CreateOpts;
use vmm_core::intr_pins::{LegacyPIC, NoOpPin};
use vmm_core::machine::{Builder, Machine, MachineSetup};
use vmm_devices::bhyve::{
    BhyveAtPic, BhyveAtPit, BhyveHpet, BhyveIoApic, BhyveRtc,
    pmtimer::{BhyvePmTimer, PMBASE_DEFAULT},
};
use vmm_devices::acpi;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::fwcfg::FwCfg;
use vmm_devices::smbios;
use vmm_devices::pci::Bdf;
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::uart::lpc::{self, LpcUart};
use vmm_devices::virtio::block::{VirtioBlock, VirtioBlockOpts};
use vmm_devices::virtio::pci::VirtioPciDevice;
use vmm_devices::virtio::viona::VirtioViona;

mod bootrom;
mod control;
mod direct_boot;
mod mdata;
mod vcpu_tasks;

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
fn spawn_serial_drain(
    uart: Arc<LpcUart>,
    log: Logger,
    output_path: Option<&std::path::Path>,
) -> thread::JoinHandle<()> {
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
}

/// Build the VM, load bootrom/kernel images, finalize memory, and apply
/// CPU topology and baseline masking.
fn create_machine(cli: &Cli, log: &Logger) -> anyhow::Result<MachineCreation> {
    let num_cpus = cli.num_cpus()?;
    let mem_size = cli.mem_size()?;

    info!(log, "vmm starting";
        "version" => version_string(),
    );
    info!(log, "creating VM";
        "name" => &cli.vm_name,
        "cpus" => num_cpus,
        "memory_mb" => mem_size / (1024 * 1024),
        "track_dirty" => cli.track_dirty,
    );

    let opts = CreateOpts {
        force: true,
        use_reservoir: cli.wire_memory,
        track_dirty: cli.track_dirty,
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

    if !direct_boot_mode && cli.migrate_listen.is_none() {
        load_bootrom(&mut setup, &cli.lpc, log)?;
    }

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
    machine.hdl().set_topology(1, num_cpus as u16, 1, 0)
        .context("failed to set CPU topology")?;

    // Apply CPU feature masking for migration compatibility
    let cpu_baseline = vmm_core::cpuid::CpuBaseline::from_str(&cli.cpu_baseline)
        .ok_or_else(|| anyhow::anyhow!(
            "unknown CPU baseline '{}' (valid: host, avx2, no-avx512, sse42)",
            cli.cpu_baseline,
        ))?;
    if cpu_baseline != vmm_core::cpuid::CpuBaseline::Host {
        info!(log, "applying CPU baseline"; "baseline" => &cli.cpu_baseline);
    }
    vmm_core::cpuid::apply_cpuid_baseline(
        machine.hdl(),
        num_cpus,
        cpu_baseline,
        log,
    ).context("failed to apply CPU baseline")?;

    Ok(MachineCreation {
        machine,
        api_version,
        num_cpus,
        mem_size,
        kernel_image,
        initrd_image,
    })
}

/// Load the bootrom image into guest memory (pre-finalize).
fn load_bootrom(
    setup: &mut MachineSetup,
    lpc_args: &[String],
    log: &Logger,
) -> anyhow::Result<()> {
    let rom_path = bootrom::find_bootrom_path(lpc_args);
    info!(log, "loading bootrom"; "path" => rom_path.display().to_string());
    let rom = bootrom::BootromFile::open(&rom_path)?;
    info!(log, "bootrom validated";
        "size" => rom.size(),
        "path" => rom.path().display().to_string(),
    );
    let hdl = setup.hdl().clone();
    bootrom::load_bootrom(setup.map_mut(), &hdl, &rom)?;
    Ok(())
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
fn init_chipset(machine: &Machine, log: &Logger) -> anyhow::Result<ChipsetDevices> {
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
    machine.hdl().rtc_settime(now)
        .context("failed to set RTC time")?;
    info!(log, "RTC NVRAM configured";
        "lowmem_mb" => lowmem / (1024 * 1024),
        "highmem_mb" => highmem / (1024 * 1024),
    );

    Ok(ChipsetDevices { chipset, pic, atpic, atpit, hpet, ioapic, rtc })
}

/// Timer and power-management device handles kept alive for the VM's
/// lifetime.
#[allow(dead_code)]
struct TimerAndPmDevices {
    pmtimer: Arc<BhyvePmTimer>,
    acpi_pm: Arc<vmm_devices::acpi_pm::AcpiPm>,
    pvpanic: Arc<vmm_devices::pvpanic::PvPanic>,
}

/// Configure the ACPI PM timer, ISA IRQ trigger modes for SCI and PCI,
/// ACPI PM1 registers, and the pvpanic PIO device.
fn setup_timers_and_pm(
    machine: &Machine,
    log: &Logger,
) -> anyhow::Result<TimerAndPmDevices> {
    // Configure ACPI PM timer in kernel
    let pmtimer = BhyvePmTimer::create(machine.hdl().clone(), PMBASE_DEFAULT);
    pmtimer.attach()
        .context("failed to configure PM timer")?;

    // Set SCI IRQ (9) to level-triggered mode in the kernel ATPIC.
    machine.hdl().isa_set_irq_trigger(9, true)
        .context("failed to set SCI IRQ trigger mode")?;

    // Set PCI IRQs 10 and 11 to level-triggered (firmware assigns these
    // via PIR registers to PIRQ links A-D).
    machine.hdl().isa_set_irq_trigger(10, true)
        .context("failed to set IRQ 10 trigger mode")?;
    machine.hdl().isa_set_irq_trigger(11, true)
        .context("failed to set IRQ 11 trigger mode")?;
    info!(log, "PM timer attached"; "port" => format!("{:#x}", pmtimer.port()));

    // Create ACPI PM1 event/enable/control registers + reset handler
    let acpi_pm = vmm_devices::acpi_pm::AcpiPm::create_and_attach(
        machine.bus_pio(),
        log.new(slog::o!("component" => "acpi-pm")),
    );
    info!(log, "ACPI PM registers attached"; "pmbase" => format!("{:#x}", 0x400));

    // pvpanic device — guest writes to port 0x505 on kernel panic
    let pvpanic = vmm_devices::pvpanic::PvPanic::new(log.clone());
    {
        let pvpanic = Arc::clone(&pvpanic);
        let handler: Arc<vmm_core::pio::PioFn> = Arc::new(move |_offset, rwo| {
            match rwo {
                vmm_core::common::RWOp::Read(ro) => {
                    ro.write_u8(pvpanic.pio_read());
                }
                vmm_core::common::RWOp::Write(wo) => {
                    pvpanic.pio_write(wo.read_u8());
                }
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

    Ok(TimerAndPmDevices { pmtimer, acpi_pm, pvpanic })
}

/// Create and attach COM1/COM2 UARTs, set up serial I/O paths, and
/// optionally start the mdata agent on COM2.
fn setup_serial(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    cli: &Cli,
    log: &Logger,
) -> (Arc<LpcUart>, Arc<LpcUart>) {
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
    setup_serial_io(&com1, com1_path.as_deref(), "com1", log);

    // Set up COM2:
    // - "socket,/path" → Unix socket for host-side metadata agent
    // - absent/no --mdata-nics → built-in mdata agent
    // - other path → raw file I/O
    if let Some(ref path) = com2_path {
        if path.starts_with("socket,") {
            // Production zone: create Unix socket so the host-side
            // metadata agent (vmadmd) can connect and serve sdc:nics,
            // SSH keys, etc. to the guest via COM2.
            setup_serial_io(&com2, Some(path.as_str()), "com2", log);
        } else {
            setup_serial_io(&com2, Some(path.as_str()), "com2", log);
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

    (com1, com2)
}

/// Set up fw_cfg device with E820 memory map, ACPI tables (both
/// table-loader protocol and legacy GPA), SMBIOS tables, and boot order.
/// Returns the FwCfg handle (must be kept alive for the VM's lifetime).
fn setup_fwcfg_and_acpi(
    machine: &Machine,
    cli: &Cli,
    num_cpus: u32,
    mem_size: usize,
    log: &Logger,
) -> Arc<FwCfg> {
    // Create fw_cfg device and attach to PIO bus
    let fwcfg = FwCfg::new();
    fwcfg.attach(machine.bus_pio());
    info!(log, "fw_cfg attached"; "selector_port" => "0x510", "data_port" => "0x511");

    // Add CPU count to fw_cfg (OVMF uses these for SMP initialization)
    let nb_cpus = (num_cpus as u16).to_le_bytes().to_vec();
    fwcfg.insert_legacy(0x05, nb_cpus.clone()); // NB_CPUS
    fwcfg.insert_legacy(0x0F, nb_cpus);         // MAX_CPUS

    // Generate E820 memory map and add to fw_cfg
    let e820_data = acpi::build_e820(machine.mem_size());
    fwcfg.insert_named("etc/e820", e820_data);
    info!(log, "E820 memory map added to fw_cfg";
        "mem_size" => machine.mem_size(),
    );

    // Generate ACPI tables with table-loader protocol support.
    // The layout produces two separate blobs (tables + rsdp) with
    // blob-relative offsets that the UEFI firmware patches via the
    // etc/table-loader commands.
    let acpi_layout = acpi::generate_acpi_layout(num_cpus);
    info!(log, "ACPI layout generated for table-loader protocol";
        "tables_size" => acpi_layout.tables.len(),
        "rsdp_size" => acpi_layout.rsdp.len(),
        "num_cpus" => num_cpus,
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
    fwcfg.insert_named("bootorder", b"/pci@i0cf8/pci@4,0\n".to_vec());
    info!(log, "boot order configured via fw_cfg");

    // Write legacy GPA-based ACPI tables to guest memory
    write_legacy_acpi_tables(machine, num_cpus, log);

    // Generate and write SMBIOS tables
    write_smbios_tables(machine, &fwcfg, cli, num_cpus, mem_size, log);

    fwcfg
}

/// Generate legacy GPA-based ACPI tables and write to multiple guest
/// memory locations for compatibility with both UEFI and BIOS scan paths.
fn write_legacy_acpi_tables(machine: &Machine, num_cpus: u32, log: &Logger) {
    let acpi_tables = acpi::generate_acpi_tables(num_cpus);

    // Write to BIOS-scannable region (0xE0000-0xFFFFF) as fallback
    // if the table-loader protocol doesn't properly install the RSDP.
    if let Err(e) = machine.memctx().write(0xF2400, &acpi_tables.data) {
        warn!(log, "failed to write legacy ACPI tables"; "error" => %e);
    } else {
        info!(log, "legacy ACPI tables at 0xF2400"; "size" => acpi_tables.data.len());
    }
    // Write RSDP at 0xE0000 for easier scan discovery
    let rsdp_size = 36;
    if acpi_tables.data.len() >= rsdp_size {
        if let Err(e) = machine.memctx().write(0xE0000, &acpi_tables.data[..rsdp_size]) {
            warn!(log, "failed to write RSDP at 0xE0000"; "error" => %e);
        } else {
            info!(log, "RSDP written at 0xE0000");
        }
    }

    // Write full tables to bhyve legacy base for non-UEFI/direct-mapped scenarios.
    if let Err(e) = machine.memctx().write(acpi::BHYVE_ACPI_BASE, &acpi_tables.data) {
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
    let (smbios_anchor, smbios_tables) = smbios::generate_smbios(&smbios_config);
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
        if let Err(e) = machine.memctx().write(SMBIOS_TABLE_GPA, &smbios_tables) {
            warn!(log, "failed to write SMBIOS tables to guest memory"; "error" => %e);
        }

        // Patch the entry point with the actual table address
        let mut ep = smbios_anchor;
        if ep.len() >= 0x1F {
            // 0x18: Structure table address (4 bytes, little-endian)
            ep[0x18..0x1C].copy_from_slice(&(SMBIOS_TABLE_GPA as u32).to_le_bytes());

            // Recompute intermediate checksum (bytes 0x10..0x1F)
            ep[0x15] = 0;
            let sum: u8 = ep[0x10..0x1F].iter().fold(0u8, |a, &b| a.wrapping_add(b));
            ep[0x15] = 0u8.wrapping_sub(sum);

            // Recompute entry point checksum (bytes 0x00..0x10)
            ep[0x04] = 0;
            let sum: u8 = ep[0x00..0x10].iter().fold(0u8, |a, &b| a.wrapping_add(b));
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
/// for pause/resume orchestration.
fn init_pci_devices(
    machine: &Machine,
    chipset: &I440FxChipset,
    cli: &Cli,
    log: &Logger,
) -> anyhow::Result<(
    Vec<Arc<dyn vmm_devices::pci::PciDevice>>,
    Vec<Arc<dyn vmm_devices::Lifecycle>>,
)> {
    let mut pci_devices: Vec<Arc<dyn vmm_devices::pci::PciDevice>> = Vec::new();
    let mut lifecycle_devices: Vec<Arc<dyn vmm_devices::Lifecycle>> = Vec::new();

    for slot_spec in &cli.pci_slot {
        match parse_and_create_pci_device(
            slot_spec,
            chipset,
            machine.bus_pio(),
            machine.bus_mmio(),
            machine.physmap(),
            machine.hdl(),
            log,
        ) {
            Ok((Some(pci), lifecycle)) => {
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

    Ok((pci_devices, lifecycle_devices))
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
        let cmdline = cli.cmdline.as_deref()
            .unwrap_or("console=ttyS0 earlyprintk=serial");
        let acpi_rsdp_addr = acpi::BHYVE_ACPI_BASE as u64;
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
    log: &Logger,
) -> anyhow::Result<()> {
    info!(log, "migration destination mode"; "listen" => listen_addr);

    // Activate and reset vCPUs. The reset initializes the VMCS/VMCB
    // host-state fields. The migration import then overwrites the
    // guest-state fields with the source's values.
    for vcpu in machine.vcpus() {
        vcpu.activate()
            .with_context(|| format!("failed to activate vCPU {}", vcpu.id()))?;
        vcpu.reboot_state()
            .with_context(|| format!("failed to reset vCPU {}", vcpu.id()))?;
    }

    // Listen for migration connection
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;

    let listen_addr_owned = listen_addr.to_string();
    let hdl = machine.hdl().clone();
    let physmap = machine.physmap().clone();
    let migrate_mem_size = mem_size as u64;
    let status = Arc::new(std::sync::Mutex::new(
        vmm_migrate::MigrationStatus::default(),
    ));

    info!(log, "waiting for migration connection"; "addr" => listen_addr);

    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(&listen_addr_owned)
            .await
            .context("failed to bind migration listener")?;

        let (stream, peer) = listener.accept()
            .await
            .context("failed to accept migration connection")?;

        info!(log, "migration connection from"; "peer" => %peer);

        vmm_migrate::destination::run_destination(
            stream,
            hdl.clone(),
            &vmm_core::mem::MemCtx::new(physmap),
            num_cpus,
            migrate_mem_size,
            status,
            log,
        )
        .await
        .context("migration import failed")
    })?;

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
        vcpu.activate()
            .with_context(|| format!("failed to activate vCPU {}", vcpu.id()))?;

        if cli.vmexit_on_hlt {
            machine.hdl().set_capability(
                vcpu.id(),
                bhyve_api::vm_cap_type::VM_CAP_HALT_EXIT as i32,
                1,
            ).with_context(|| format!("failed to set HALT_EXIT on vCPU {}", vcpu.id()))?;
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
            bsp.setup_bsp()
                .context("failed to set up BSP")?;
        }
    }

    Ok(())
}

/// Spawn per-vCPU threads and return the event channel and join handles.
fn spawn_vcpus(
    machine: &Machine,
    num_cpus: u32,
    api_version: u32,
    log: &Logger,
) -> (
    mpsc::Receiver<vcpu_tasks::VcpuEvent>,
    Vec<JoinHandle<()>>,
    Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
) {
    info!(log, "starting vCPU threads");

    // Create per-vCPU metrics
    let vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>> = (0..num_cpus)
        .map(|_| Arc::new(vmm_devices::metrics::VcpuMetrics::new()))
        .collect();

    // Spawn vCPU threads and run until suspend/error
    let (event_rx, handles) = vcpu_tasks::spawn_vcpu_threads(
        machine.vcpus(),
        machine.bus_pio(),
        machine.bus_mmio(),
        machine.physmap(),
        api_version,
        &vcpu_metrics,
        log,
    );

    (event_rx, handles, vcpu_metrics)
}

/// After migration, inject a timer interrupt to restart the scheduler tick
/// in the guest (nohz/tickless mode workaround).
fn post_migration_timer_kick(machine: &Machine, log: &Logger) {
    std::thread::sleep(std::time::Duration::from_millis(200));
    for vcpu in machine.vcpus() {
        match machine.hdl().lapic_irq(vcpu.id(), 0xEC) {
            Ok(()) => info!(log, "post-migration timer kick"; "vcpu" => vcpu.id()),
            Err(e) => warn!(log, "timer kick failed"; "vcpu" => vcpu.id(), "error" => %e),
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
    log: &Logger,
) -> Option<PathBuf> {
    let sock_path = cli.control_socket.as_ref()?;
    let ctrl = control::VmController::new(
        machine.hdl().clone(),
        machine.physmap().clone(),
        cli.vm_name.clone(),
        num_cpus,
        mem_size,
        vcpu_metrics.to_vec(),
        devices,
        log.clone(),
    );
    match control::spawn_control_thread(sock_path, ctrl, log.clone()) {
        Ok((_handle, path)) => {
            info!(log, "control socket listening"; "path" => path.display().to_string());
            Some(path)
        }
        Err(e) => {
            error!(log, "failed to start control socket"; "error" => %e);
            None
        }
    }
}

/// Install the SIGTERM signal handler and spawn a poller thread that
/// triggers ACPI poweroff when the signal is received.
fn install_sigterm_handler(machine: &Machine, log: &Logger) {
    let log_sig = log.clone();
    // libc::signal returns SIG_ERR on failure; safe to ignore
    let _ = unsafe {
        libc::signal(libc::SIGTERM, sigterm_handler as libc::sighandler_t)
    };
    // Spawn a thread to wait for the signal flag
    let hdl_sig = machine.hdl().clone();
    thread::Builder::new()
        .name("sigterm-handler".into())
        .spawn(move || {
            loop {
                thread::sleep(std::time::Duration::from_millis(500));
                if SIGTERM_RECEIVED.load(std::sync::atomic::Ordering::Relaxed) {
                    info!(log_sig, "SIGTERM received, triggering ACPI poweroff");
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
    log: &Logger,
) {
    // Wait for the first vCPU event
    match event_rx.recv() {
        Ok(vcpu_tasks::VcpuEvent::Suspended { vcpu_id, kind }) => {
            info!(log, "VM suspended";
                "vcpu" => vcpu_id,
                "kind" => ?kind,
            );
        }
        Ok(vcpu_tasks::VcpuEvent::Error { vcpu_id, error: err }) => {
            error!(log, "vCPU error";
                "vcpu" => vcpu_id,
                "error" => &err,
            );
        }
        Err(_) => {
            warn!(log, "all vCPU threads exited without events");
        }
    }

    // Wait for all vCPU threads to finish
    for handle in handles {
        // thread panic is non-recoverable at this point
        let _ = handle.join();
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
}

fn run(cli: Cli, log: &Logger) -> anyhow::Result<()> {
    // Phase 1: Create VM, load bootrom/kernel, finalize memory
    let creation = create_machine(&cli, log)?;
    let MachineCreation {
        machine,
        api_version,
        num_cpus,
        mem_size,
        kernel_image,
        initrd_image,
    } = creation;

    // Phase 2: Initialize chipset and kernel-emulated devices
    let chipset_devs = init_chipset(&machine, log)?;

    // Phase 3: Timers, PM, pvpanic
    let _timer_pm_devs = setup_timers_and_pm(&machine, log)?;

    // Phase 4: Serial ports and mdata agent
    let (_com1, _com2) = setup_serial(&machine, &chipset_devs.pic, &cli, log);

    // Phase 5: fw_cfg, ACPI tables, SMBIOS, E820, boot order
    let _fwcfg = setup_fwcfg_and_acpi(&machine, &cli, num_cpus, mem_size, log);

    // Phase 6: PCI devices
    let (_pci_devices, lifecycle_devices) = init_pci_devices(&machine, &chipset_devs.chipset, &cli, log)?;

    // Phase 7: Direct boot kernel loading (after ACPI tables are ready)
    load_direct_boot_kernel(&machine, &cli, &kernel_image, &initrd_image, mem_size, log)?;

    // Phase 8: vCPU activation (migration or normal)
    if let Some(ref listen_addr) = cli.migrate_listen {
        run_migration_destination(&machine, listen_addr, num_cpus, mem_size, log)?;
    } else {
        activate_vcpus(&machine, &cli, &kernel_image, log)?;
    }

    // Phase 9: Spawn vCPU threads
    let (event_rx, handles, vcpu_metrics) = spawn_vcpus(
        &machine, num_cpus, api_version, log,
    );

    // Phase 10: Post-migration timer kick
    if cli.migrate_listen.is_some() {
        post_migration_timer_kick(&machine, log);
    }

    // Phase 11: Control socket
    let control_cleanup = spawn_control_socket(
        &machine, &cli, num_cpus, mem_size, &vcpu_metrics, lifecycle_devices, log,
    );

    // Phase 12: Signal handling
    install_sigterm_handler(&machine, log);

    // Phase 13: Main event loop and cleanup
    run_event_loop(&machine, event_rx, handles, control_cleanup, log);

    Ok(())
}

/// Create a virtio-blk PCI device from a config string.
///
/// Shared between the `virtio-blk` and `ahci-cd`/`ahci-hd` (fallback) paths.
fn create_virtio_blk_device(
    config: &str,
    bdf: Bdf,
    read_only_override: bool,
    chipset: &I440FxChipset,
    physmap: &Arc<vmm_core::mem::PhysMap>,
    bus_pio: &Arc<vmm_core::pio::PioBus>,
    bus_mmio: &Arc<vmm_core::mmio::MmioBus>,
    vmm_hdl: &Arc<vmm_core::hdl::VmmHdl>,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioBlock>>> {
    let (path, mut opts) = parse_blk_config(config);
    if read_only_override {
        opts.read_only = true;
    }

    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .read(true)
        .write(!opts.read_only)
        .open(path)
        .with_context(|| format!("failed to open disk: {}", path))?;
    let num_queues: u16 = 1;
    let blk = VirtioBlock::new(file, &opts, Arc::clone(physmap), num_queues)
        .with_context(|| format!("failed to create virtio-blk for: {}", path))?;

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
        128,  // queue size
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

/// Parse a `-s` PCI slot specification and create the device.
///
/// Format: `[bus:]dev[:func],driver[,config...]`
/// Examples:
///   `4,virtio-blk,/dev/zvol/rdsk/zones/uuid/disk0`
///   `0:5:0,virtio-blk,/path/to/disk.raw`
fn parse_and_create_pci_device(
    spec: &str,
    chipset: &I440FxChipset,
    bus_pio: &Arc<vmm_core::pio::PioBus>,
    bus_mmio: &Arc<vmm_core::mmio::MmioBus>,
    physmap: &Arc<vmm_core::mem::PhysMap>,
    vmm_hdl: &Arc<vmm_core::hdl::VmmHdl>,
    log: &Logger,
) -> anyhow::Result<(Option<Arc<dyn vmm_devices::pci::PciDevice>>, Option<Arc<dyn vmm_devices::Lifecycle>>)> {
    let parts: Vec<&str> = spec.splitn(3, ',').collect();
    if parts.len() < 2 {
        anyhow::bail!("invalid PCI slot spec '{}': need slot,driver[,config]", spec);
    }

    let bdf = parse_bdf(parts[0])
        .ok_or_else(|| anyhow::anyhow!("invalid BDF '{}' in spec '{}'", parts[0], spec))?;
    let driver = parts[1];
    let config = parts.get(2).copied().unwrap_or("");

    match driver {
        "virtio-blk" | "virtio-blk-pci" => {
            if config.is_empty() {
                anyhow::bail!("virtio-blk requires a disk path");
            }
            let pci_dev = create_virtio_blk_device(
                config, bdf, false, chipset, physmap, bus_pio, bus_mmio, vmm_hdl,
            )?;
            info!(log, "virtio-blk attached";
                "bdf" => %bdf,
                "path" => config,
                "msix" => "2 vectors",
            );
            Ok((Some(pci_dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>), Some(pci_dev as Arc<dyn vmm_devices::Lifecycle>)))
        }
        "virtio-net-viona" => {
            if config.is_empty() {
                anyhow::bail!("virtio-net-viona requires a VNIC name");
            }
            // Parse config: vnic_name[,promiscphys=true]
            let (vnic_name, promiscphys) = if let Some(comma) = config.find(',') {
                let name = &config[..comma];
                let promisc = config[comma + 1..].split(',')
                    .any(|opt| opt == "promiscphys" || opt == "promiscphys=true");
                (name, promisc)
            } else {
                (config, false)
            };

            use std::os::fd::AsRawFd;
            let viona = VirtioViona::new(vnic_name, vmm_hdl.as_raw_fd())
                .with_context(|| {
                    format!("failed to create viona device for VNIC: {}", vnic_name)
                })?;

            if promiscphys {
                viona.set_promisc(true)
                    .with_context(|| format!("failed to set promiscphys on {}", vnic_name))?;
            }

            // MSI-X: one vector per queue (rx + tx) + config change
            let num_viona_queues = vmm_devices::virtio::viona::NET_NUM_QUEUES as u16;
            let msix = Arc::new(MsixTable::new(
                num_viona_queues + 1,
                Arc::clone(vmm_hdl),
            ));

            // Start interrupt polling BEFORE moving viona into PCI device.
            // The polling thread needs the viona fd, and we pass a per-queue
            // callback that routes via the correct MSI-X vector.
            let intr_hook: Arc<std::sync::Mutex<Option<std::sync::Weak<VirtioPciDevice<VirtioViona>>>>> =
                Arc::new(std::sync::Mutex::new(None));
            let hook_clone = Arc::clone(&intr_hook);
            let raise_fn: Arc<dyn Fn(u16) + Send + Sync> = Arc::new(move |queue_idx: u16| {
                if let Some(weak) = hook_clone.lock().ok().and_then(|g| g.clone()) {
                    if let Some(dev) = weak.upgrade() {
                        dev.raise_queue_interrupt_for(queue_idx);
                    }
                }
            });
            viona.start_intr_poll(
                raise_fn,
                log.new(slog::o!("component" => "viona-intr")),
            );

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
            Ok((Some(pci_dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>), Some(pci_dev as Arc<dyn vmm_devices::Lifecycle>)))
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
            Ok((Some(nvme.clone() as Arc<dyn vmm_devices::pci::PciDevice>), Some(nvme as Arc<dyn vmm_devices::Lifecycle>)))
        }
        "virtio-rnd" => {
            let rng = vmm_devices::virtio::VirtioRng::new();
            let lintr = chipset.route_lintr(&bdf);
            // MSI-X: 1 queue + config change = 2 vectors
            let msix = Arc::new(MsixTable::new(2, Arc::clone(vmm_hdl)));
            let pci_dev = VirtioPciDevice::new(
                rng,
                vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_RNG,
                1,    // 1 queue
                64,   // queue size
                0,    // no device-specific config
                lintr,
                Arc::clone(physmap),
                Arc::clone(bus_pio),
                Arc::clone(bus_mmio),
                Some(msix),
            );
            chipset.attach_device(bdf, pci_dev.clone());
            info!(log, "virtio-rnd attached"; "bdf" => %bdf, "msix" => "2 vectors");
            Ok((Some(pci_dev.clone() as Arc<dyn vmm_devices::pci::PciDevice>), Some(pci_dev as Arc<dyn vmm_devices::Lifecycle>)))
        }
        "fbuf" => {
            // Parse config: vga=off,unix=/path[,password=pwd][,w=N][,h=N]
            let mut vnc_path = std::path::PathBuf::from("/tmp/vm.vnc");
            let mut vnc_password = None;
            let mut width: u16 = 1024;
            let mut height: u16 = 768;
            for opt in config.split(',') {
                if let Some(p) = opt.strip_prefix("unix=") {
                    vnc_path = std::path::PathBuf::from(p);
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
                log.new(slog::o!("component" => "fbuf")),
            );
            chipset.attach_device(bdf, fbuf.clone());
            info!(log, "fbuf attached";
                "bdf" => %bdf,
                "vnc" => vnc_path.display().to_string(),
                "resolution" => format!("{}x{}", width, height),
            );
            Ok((Some(fbuf.clone() as Arc<dyn vmm_devices::pci::PciDevice>), Some(fbuf as Arc<dyn vmm_devices::Lifecycle>)))
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
            info!(log, "xhci tablet attached"; "bdf" => %bdf);
            Ok((Some(xhci.clone() as Arc<dyn vmm_devices::pci::PciDevice>), Some(xhci as Arc<dyn vmm_devices::Lifecycle>)))
        }
        "passthru" => {
            if config.is_empty() {
                anyhow::bail!("passthru requires a device path (e.g., /dev/ppt0)");
            }
            // Config is just the path to /dev/pptN
            let path = config.split(',').next().unwrap_or(config);
            let dev = vmm_devices::passthru::PciPassthru::new(
                path,
                Arc::clone(vmm_hdl),
            ).with_context(|| format!("failed to init passthru device: {}", path))?;
            chipset.attach_device(bdf, dev.clone());
            info!(log, "passthru attached";
                "bdf" => %bdf,
                "path" => path,
            );
            Ok((Some(dev as Arc<dyn vmm_devices::pci::PciDevice>), None))
        }
        "ahci-cd" | "ahci-hd" => {
            anyhow::bail!(
                "AHCI device type '{}' is not yet implemented; use virtio-blk instead",
                driver,
            );
        }
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
                        if sz.is_power_of_two() && sz >= 512 && sz <= 65536 {
                            opts.sector_size = sz;
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
) {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};

    // Handle "socket,/path" — create a Unix domain socket listener
    // that the host-side metadata agent connects to.
    if let Some(path) = config.and_then(|c| c.strip_prefix("socket,")) {
        setup_serial_socket(uart, path, name, log);
        return;
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
            let name_owned = name.to_string();
            thread::Builder::new()
                .name(format!("{name}-input"))
                .spawn(move || {
                    let stdin = std::io::stdin();
                    let mut buf = [0u8; 1];
                    loop {
                        match stdin.lock().read(&mut buf) {
                            Ok(0) => break,
                            Ok(_) => { uart_input.input_byte(buf[0]); }
                            Err(_) => break,
                        }
                    }
                })
                .expect("failed to spawn serial input thread");
            info!(log, "{name} connected to stdio");
        }
        Some(path) => {
            // Open as a device/file for both read and write
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
            {
                Ok(file) => {
                    // TX: write to device
                    let fd = file.as_raw_fd();
                    // Dup the fd so we can have separate read/write handles
                    let write_fd = unsafe { libc::dup(fd) };
                    let write_file = std::sync::Mutex::new(unsafe {
                        std::fs::File::from_raw_fd(write_fd)
                    });
                    uart.set_tx_sink(Box::new(move |b: u8| {
                        if let Ok(mut f) = write_file.lock() {
                            if let Err(e) = f.write_all(&[b]) {
                                eprintln!("[serial] device write error: {}", e);
                            }
                        }
                    }));

                    // RX: read from device in a thread
                    let uart_input = uart.clone();
                    let path_owned = path.to_string();
                    thread::Builder::new()
                        .name(format!("{name}-input"))
                        .spawn(move || {
                            let mut buf = [0u8; 1];
                            let mut reader = file;
                            loop {
                                match reader.read(&mut buf) {
                                    Ok(0) => break,
                                    Ok(_) => { uart_input.input_byte(buf[0]); }
                                    Err(_) => break,
                                }
                            }
                        })
                        .expect("failed to spawn serial input thread");
                    info!(log, "{name} connected to {path}");
                }
                Err(e) => {
                    warn!(log, "failed to open {name} device";
                        "path" => path,
                        "error" => %e,
                    );
                }
            }
        }
    }
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
) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    // Remove stale socket from a previous run
    let _ = std::fs::remove_file(path);

    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            warn!(log, "failed to bind {name} socket";
                "path" => path, "error" => %e);
            return;
        }
    };
    info!(log, "{name} socket listening"; "path" => path);

    // TX: buffer writes until a client connects. Use a shared
    // optional stream protected by a mutex.
    let tx_stream: Arc<std::sync::Mutex<Option<std::os::unix::net::UnixStream>>> =
        Arc::new(std::sync::Mutex::new(None));
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
                        let mut reader = stream;
                        let mut buf = [0u8; 64];
                        loop {
                            match reader.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    for &b in &buf[..n] {
                                        uart_rx.input_byte(b);
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
}

/// Version string including git commit hash (set by build.rs).
fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("VMM_GIT_COMMIT");
    format!("{version} ({commit})")
}

fn main() -> ExitCode {
    // Check for --version before full CLI parse so it works
    // even when required args are missing.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
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
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            slog::error!(log, "fatal error"; "error" => format!("{:#}", e));
            ExitCode::FAILURE
        }
    }
}
