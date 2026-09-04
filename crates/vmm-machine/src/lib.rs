// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Machine plumbing shared by every VMM binary in this workspace.
//!
//! This crate holds the phase BODIES: guest memory, chipset, PCI bus,
//! interrupt routing, firmware table emission, vCPU spawn, event loop
//! and teardown. It names no concrete PCI driver type. Each binary keeps
//! its own `run()` naming the phase SEQUENCE, because that ordering is
//! load-bearing.

pub mod build;
pub mod chipset;
pub mod firmware;
pub mod hotplug;
pub mod parse;
pub mod pci;
pub mod pm;
pub mod registry;
pub mod signal;
pub mod teardown;
pub mod vcpu;
pub mod vcpu_tasks;
pub mod vcpus;

pub use build::{
    build_machine, host_tsc_frequency_hz, no_pre_finalize, MachineBuild,
    MachineOpts,
};
pub use chipset::{init_chipset, ChipsetDevices};
pub use firmware::{
    hotplug_enabled, setup_fwcfg_and_acpi, write_legacy_acpi_tables,
    write_smbios_tables, AcpiTables,
};
pub use hotplug::cpu::{CpuHotplugEngine, CpuHotplugError, CpuOnline};
pub use hotplug::mem::{
    MemHotplugEngine, MemHotplugError, MemWindow, DEFAULT_SLOT_SIZE,
};
pub use hotplug::{
    hot_add_window, hotplug_specs, CpuSlots, HotplugEngine, HotplugEngines,
    HotplugError, HotplugFactory, VmPause,
};
pub use parse::{find_lpc_device, parse_bdf};
pub use pci::{
    attach_pci_devices, CreatedPciDevice, LifecycleHandle, PciAttachment,
    PciDeviceCtx, PciDeviceFactory, PciDeviceHandle,
};
pub use pm::{
    mem_hotplug_window, register_pm_devices, setup_timers_and_pm, Gpe0,
    HotplugOpts, HotplugRegisters, TimerAndPmDevices,
};
pub use registry::{
    DeviceRegistry, RegisteredDevice, RegistryError, SlotState,
};
pub use signal::{install_sigterm_handler, sigterm_flag, sigterm_received};
pub use teardown::{
    join_vcpus_bounded, quiesce_devices, quiesce_registered_devices,
    registry_named_devices, run_event_loop, run_event_loop_growable,
    run_event_loop_registered, RunOutcome, VcpuSet, VcpuThreads,
};
pub use vcpu::{
    activate_all_vcpus, activate_one, activate_uefi_bsp_only, activate_vcpus,
    check_aps_awaiting_sipi, set_halt_exit, spawn_vcpu_fleet, spawn_vcpus,
    uefi_reset_targets, BspEntry, VcpuFleet,
};
pub use vcpu_tasks::{
    spawn_boot_threads, spawn_one, start_gate, StartGate, StartSignal,
    VcpuEvent, VcpuThreadCtx,
};
pub use vcpus::{VcpuError, VcpuRegistry, VcpuSetup};
