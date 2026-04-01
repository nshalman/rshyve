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
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::uart::lpc::{self, LpcUart};

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
fn spawn_serial_drain(uart: Arc<LpcUart>, _log: Logger) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("serial-drain".into())
        .spawn(move || {
            let mut total_bytes = 0u64;
            loop {
                match uart.output_byte() {
                    Some(b) => {
                        total_bytes += 1;
                        let mut stderr = std::io::stderr().lock();
                        let _ = stderr.write_all(&[b]);
                        if total_bytes <= 5 || total_bytes % 100 == 0 {
                            let _ = writeln!(stderr,
                                "\n[serial] byte #{}: {:#04x} '{}'",
                                total_bytes, b,
                                if b.is_ascii_graphic() || b == b' ' {
                                    b as char
                                } else {
                                    '.'
                                },
                            );
                        }
                    }
                    None => {
                        thread::sleep(std::time::Duration::from_millis(1));
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
    info!(log, "PM timer attached"; "port" => format!("{:#x}", pmtimer.port()));

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
    let _serial_handle = spawn_serial_drain(Arc::clone(&com1), log.clone());

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
