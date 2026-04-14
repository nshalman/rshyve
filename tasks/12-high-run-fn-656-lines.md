# 12 — `run()` function is 656 lines — decompose

**Priority:** HIGH
**File:** `bin/vmm/src/main.rs:97-754`
**Category:** Code quality — maintainability, testability

## Problem

The `run()` function handles the entire VM lifecycle in one function:

- VM creation and memory setup (~50 lines)
- Bootrom / direct-boot loading (~40 lines)
- Chipset and interrupt controller init (~30 lines)
- UART / serial console setup (~40 lines)
- ACPI table generation and loading (~60 lines)
- fw_cfg device setup (~30 lines)
- SMBIOS table generation (~20 lines)
- PCI device parsing and initialization (~200 lines)
- Migration mode handling (~50 lines)
- vCPU thread spawning (~30 lines)
- Signal handler setup (~20 lines)
- Event loop (~40 lines)
- Cleanup (~20 lines)

At 656 lines, this is untestable — you can't unit test ACPI generation without also creating a VM, parsing CLI args, and initializing all devices.

## Fix

Extract into focused functions:

```rust
fn run(cli: Cli, log: &Logger) -> anyhow::Result<()> {
    let vm = create_vm(&cli, log)?;
    let machine = init_machine(&vm, &cli, log)?;
    let devices = init_devices(&machine, &cli, log)?;
    setup_acpi(&machine, &devices, &cli, log)?;
    setup_serial(&machine, &cli, log)?;

    if let Some(migration) = cli.migration_mode() {
        return handle_migration(migration, &machine, &devices, log);
    }

    let vcpu_handles = spawn_vcpus(&machine, &devices, log)?;
    let ctrl = spawn_control(&machine, &devices, &cli, log)?;
    run_event_loop(&machine, vcpu_handles, ctrl, log)?;
    cleanup(&machine, &cli, log)
}
```

Each extracted function:
- Has a clear input/output contract
- Can be tested independently (with mocked Machine/PhysMap)
- Has a focused responsibility

### Specific extractions

1. `init_pci_devices(machine, cli, log) -> Vec<Arc<dyn PciDevice>>` — the 200-line PCI device parsing loop
2. `setup_acpi(machine, num_cpus, log)` — ACPI table generation and loading
3. `setup_serial(machine, cli, log)` — UART creation and serial drain thread
4. `spawn_vcpus(machine, devices, log) -> Vec<JoinHandle<()>>` — vCPU thread creation
