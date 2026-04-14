# 36 — Virtio v1.3 Specification Compliance Audit

**Priority:** Mixed (violations are HIGH, gaps are MEDIUM, missing features are LOW-MEDIUM)
**Spec:** `docs/virtio-v1.3-csd01.html`
**Category:** Spec compliance — comprehensive comparison

---

## SPEC VIOLATIONS (MUST-level requirements not met)

### V1. No flags-based notification suppression (non-EVENT_IDX path)

**Spec:** Section 2.7.7 — "If flags is 1, the device SHOULD NOT send a notification." "If flags is 0, the device MUST send a notification." When EVENT_IDX is NOT negotiated, the device must read the `flags` field of the available ring to decide whether to interrupt.

**Code:** `queue.rs:303-309`
```rust
pub fn should_notify_guest(&self, physmap: &PhysMap, old_used_idx: u16) -> bool {
    if !self.event_idx {
        return true;  // Always interrupts — never reads flags
    }
    // ... EVENT_IDX path (correct) ...
}
```
Same pattern at `queue.rs:792-794` in `VirtioCompletion::should_interrupt()`.

**Impact:** Device always fires an interrupt on every used ring update, even when the guest explicitly set `VIRTQ_AVAIL_F_NO_INTERRUPT` (flags=1) to suppress notifications. Wastes VM exits and CPU cycles. Linux guests use this flag extensively during NAPI polling.

**Fix:**
```rust
if !self.event_idx {
    // Read flags from avail ring (offset 0)
    let flags = physmap.lookup(self.avail_addr, 2)
        .and_then(|sub| sub.read::<u16>().ok())
        .unwrap_or(0);
    return flags & 1 == 0;  // Notify only if VIRTQ_AVAIL_F_NO_INTERRUPT is NOT set
}
```
Apply same fix to `VirtioCompletion::should_interrupt()`, reading from `self.avail_addr`.

---

### V2. EVENT_IDX hardcoded in transport, bypassing device feature negotiation

**Spec:** Section 2.2 — "A device MUST NOT offer features it wouldn't support if accepted."

**Code:** `pci.rs:294-295`
```rust
bits::LEGACY_REG_DEVICE_FEATURES => {
    self.device.device_features()
        | bits::VIRTIO_F_RING_EVENT_IDX as u32  // hardcoded
}
```

**Impact:** The transport layer injects EVENT_IDX into every device's feature set regardless of whether the backend supports it. This violates the spec's feature negotiation model. If a new device backend is added that doesn't handle EVENT_IDX, the guest will negotiate it and then get incorrect notification behavior.

**Fix:** Remove the OR from `pci.rs`. Have each device backend include EVENT_IDX in `device_features()`:
```rust
// In block.rs:
fn device_features(&self) -> u32 {
    let mut features = VIRTIO_BLK_F_FLUSH | VIRTIO_BLK_F_BLK_SIZE;
    features |= VIRTIO_F_RING_EVENT_IDX;  // device explicitly offers it
    // ...
    features as u32
}
```

---

### V3. No FEATURES_OK validation (device side)

**Spec:** Section 3.1.1, steps 5-6 — Driver writes FEATURES_OK, then re-reads status. Device must be able to reject invalid features by clearing FEATURES_OK on re-read.

**Code:** `pci.rs:395-408`
```rust
bits::LEGACY_REG_DEVICE_STATUS => {
    let new_status = val as u8;
    if new_status == 0 {
        // reset path ...
    } else {
        vs.status = new_status;  // stored verbatim, no validation
    }
}
```

**Mitigating factor:** This is a legacy (transitional) transport. The spec says transitional drivers MAY omit steps 5-6. But modern guests (Linux 4.0+) DO use FEATURES_OK and expect the device to validate.

**Fix:** When FEATURES_OK is set, check that `guest_features ⊆ device_features`:
```rust
if new_status & STATUS_FEATURES_OK != 0
    && vs.status & STATUS_FEATURES_OK == 0 {
    // First time FEATURES_OK is being set — validate
    let offered = self.device.device_features();
    if vs.guest_features & !offered != 0 {
        // Guest requested features we didn't offer — reject
        vs.status = new_status & !STATUS_FEATURES_OK;
        return;
    }
}
vs.status = new_status;
```

---

### V4. No feature subset enforcement on guest_features write

**Spec:** Section 2.2 — "A driver MUST NOT accept a feature which the device did not offer."

**Code:** `pci.rs:355-357`
```rust
bits::LEGACY_REG_GUEST_FEATURES => {
    vs.guest_features = val;  // stored as-is, never masked
    self.device.set_features(val);
```

**Fix:** Mask against offered features:
```rust
let offered = self.device.device_features()
    | bits::VIRTIO_F_RING_EVENT_IDX as u32;  // or remove per V2 fix
vs.guest_features = val & offered;
self.device.set_features(vs.guest_features);
```

