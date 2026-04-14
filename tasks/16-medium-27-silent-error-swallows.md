# 16 — 27 instances of `let _ = result` silently swallowing errors

**Priority:** MEDIUM
**Category:** Silent failures — errors lost across the codebase

## Problem

`let _ = result` discards `Result` values throughout the codebase. Many are on critical paths where silent failure means guest I/O hangs, interrupts lost, or unrecoverable states.

## Inventory — grouped by impact

### Guest I/O hangs (used ring writes):
- `crates/vmm-devices/src/virtio/queue.rs:264,267,274` — `push_used` entry + index writes
- `crates/vmm-devices/src/virtio/queue.rs:617-631` — `VirtioCompletion::write_used_entry`
- `crates/vmm-devices/src/nvme/mod.rs:1172` — NVMe CQE write to guest memory

### Lost interrupts:
- `crates/vmm-devices/src/pci/msix.rs:130,183` — MSI-X delivery ioctl
- `crates/vmm-core/src/intr_pins.rs:288,397,403` — IOAPIC deassert

### Lost I/O requests:
- `crates/vmm-devices/src/nvme/mod.rs:1099` — NVMe I/O channel send
- `crates/vmm-devices/src/virtio/block.rs:297` — virtio-blk I/O channel send

### VM stuck in broken state:
- `bin/vmm/src/control.rs:432,568` — thread spawn (covered in task #10)
- `bin/vmm/src/control.rs:274,303` — resume before shutdown
- `bin/vmm/src/main.rs:707` — SIGTERM suspend ioctl

### vCPU corruption:
- `bin/vmm/src/vcpu_tasks.rs:172-177` — set_reg for RDMSR emulation
- `bin/vmm/src/vcpu_tasks.rs:142,192,220,234,255,264,277,289` — vCPU event channel sends

### Device init failures:
- `crates/vmm-devices/src/acpi_pm.rs:94,102,110,118` — PIO registration
- `crates/vmm-devices/src/virtio/pci.rs:235-239` — BAR registration

### Resource cleanup:
- `crates/vmm-devices/src/passthru.rs:1023,1039,1047` — unbind/unmap on Drop
- `crates/vmm-devices/src/fbuf/mod.rs:495,546` — VNC socket cleanup
- `bin/vmm/src/control.rs:620` — stale socket removal

## Fix — by category

### Category 1: Log and continue (I/O hot path)
For used ring writes and interrupt delivery where returning an error isn't practical:

```rust
// Before:
let _ = sub.write::<u32>(&val);

// After:
if let Err(e) = sub.write::<u32>(&val) {
    slog::warn!(log, "failed to write used ring entry"; "gpa" => gpa, "error" => %e);
}
```

### Category 2: Return error (channel sends)
For I/O dispatch where the channel may be closed:

```rust
// Before:
let _ = self.io_tx.send(req);

// After:
if self.io_tx.send(req).is_err() {
    slog::error!(log, "I/O worker channel closed — request dropped");
    // Write error status to guest
    write_status(physmap, status_addr, VIRTIO_BLK_S_IOERR);
}
```

### Category 3: Fail loudly (device init)
For PIO/BAR registration during initialization:

```rust
// Before:
let _ = bus_pio.register(base, len, handler);

// After:
bus_pio.register(base, len, handler)
    .expect("ACPI PM PIO registration must succeed");
```

### Category 4: Acceptable (cleanup)
Drop impls and stale file removal are acceptable uses of `let _ =`. Document with a comment:

```rust
let _ = std::fs::remove_file(path); // Best-effort cleanup, file may not exist
```

## Scope

27 call sites across 12 files. Prioritize categories 2 and 3 first (clear bugs), then category 1 (observability).
