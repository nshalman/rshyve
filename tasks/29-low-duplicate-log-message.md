# 29 — Duplicate log message 3 lines apart

**Priority:** LOW
**File:** `bin/vmm/src/vcpu_tasks.rs:119, 122`
**Category:** Code quality — copy-paste artifact

## Problem

```rust
debug!(log, "vCPU run loop starting");
let mut next_entry = VmEntry::Run;
info!(log, "vCPU run loop starting");
```

Same message logged at both `debug` and `info` level within 3 lines.

## Fix

Remove the `debug!` line, keep only the `info!` line (or vice versa):

```rust
let mut next_entry = VmEntry::Run;
info!(log, "vCPU run loop starting"; "vcpu" => vcpu_id);
```