---

### V5. Indirect descriptor rejection is unconditional (not gated on feature)

**Spec:** Section 2.7.5.3 — If `VIRTIO_F_INDIRECT_DESC` is negotiated, the device MUST support indirect descriptors.

**Code:** `queue.rs:407-412`
```rust
// Reject indirect descriptors -- not supported.
if desc.is_indirect() {
    return None;
}
```

**Status:** No violation today — no backend offers `VIRTIO_F_INDIRECT_DESC`. But the rejection is unconditional, meaning if any backend offers it in the future, the device would violate the spec by rejecting what was negotiated.

**Fix:** Make the check conditional:
```rust
if desc.is_indirect() {
    if !self.indirect_supported {
        return None;  // Not negotiated — reject
    }
    // Walk indirect table (future implementation)
    return self.walk_indirect_table(physmap, &desc);
}
```

---

### V6. avail_end calculation off by 2 bytes

**Spec:** Section 2.7 — Available ring: flags(2) + idx(2) + ring[N](2*N) + used_event(2) = **6 + 2*N bytes**.

**Code:** `queue.rs:164-165`
```rust
let avail_end = self.avail_addr + 4 + u64::from(self.size) * 2;
//                                 ^ should be 6 (includes used_event)
```

**Impact:** Cosmetic — page alignment absorbs the 2-byte error. The `used_event` IS written at the correct offset elsewhere (`avail_addr + 4 + size * 2`, which is correct relative to avail_addr).

**Fix:** `self.avail_addr + 6 + u64::from(self.size) * 2`

---

### V7. VirtioDevice trait limits features to 32 bits

**Spec:** Section 2.2, 6.3 — Feature bits go up to 127+. `VIRTIO_F_VERSION_1` is bit 32.

**Code:** `mod.rs` trait definition:
```rust
fn device_features(&self) -> u32;
fn set_features(&mut self, features: u32);
```

**Impact:** Cannot negotiate `VIRTIO_F_VERSION_1` or any feature above bit 31. Acceptable for legacy-only transport but blocks modern transport support.

**Fix (legacy):** Document as intentional limitation.
**Fix (modern):** Change to `u64`:
```rust
fn device_features(&self) -> u64;
fn set_features(&mut self, features: u64);
```

---

## COMPLIANCE GAPS (SHOULD/MAY level)

### G1. DEVICE_NEEDS_RESET never set

**Spec:** Section 2.1.2 — Device MAY set DEVICE_NEEDS_RESET (bit 6) on unrecoverable error.
**Code:** `bits.rs:22` defines the constant. Never used anywhere.
**Fix:** Add a method to set status |= DEVICE_NEEDS_RESET and fire ISR_CFG_CHANGE when a backend encounters a fatal error.

### G2. No config change interrupts

**Spec:** Section 4.1.4.5 — Config changes signaled via ISR bit 1 (ISR_CFG_CHANGE).
**Code:** `bits.rs:131` defines `ISR_CFG_CHANGE`. Never set.
**Impact:** No live disk resize, no link status change notifications to guest.

### G3. Block device uses fsync instead of fdatasync

**Spec:** Section 5.2.6.1 — FLUSH flushes "the volatile write cache."
**Code:** `block.rs:176, 371` — `libc::fsync(fd)`.
**Impact:** Correct but slower — fsync also flushes file metadata. `fdatasync` is sufficient per spec and faster.

### G4. Invalid queue notify silently ignored

**Spec:** Section 4.1.4.3.1 — Device SHOULD handle malformed driver behavior gracefully.
**Code:** `pci.rs:380-383` — Checks DRIVER_OK but not queue index bounds. Invalid index silently returns false from `notify_queue`.
**Fix:** Log a warning for invalid queue indices.

### G5. Incomplete descriptor direction validation in block device

**Spec:** Section 5.2.6 — Header MUST be device-readable, status MUST be device-writable.
**Code:** Header validated as Readable. Data direction validated correctly. Status byte direction NOT explicitly validated — `write_status` just fails silently if last buffer isn't Writable.

---

## MISSING FEATURES (sorted by importance)

