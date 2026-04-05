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
use vmm_core::intr_pins::{LegacyPIC, NoOpPin};
use vmm_core::machine::Builder;
use vmm_devices::bhyve::{
    BhyveAtPic, BhyveAtPit, BhyveHpet, BhyveIoApic, BhyveRtc,
    pmtimer::{BhyvePmTimer, PMBASE_DEFAULT},
};
use vmm_devices::acpi;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::fwcfg::FwCfg;
use vmm_devices::smbios;
use vmm_devices::pci::Bdf;
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
        let rom_path = bootrom::find_bootrom_path(&cli.lpc);
        info!(log, "loading bootrom"; "path" => rom_path.display().to_string());
        let rom = bootrom::BootromFile::open(&rom_path)?;
        info!(log, "bootrom validated";
            "size" => rom.size(),
            "path" => rom.path().display().to_string(),
        );
        let hdl = setup.hdl().clone();
        bootrom::load_bootrom(setup.map_mut(), &hdl, &rom)?;
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
    ).context("failed to apply CPU baseline")?;

    // ----- Device initialization -----

    // Create legacy PIC for ISA interrupt delivery
    let pic = LegacyPIC::new(machine.hdl().clone());

    // Create i440fx chipset (host bridge + ISA bridge + PCI config ports)
    let chipset = I440FxChipset::create(
        machine.bus_pio(),
        Some(machine.hdl().clone()),
    );
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
    let _acpi_pm = vmm_devices::acpi_pm::AcpiPm::create_and_attach(machine.bus_pio());
    info!(log, "ACPI PM registers attached"; "pmbase" => format!("{:#x}", 0x400));

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
    setup_serial_io(&com1, com1_path.as_deref(), "com1", &log);

    // Set up COM2: if a path is specified, use it; otherwise start
    // the built-in mdata agent for guest configuration.
    if let Some(ref path) = com2_path {
        setup_serial_io(&com2, Some(path.as_str()), "com2", &log);
    } else {
        // Start mdata agent on COM2 — this serves network config,
        // hostname, SSH keys to the guest via the SmartOS metadata protocol.
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
        let _rsdp_start = 0; // RSDP is at the start of legacy_tables.data
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
            machine.bus_mmio(),
            machine.physmap(),
            machine.hdl(),
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

    // ----- Direct boot: load kernel into guest RAM -----

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

    // ----- Migration destination mode -----
    //
    // If --migrate-listen is set, we skip bootrom/vCPU setup and instead
    // wait for migration data. After importing state, we start vCPUs
    // with the imported register/device state.
    if let Some(ref listen_addr) = cli.migrate_listen {
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

        let listen_addr_clone = listen_addr.clone();
        let hdl = machine.hdl().clone();
        let physmap = machine.physmap().clone();
        let migrate_num_cpus = num_cpus;
        let migrate_mem_size = mem_size as u64;
        let status = std::sync::Arc::new(std::sync::Mutex::new(
            vmm_migrate::MigrationStatus::default(),
        ));

        info!(log, "waiting for migration connection"; "addr" => &listen_addr_clone);

        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind(&listen_addr_clone)
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
                migrate_num_cpus,
                migrate_mem_size,
                status,
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
    } else {
        // ----- Normal vCPU startup -----

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
    }

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

    // After migration: inject timer interrupt to restart the scheduler tick.
    // In nohz/tickless mode the LAPIC timer was stopped when the guest went
    // idle. timer_fire_when=0 means vlapic_resume() won't arm a callout.
    // We inject LOCAL_TIMER_VECTOR directly into the LAPIC after the vCPU
    // thread has started and entered VM_RUN/HLT.
    if cli.migrate_listen.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(200));
        for vcpu in machine.vcpus() {
            match machine.hdl().lapic_irq(vcpu.id(), 0xEC) {
                Ok(()) => info!(log, "post-migration timer kick"; "vcpu" => vcpu.id()),
                Err(e) => warn!(log, "timer kick failed"; "vcpu" => vcpu.id(), "error" => %e),
            }
        }
    }

    // Spawn control socket if --control-socket was specified
    let control_cleanup = if let Some(ref sock_path) = cli.control_socket {
        let ctrl = control::VmController::new(
            machine.hdl().clone(),
            machine.physmap().clone(),
            cli.vm_name.clone(),
            num_cpus,
            mem_size,
            vcpu_metrics.clone(),
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
    } else {
        None
    };

    // Set up SIGTERM handler for clean shutdown.
    // When zoneadmd sends SIGTERM, trigger ACPI poweroff so the guest
    // can shut down gracefully.
    {
        let hdl_sig = machine.hdl().clone();
        let log_sig = log.clone();
        let _ = unsafe {
            libc::signal(libc::SIGTERM, sigterm_handler as libc::sighandler_t)
        };
        // Spawn a thread to wait for the signal flag
        let hdl_sig2 = hdl_sig.clone();
        thread::Builder::new()
            .name("sigterm-handler".into())
            .spawn(move || {
                loop {
                    thread::sleep(std::time::Duration::from_millis(500));
                    if SIGTERM_RECEIVED.load(std::sync::atomic::Ordering::Relaxed) {
                        info!(log_sig, "SIGTERM received, triggering ACPI poweroff");
                        let _ = hdl_sig2.suspend(
                            bhyve_api::vm_suspend_how::VM_SUSPEND_POWEROFF as u32,
                            0,
                        );
                        break;
                    }
                }
            })
            .expect("failed to spawn sigterm handler thread");
    }

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

    // Clean up control socket
    if let Some(ref path) = control_cleanup {
        let _ = std::fs::remove_file(path);
    }

    // Clean up VM
    if let Err(e) = machine.hdl().destroy() {
        warn!(log, "failed to destroy VM"; "error" => %e);
    }

    info!(log, "VM shutdown complete");
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
    let blk = VirtioBlock::new(file, &opts, Arc::clone(physmap))
        .with_context(|| format!("failed to create virtio-blk for: {}", path))?;

    let lintr = chipset.route_lintr(&bdf);
    let cfg_size = blk.config_size();
    let blk_intr = Arc::clone(&blk.interrupt);

    let pci_dev = VirtioPciDevice::new(
        blk,
        vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_BLOCK,
        1,    // 1 queue for block
        128,  // queue size
        cfg_size,
        lintr,
        Arc::clone(physmap),
        Arc::clone(bus_pio),
    );

    // Wire interrupt: workers call this after pushing used ring
    {
        let pci_weak = Arc::downgrade(&pci_dev);
        *blk_intr.lock().expect("lock") = Some(Arc::new(move || {
            if let Some(dev) = pci_weak.upgrade() {
                dev.raise_queue_interrupt();
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
            let pci_dev = create_virtio_blk_device(
                config, bdf, false, chipset, physmap, bus_pio,
            )?;
            info!(log, "virtio-blk attached";
                "bdf" => %bdf,
                "path" => config,
                "pirq" => "via chipset",
            );
            Ok(Some(pci_dev))
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

            // Start interrupt polling BEFORE moving viona into PCI device.
            // The polling thread needs the viona fd, and we pass a callback
            // that will raise the PCI interrupt when viona completes I/O.
            // We use a Mutex<Option<Weak>> to defer the PCI device reference
            // until after construction.
            let intr_hook: Arc<std::sync::Mutex<Option<std::sync::Weak<VirtioPciDevice<VirtioViona>>>>> =
                Arc::new(std::sync::Mutex::new(None));
            let hook_clone = Arc::clone(&intr_hook);
            let raise_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                if let Some(weak) = hook_clone.lock().ok().and_then(|g| g.clone()) {
                    if let Some(dev) = weak.upgrade() {
                        dev.raise_queue_interrupt();
                    }
                }
            });
            viona.start_intr_poll(raise_fn);

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
            );
            Ok(Some(pci_dev))
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
            Ok(Some(nvme))
        }
        "virtio-rnd" => {
            let rng = vmm_devices::virtio::VirtioRng::new();
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = VirtioPciDevice::new(
                rng,
                vmm_devices::virtio::bits::VIRTIO_DEV_TYPE_RNG,
                1,    // 1 queue
                64,   // queue size
                0,    // no device-specific config
                lintr,
                Arc::clone(physmap),
                Arc::clone(bus_pio),
            );
            chipset.attach_device(bdf, pci_dev.clone());
            info!(log, "virtio-rnd attached"; "bdf" => %bdf);
            Ok(Some(pci_dev))
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
            );
            chipset.attach_device(bdf, fbuf.clone());
            info!(log, "fbuf attached";
                "bdf" => %bdf,
                "vnc" => vnc_path.display().to_string(),
                "resolution" => format!("{}x{}", width, height),
            );
            Ok(Some(fbuf))
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
            Ok(Some(xhci))
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
            Ok(Some(dev))
        }
        "ahci-cd" | "ahci-hd" => {
            anyhow::bail!(
                "AHCI device type '{}' is not yet implemented; use virtio-blk instead",
                driver,
            );
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
/// - `/dev/zconsole` or any path → open as read/write device
fn setup_serial_io(
    uart: &Arc<LpcUart>,
    config: Option<&str>,
    name: &str,
    log: &Logger,
) {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};

    // Strip "socket," prefix (boot.c passes "-l com2,socket,/tmp/vm.ttyb")
    let config = config.map(|c| c.strip_prefix("socket,").unwrap_or(c));

    match config {
        None | Some("stdio") => {
            // stdout for TX, stdin for RX
            uart.set_tx_sink(Box::new(|b: u8| {
                let _ = std::io::stdout().write_all(&[b]);
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
                            let _ = f.write_all(&[b]);
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

    // Register USDT probes before any device setup
    usdt::register_probes().expect("failed to register USDT probes");

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
