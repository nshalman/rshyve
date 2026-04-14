# 15 — ACPI tables generated twice with identical parameters

**Priority:** MEDIUM
**File:** `bin/vmm/src/main.rs:374, 395`
**Category:** Code quality — wasted computation, copy-paste artifact

## Problem

```rust
let legacy_tables = acpi::generate_acpi_tables(num_cpus);  // line 374
// ... 20 lines later ...
let acpi_tables = acpi::generate_acpi_tables(num_cpus);     // line 395
```

Same function, same input, called twice. The first call appears to write the RSDP to a legacy location, and the second writes the full tables to the fw_cfg device. Both generate identical data.

## Fix

Call once, reuse:

```rust
let acpi_tables = acpi::generate_acpi_tables(num_cpus);

// Write RSDP to legacy location
physmap.write(RSDP_GPA, &acpi_tables.rsdp)?;

// ... later, same tables for fw_cfg ...
fwcfg.add_acpi_tables(&acpi_tables);
```

Verify whether `legacy_tables` and `acpi_tables` are actually used differently. If the first is only used for the RSDP and the second for the full table blob, extract the RSDP from the single generated result.
