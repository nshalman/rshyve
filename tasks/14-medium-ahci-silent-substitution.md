# 14 — AHCI silently substituted with virtio-blk

**Priority:** MEDIUM
**File:** `bin/vmm/src/main.rs:1028-1047`
**Category:** Correctness — silent device type change

## Problem

```rust
"ahci-cd" | "ahci-hd" => {
    warn!(log, "AHCI not yet implemented, using virtio-blk fallback");
    // ... creates a VirtioBlock device instead
}
```

When a user specifies `-s 4,ahci-hd,/path/to/disk.raw`, they get a virtio-blk device instead of AHCI. The guest sees a completely different device type requiring different drivers. A guest OS expecting AHCI/SATA will not have virtio drivers loaded and will fail to find the disk.

A `warn` buried in logs is insufficient for a device type substitution.

## Fix

Option A — Return an error (strict):

```rust
"ahci-cd" | "ahci-hd" => {
    anyhow::bail!(
        "AHCI device type '{}' is not yet implemented. \
         Use 'virtio-blk' instead: -s {},{},{}",
        driver, slot, "virtio-blk", conf.unwrap_or("")
    );
}
```

Option B — Log at error level and add a CLI flag to opt-in to fallback:

```rust
"ahci-cd" | "ahci-hd" => {
    if !cli.allow_device_fallback {
        anyhow::bail!("AHCI not implemented. Use --allow-device-fallback to auto-substitute virtio-blk.");
    }
    error!(log, "AHCI not implemented — substituting virtio-blk"; "requested" => driver);
    // ...
}
```

Option A is recommended. Explicit is better than implicit, especially for device types that affect guest driver requirements.
