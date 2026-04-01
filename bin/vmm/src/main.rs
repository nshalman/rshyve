// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vmm - New Rust-based bhyve VMM with live migration support.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use anyhow::Context;
use clap::Parser;
use slog::{info, warn, error, o, Drain, Logger};

use vmm_config::Cli;
use vmm_core::hdl::CreateOpts;
use vmm_core::intr_pins::{IoApicPin, LegacyPIC, NoOpPin};
use vmm_core::machine::Builder;
use vmm_devices::bhyve::{
    BhyveAtPic, BhyveAtPit, BhyveHpet, BhyveIoApic, BhyveRtc,
    pmtimer::{BhyvePmTimer, PMBASE_DEFAULT},
};
use vmm_devices::acpi;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::fwcfg::FwCfg;
use vmm_devices::smbios;
use vmm_devices::pci::{Bdf, BarN};
use vmm_devices::uart::lpc::{self, LpcUart};
use vmm_devices::virtio::block::VirtioBlock;
use vmm_devices::virtio::pci::VirtioPciDevice;
use vmm_devices::virtio::viona::VirtioViona;

mod bootrom;
mod vcpu_tasks;

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
    _log: Logger,
    output_path: Option<&std::path::Path>,
) -> thread::JoinHandle<()> {
    // Open serial output file if specified, otherwise use stdout
    let output: Box<dyn Write + Send> = if let Some(path) = output_path {
        match std::fs::File::create(path) {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::stdout()),
        }
    } else {
        Box::new(std::io::stdout())
    };
    let output = std::sync::Mutex::new(output);

    thread::Builder::new()
        .name("serial-drain".into())
        .spawn(move || {
            loop {
                match uart.output_byte() {
                    Some(b) => {
                        if let Ok(mut out) = output.lock() {
                            let _ = out.write_all(&[b]);
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

fn run(cli: Cli, log: &Logger) -> anyhow::Result<()> {
    let num_cpus = cli.num_cpus()?;
    let mem_size = cli.mem_size()?;

    info!(log, "creating VM";
        "name" => &cli.vm_name,
        "cpus" => num_cpus,
        "memory_mb" => mem_size / (1024 * 1024),
        "track_dirty" => cli.track_dirty,
    );

    let opts = CreateOpts {
        force: true,
        use_reservoir: false,
        track_dirty: cli.track_dirty,
    };

    // Phase 1: Build VM with RAM
    let mut setup = Builder::new(&cli.vm_name)
        .opts(opts)
        .cpus(num_cpus)
        .memory(mem_size)
        .build()
        .context("failed to create VM")?;

    // Phase 2: Load bootrom
    let rom_path = bootrom::find_bootrom_path(&cli.lpc);
    info!(log, "loading bootrom"; "path" => rom_path.display().to_string());

    let rom = bootrom::BootromFile::open(&rom_path)?;
    info!(log, "bootrom validated";
        "size" => rom.size(),
        "path" => rom.path().display().to_string(),
    );

    {
        let hdl = setup.hdl().clone();
        bootrom::load_bootrom(setup.map_mut(), &hdl, &rom)?;
    }
    drop(rom);

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
            warn!(log, "API version query failed, assuming v18";
                "error" => %e,
            );
            18
        }
    };

    // Set CPU topology (required for kernel LAPIC/timer setup)
    // maxcpus must be 0 (kernel ignores it and uses VM_MAXCPU)
    machine.hdl().set_topology(1, num_cpus as u16, 1, 0)
        .context("failed to set CPU topology")?;

    // ----- Device initialization -----

    // Create legacy PIC for ISA interrupt delivery
    let pic = LegacyPIC::new(machine.hdl().clone());

    // Create i440fx chipset (host bridge + ISA bridge + PCI config ports)
    let chipset = I440FxChipset::create(machine.bus_pio());
    info!(log, "chipset initialized"; "type" => "i440fx");

    // Create bhyve kernel-emulated devices.
    // These are emulated in-kernel; we just hold references for lifecycle.
    let _atpic = BhyveAtPic::create(machine.hdl().clone());
    let _atpit = BhyveAtPit::create(machine.hdl().clone());
    let _hpet = BhyveHpet::create(machine.hdl().clone());
    let _ioapic = BhyveIoApic::create(machine.hdl().clone());
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

    // Configure ACPI PM timer in kernel
    let pmtimer = BhyvePmTimer::create(machine.hdl().clone(), PMBASE_DEFAULT);
    pmtimer.attach()
        .context("failed to configure PM timer")?;

    // Set SCI IRQ (9) to level-triggered mode in the kernel ATPIC.
    // This is required for ACPI SCI delivery to work correctly.
    machine.hdl().isa_set_irq_trigger(9, true)
        .context("failed to set SCI IRQ trigger mode")?;
    info!(log, "PM timer attached"; "port" => format!("{:#x}", pmtimer.port()));

    // Create ACPI PM1 event/enable/control registers + reset handler
    let _acpi_pm = vmm_devices::acpi_pm::AcpiPm::create_and_attach(machine.bus_pio());
    info!(log, "ACPI PM registers attached"; "pmbase" => format!("{:#x}", 0xB000));

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

    // Spawn serial console drain (COM1 output → stderr)
    // Set up synchronous serial output: bytes go directly to stdout
    // in the PIO handler context, matching C bhyve's com1,stdio behavior.
    com1.set_tx_sink(Box::new(|b: u8| {
        use std::io::Write;
        let _ = std::io::stdout().write_all(&[b]);
        let _ = std::io::stdout().flush();
    }));

    // ----- fw_cfg, E820, and ACPI table setup -----

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
    // Try to load iasl-compiled DSDT override for testing
    let dsdt_override = std::fs::read("/opt/vmmnew/test_dsdt.aml").ok();
    if dsdt_override.is_some() {
        info!(log, "using iasl-compiled DSDT override";
            "size" => dsdt_override.as_ref().map(|d| d.len()).unwrap_or(0),
        );
    }
    let acpi_layout = acpi::generate_acpi_layout_with_dsdt(
        num_cpus,
        dsdt_override,
    );
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

    // Write RSDP to the BIOS-scannable region (0xE0000-0xFFFFF).
    // The Linux kernel's legacy ACPI scan looks here for the RSDP
    // signature "RSD PTR ". This serves as a fallback if the table-loader
    // protocol doesn't properly install the RSDP.
    {
        let legacy_tables = acpi::generate_acpi_tables(num_cpus);
        // Write RSDP at 0xF2400 (legacy bhyve location)
        if let Err(e) = machine.memctx().write(0xF2400, &legacy_tables.data) {
            warn!(log, "failed to write legacy ACPI tables"; "error" => %e);
        } else {
            info!(log, "legacy ACPI tables at 0xF2400"; "size" => legacy_tables.data.len());
        }
        // Also write RSDP at 0xE0000 for easier scan discovery
        let rsdp_start = 0; // RSDP is at the start of legacy_tables.data
        let rsdp_size = 36;
        if legacy_tables.data.len() >= rsdp_size {
            if let Err(e) = machine.memctx().write(0xE0000, &legacy_tables.data[..rsdp_size]) {
                warn!(log, "failed to write RSDP at 0xE0000"; "error" => %e);
            } else {
                info!(log, "RSDP written at 0xE0000");
            }
        }
    }

    // Also generate legacy GPA-based tables and write to guest memory
    // as a fallback for non-UEFI or direct-mapped scenarios.
    let acpi_tables = acpi::generate_acpi_tables(num_cpus);
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

    // Generate SMBIOS tables and add to fw_cfg
    let smbios_config = smbios::SmbiosConfig {
        vm_name: cli.vm_name.clone(),
        uuid: cli.uuid.clone(),
        num_cpus,
        memory_mb: (mem_size / (1024 * 1024)) as u64,
    };
    let (smbios_anchor, smbios_tables) = smbios::generate_smbios(&smbios_config);
    fwcfg.insert_named("etc/smbios/smbios-anchor", smbios_anchor);
    fwcfg.insert_named("etc/smbios/smbios-tables", smbios_tables.clone());
    info!(log, "SMBIOS tables added to fw_cfg";
        "anchor_size" => 31,
        "tables_size" => smbios_tables.len(),
    );

    // Keep fw_cfg alive for the duration of the VM
    let _fwcfg = fwcfg;

    // ----- PCI device creation from -s flags -----
    // Keep devices alive by collecting them
    let mut _pci_devices: Vec<Arc<dyn vmm_devices::pci::PciDevice>> = Vec::new();

    for slot_spec in &cli.pci_slot {
        match parse_and_create_pci_device(
            slot_spec,
            &chipset,
            machine.bus_pio(),
            machine.physmap(),
            machine.hdl(),
            &pic,
            log,
        ) {
            Ok(Some(dev)) => _pci_devices.push(dev),
            Ok(None) => {} // recognized but no Arc to hold
            Err(e) => {
                error!(log, "failed to create PCI device";
                    "spec" => slot_spec,
                    "error" => format!("{:#}", e),
                );
                return Err(e);
            }
        }
    }

    // Keep chipset alive for the duration
    let _chipset = chipset;

    // ----- vCPU startup -----

    // Activate and initialize all vCPUs
    for vcpu in machine.vcpus() {
        vcpu.activate()
            .with_context(|| format!("failed to activate vCPU {}", vcpu.id()))?;
        vcpu.reboot_state()
            .with_context(|| format!("failed to reset vCPU {}", vcpu.id()))?;
    }

    // Set up BSP (vCPU 0) for boot
    if let Some(bsp) = machine.vcpus().first() {
        bsp.setup_bsp()
            .context("failed to set up BSP")?;
    }

    info!(log, "starting vCPU threads");

    // Spawn vCPU threads and run until suspend/error
    let (event_rx, handles) = vcpu_tasks::spawn_vcpu_threads(
        machine.vcpus(),
        machine.bus_pio(),
        machine.bus_mmio(),
        machine.physmap(),
        api_version,
        log,
    );

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
        let _ = handle.join();
    }

    // Clean up
    if let Err(e) = machine.hdl().destroy() {
        warn!(log, "failed to destroy VM"; "error" => %e);
    }

    info!(log, "VM shutdown complete");
    Ok(())
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
    physmap: &Arc<vmm_core::mem::PhysMap>,
    vmm_hdl: &Arc<vmm_core::hdl::VmmHdl>,
    pic: &Arc<LegacyPIC>,
    log: &Logger,
) -> anyhow::Result<Option<Arc<dyn vmm_devices::pci::PciDevice>>> {
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
            use std::fs::OpenOptions;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(config)
                .with_context(|| format!("failed to open disk: {}", config))?;
            let blk = VirtioBlock::new(file, false)
                .with_context(|| format!("failed to create virtio-blk for: {}", config))?;

            // PCI interrupt routing: DSDT _PRT maps slot N, pin A to
            // GSI = 16 + ((slot - 1) % 8). Must match the APRT table in DSDT.
            // Use IOAPIC for GSI >= 16 (PCI interrupts).
            let gsi = 16 + ((bdf.dev() as u8) % 8);
            let irq_pin: Arc<dyn vmm_core::intr_pins::IntrPin> =
                IoApicPin::new(gsi, Arc::clone(vmm_hdl));
            let lintr = Some((
                vmm_devices::pci::INTxPinID::IntA,
                irq_pin,
            ));

            let pci_dev = VirtioPciDevice::new(
                blk,
                vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_BLOCK,
                1,    // 1 queue for block
                128,  // queue size
                8,    // device config size (capacity u64)
                lintr,
                Arc::clone(physmap),
                Arc::clone(bus_pio),
            );

            chipset.attach_device(bdf, pci_dev.clone());
            info!(log, "virtio-blk attached";
                "bdf" => %bdf,
                "path" => config,
                "gsi" => gsi,
            );
            Ok(Some(pci_dev))
        }
        "virtio-net-viona" => {
            if config.is_empty() {
                anyhow::bail!("virtio-net-viona requires a VNIC name");
            }
            use std::os::fd::AsRawFd;
            let viona = VirtioViona::new(config, vmm_hdl.as_raw_fd())
                .with_context(|| {
                    format!("failed to create viona device for VNIC: {}", config)
                })?;

            // PCI interrupt routing via IOAPIC (must match DSDT _PRT)
            let gsi = 16 + ((bdf.dev() as u8) % 8);
            let irq_pin: Arc<dyn vmm_core::intr_pins::IntrPin> =
                IoApicPin::new(gsi, Arc::clone(vmm_hdl));
            let lintr = Some((
                vmm_devices::pci::INTxPinID::IntA,
                irq_pin,
            ));

            let pci_dev = VirtioPciDevice::new(
                viona,
                vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_NET,
                vmm_devices::virtio::viona::NET_NUM_QUEUES,
                vmm_devices::virtio::viona::NET_QUEUE_SIZE,
                vmm_devices::virtio::viona::NET_CONFIG_SIZE,
                lintr,
                Arc::clone(physmap),
                Arc::clone(bus_pio),
            );

            chipset.attach_device(bdf, pci_dev.clone());
            info!(log, "virtio-net-viona attached";
                "bdf" => %bdf,
                "vnic" => config,
                "gsi" => gsi,
            );
            Ok(Some(pci_dev))
        }
        "hostbridge" => {
            // Already created by chipset, skip
            Ok(None)
        }
        "lpc" => {
            // Already created by chipset, skip
            Ok(None)
        }
        other => {
            warn!(log, "unknown PCI driver, skipping";
                "driver" => other,
                "bdf" => %bdf,
            );
            Ok(None)
        }
    }
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

fn main() -> ExitCode {
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
