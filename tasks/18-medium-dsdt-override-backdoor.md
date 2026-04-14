# 18 — Hardcoded DSDT override path — testing backdoor in production

**Priority:** MEDIUM
**File:** `bin/vmm/src/main.rs:332`
**Category:** Security — arbitrary ACPI table injection

## Problem

```rust
let dsdt_override = std::fs::read("/opt/vmmnew/test_dsdt.aml").ok();
```

If `/opt/vmmnew/test_dsdt.aml` exists, it is silently loaded as the VM's DSDT (Differentiated System Description Table). No CLI flag controls this. No log message warns about it (there IS a log at line 334, but only if the file exists).

Any process that can write to `/opt/vmmnew/` can inject arbitrary ACPI tables into every VM started on the host. ACPI tables contain AML bytecode that the guest firmware executes — a malicious DSDT could:

- Expose fake hardware devices
- Change power management behavior
- Alter the guest's view of system topology

## Fix

Option A — Remove entirely (recommended for production):

```rust
// Delete lines 332-337
```

Option B — Gate behind a CLI flag:

```rust
// In vmm-config/src/lib.rs, add:
#[arg(long)]
pub dsdt_override: Option<PathBuf>,

// In main.rs:
let dsdt_override = cli.dsdt_override.as_ref()
    .map(|p| std::fs::read(p))
    .transpose()?;
if dsdt_override.is_some() {
    warn!(log, "using DSDT override from CLI — not for production use";
        "path" => %cli.dsdt_override.as_ref().unwrap().display());
}
```

Option A is preferred. If testing DSDT overrides is needed, it should be a deliberate action (CLI flag), not an ambient file check.
