# 13 — Implement indirect descriptor support

**Spec:** Section 2.7.5.3 — Indirect descriptors allow a single descriptor to point to a table of descriptors in guest memory, reducing descriptor table consumption.
**Severity:** Medium (performance feature for complex I/O chains)
**Effort:** ~60 lines

## Problem

`queue.rs:407-412` rejects indirect descriptors. The feature constant `VIRTIO_F_RING_INDIRECT_DESC` is defined but never offered. Indirect descriptors are useful for:
- Block devices with large scatter-gather lists
- Network devices with multi-buffer packets
- Any device where descriptor table entries are scarce

## Spec rules

1. A descriptor with `VIRTQ_DESC_F_INDIRECT` set points to a table of indirect descriptors at `addr` with length `len` bytes.
2. The indirect table is a contiguous array of `VirtqDesc` structs in guest memory.
3. MUST NOT set both INDIRECT and NEXT flags on the same descriptor.
4. MUST NOT nest: indirect descriptors within an indirect table MUST NOT have the INDIRECT flag.
5. The indirect table is walked like a normal chain (NEXT flag chains within the table).

## Files to modify

1. `crates/vmm-devices/src/virtio/queue.rs` — implement `walk_indirect_table()`
2. `crates/vmm-devices/src/virtio/block.rs` — offer the feature
3. `crates/vmm-devices/src/virtio/rng.rs` — offer the feature
4. `crates/vmm-devices/src/virtio/bits.rs` — add u32 constant

## Changes

### bits.rs

```rust
pub const VIRTIO_F_RING_INDIRECT_DESC_U32: u32 = 1 << 28;
```

### queue.rs — walk_indirect_table

```rust
/// Walk an indirect descriptor table.
///
/// The indirect descriptor at `desc` has addr/len pointing to a
/// contiguous array of VirtqDesc entries in guest memory.
fn walk_indirect_table(
    &self,
    physmap: &PhysMap,
    desc: &VirtqDesc,
) -> Option<Vec<ChainBuf>> {
    let table_len = desc.len as usize;
    if table_len == 0 || table_len % 16 != 0 {
        return None;  // Invalid: must be multiple of sizeof(VirtqDesc)
    }
    let num_descs = table_len / 16;
    if num_descs > self.size as usize {
        return None;  // Spec: chain MUST NOT be longer than queue size
    }

    // Read the entire indirect table from guest memory
    let sub = physmap.lookup(desc.addr, table_len)?;
    let mut bufs = Vec::with_capacity(num_descs);
    let mut idx = 0usize;

    loop {
        if idx >= num_descs {
            return None;  // Chain overflows indirect table
        }

        let entry_sub = sub.subregion(idx * 16, 16)?;
        let entry = entry_sub.read::<VirtqDesc>().ok()?;

        // Spec: MUST NOT set INDIRECT within indirect table
        if entry.is_indirect() {
            return None;
        }

        if entry.is_write() {
            bufs.push(ChainBuf::Writable { addr: entry.addr, len: entry.len });
        } else {
            bufs.push(ChainBuf::Readable { addr: entry.addr, len: entry.len });
        }

        if entry.has_next() {
            idx = entry.next as usize;
        } else {
            break;
        }
    }

    Some(bufs)
}
```

### queue.rs — update collect_chain

```rust
if desc.is_indirect() {
    if !self.indirect_supported {
        return None;
    }
    // Spec: MUST NOT set both INDIRECT and NEXT
    if desc.has_next() {
        return None;
    }
    // Walk the indirect table and append buffers
    let indirect_bufs = self.walk_indirect_table(physmap, &desc)?;
    bufs.extend(indirect_bufs);
    break;  // Indirect descriptor terminates the chain
}
```

### Device backends — offer the feature

```rust
// block.rs, rng.rs:
fn device_features(&self) -> u32 {
    // ... existing features ...
    | bits::VIRTIO_F_RING_INDIRECT_DESC_U32
}
```

## Dependencies

- Task 06 (conditional indirect rejection) should be implemented first.

## Test

```rust
#[test]
fn indirect_descriptor_single_entry() {
    // Create an indirect table with 1 descriptor in guest memory
    // Point a main descriptor to it with INDIRECT flag
    // collect_chain should return the single buffer from the indirect table
}

#[test]
fn indirect_descriptor_chain() {
    // Create an indirect table with 3 chained descriptors
    // Verify all 3 buffers are collected
}

#[test]
fn indirect_nesting_rejected() {
    // Create an indirect table where an entry also has INDIRECT flag
    // collect_chain should return None
}

#[test]
fn indirect_plus_next_rejected() {
    // Create a descriptor with both INDIRECT and NEXT flags
    // collect_chain should return None
}
```

Run: `cargo test -p vmm-devices -- queue`