| # | Feature | Spec Section | Priority | Notes |
|---|---------|-------------|----------|-------|
| M1 | Modern PCI transport | 4.1 | HIGH | Capability structures, 64-bit addresses, modern device IDs 0x1040+ |
| M2 | MSI-X interrupts | 4.1.4.4 | HIGH | Per-queue vectors, no ISR polling. Required for non-transitional |
| M3 | Indirect descriptors | 2.7.5.3 | MEDIUM | Feature defined but not implemented. Reduces descriptor table pressure |
| M4 | DISCARD / WRITE_ZEROES | 5.2.6.2-3 | MEDIUM | TRIM/UNMAP for thin provisioning, SSD lifetime, guest perf |
| M5 | Block seg_max / size_max config | 5.2.4 | MEDIUM | Guest needs to know max segments and max segment size per request |
| M6 | Block multi-queue (MQ) | 5.2.3.1 | MEDIUM | Per-vCPU queues, eliminates single-queue lock contention |
| M7 | Packed virtqueues | 2.8 | MEDIUM | Better cache locality. Introduced in virtio 1.1 |
| M8 | Block GET_ID command | 5.2.6.1 | LOW | Device serial/identifier for `/dev/disk/by-id/` |
| M9 | MMIO transport | 4.2 | LOW | Alternative to PCI for embedded/platform devices |
| M10 | VIRTIO_F_VERSION_1 | 6.3 | LOW | Requires 64-bit features (V7 fix) |

---

## CORRECTLY IMPLEMENTED (verified against spec)

| # | Area | Spec Section | Code Location | Notes |
|---|------|-------------|---------------|-------|
| OK1 | Split virtqueue layout | 2.7 | `queue.rs:146-167` | Correct alignment, PFN computation |
| OK2 | Descriptor chain bounded by queue size | 2.7.5 | `queue.rs:400` | Prevents infinite loops |
| OK3 | Acquire fence after avail_idx read | 2.7.13 step 4 | `queue.rs:214` | Ensures desc reads see driver writes |
| OK4 | Release fence before used_idx write | 2.7.8 | `queue.rs:366, 832` | Guest sees entry before index |
| OK5 | SeqCst fence before avail_event read | 2.7.7.1 | `queue.rs:315, 802` | Full barrier for notification check |
| OK6 | SeqCst fence after used_event write | 2.7.10 | `queue.rs:260` | Ensures ordering with avail_idx re-read |
| OK7 | vring_need_event wrapping comparison | 2.7.7.1 | `queue.rs:479-482` | Matches Linux/QEMU, extensively tested |
| OK8 | EVENT_IDX re-check loop | 2.7.10 | `mod.rs:111-119, block.rs:247-301` | Prevents missed kicks |
| OK9 | PCI vendor ID 0x1AF4 | 4.1.2 | `bits.rs:108` | Correct |
| OK10 | Transitional device IDs | 4.1.2 | `pci.rs:140-158` | 0x1000-0x103F range |
| OK11 | ISR atomic read-and-clear | 4.1.4.5 | `pci.rs:322` | `swap(0, AcqRel)` |
| OK12 | ISR race re-check | 4.1.4.5 | `pci.rs:328-331` | Prevents lost interrupts |
| OK13 | Reset clears all state | 2.4 | `pci.rs:397-405` | Status, queues, ISR, interrupt pin |
| OK14 | Notifications blocked before DRIVER_OK | 3.1.1 | `pci.rs:381` | Guards queue notify |
| OK15 | Block request header format | 5.2.6 | `block.rs:42-48` | type/reserved/sector correct |
| OK16 | Block used_len semantics | 5.2.6 | all block paths | data+status for read, 1 for write |
| OK17 | RNG device | 5.5 | `rng.rs` | No config, single queue, fills writable buffers |
| OK18 | Queue size power-of-2, max 32768 | 2.7 | `queue.rs:101-102` | Enforced in constructor |

---

## TRANSPORT STATUS: Legacy Only

The implementation is purely **legacy (transitional)** PCI transport:

- PCI BAR0 PIO registers at offsets 0x00-0x13 + device config at 0x14+
- Transitional device IDs (Net=0x1000, Block=0x1001, RNG=0x1003)
- 32-bit features only (single register)
- PFN-based queue addressing (single 32-bit register)
- Guest-native byte order (x86 little-endian, no conversion)
- INTx only (no MSI-X)

**Not implemented for modern transport:**
- PCI capability structures (COMMON_CFG, NOTIFY_CFG, ISR_CFG, DEVICE_CFG, PCI_CFG)
- 64-bit feature registers with feature_select
- Separate 64-bit queue address registers (desc, avail, used)
- Explicit little-endian data format
- Modern device IDs (0x1040+)
- MSI-X interrupt vector configuration
- Notification structure with multiplier

---

## IMPLEMENTATION PRIORITY

For fixing spec violations, recommended order:

1. **V1** (flags-based notification suppression) — Easy fix, real performance impact
2. **V4** (mask guest features to offered set) — One-line fix, correctness
3. **V6** (avail_end off-by-2) — One-line fix, cosmetic
4. **V2** (EVENT_IDX per-device) — Architectural cleanup
5. **V3** (FEATURES_OK validation) — Important for modern guests
6. **V5** (conditional indirect rejection) — Future-proofing
7. **V7** (64-bit features) — Only needed for modern transport
