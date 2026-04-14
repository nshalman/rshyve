# Fix Spurious IRQ "Nobody Cared" Bug + Performance Improvements

## Context

The guest Linux kernel reports `irq N: nobody cared` for virtio devices (IRQ #16 = virtio-blk, IRQ #18 = virtio-net), followed by `Disabling IRQ` and eventually a soft lockup. This means the guest receives PCI interrupts but finds the ISR (Interrupt Status Register) is 0 when the handler reads it.

The root cause is a race condition in `raise_interrupt()` combined with performance-hostile locking patterns.

## Bug: Root Cause

**File**: `crates/vmm-devices/src/virtio/pci.rs:253-258`

```rust
pub fn raise_interrupt(&self) {
    if let Some(pin) = &self.intr_pin {
        pin.deassert();   // force edge by toggling
        pin.assert();
    }
}
```

This deassert+assert pattern attempts edge-triggered semantics on what should be level-triggered PCI INTx. The race:

1. **Batch 1 completes**: Worker calls `raise_queue_interrupt()` → sets ISR=1, deassert (no-op), assert → guest gets interrupt
2. **Batch 2 completes simultaneously**: Worker calls `raise_queue_interrupt()` → sets ISR|=1 (already 1), deassert (creates falling edge), assert (creates rising edge) → **second interrupt queued in LAPIC**
3. **Guest handles interrupt #1**: reads ISR=1, clears ISR=0, deasserts pin, returns IRQ_HANDLED
4. **Guest handles interrupt #2** (queued from step 2): reads ISR=0 → **returns IRQ_NONE → "nobody cared"**

After 99,900 such events, Linux disables the IRQ entirely → no more I/O completions → soft lockup.

Affects all three callers of `raise_queue_interrupt()`: virtio-blk async workers, virtio-blk ahci fallback, and viona interrupt poll thread.

## Fix

### Step 1: Fix `raise_interrupt()` — level-triggered semantics

**File**: `crates/vmm-devices/src/virtio/pci.rs`

Change `raise_interrupt()` to just assert (no deassert):
```rust
pub fn raise_interrupt(&self) {
    if let Some(pin) = &self.intr_pin {
        pin.assert();
    }
}
```

PCI INTx is level-triggered. Assert on work pending, deassert on ISR read. The IOAPIC/LAPIC handles re-delivery correctly when the pin is re-asserted after deassert.

### Step 2: Fix `PciIntrPin::deassert()` — guard against redundant ioctls

**File**: `crates/vmm-core/src/intr_pins.rs:392-406`

Current code unconditionally deasserts and calls the kernel ioctl even if the pin is already low. Add a guard like `LegacyPin` does:

```rust
fn deassert(&self) {
    let mut asserted = self.asserted.lock().expect("pci pin lock");
    if *asserted {
        *asserted = false;
        // ... ioctl
    }
}
```

This eliminates unnecessary kernel ioctls on the hot path.

### Step 3: Make ISR lock-free with `AtomicU8`

**File**: `crates/vmm-devices/src/virtio/pci.rs`

Move `isr_status` out of `VirtioPciState` (behind Mutex) to an `AtomicU8` on `VirtioPciDevice`:

- Workers set ISR: `self.isr_status.fetch_or(ISR_QUEUE_INTR, Ordering::Release)` — no lock needed
- Guest reads ISR: `self.isr_status.swap(0, Ordering::Acquire)` — no lock needed
- Device reset: `self.isr_status.store(0, Ordering::Relaxed)`

This eliminates Mutex contention between worker threads and the vCPU thread on every interrupt. Currently, `raise_queue_interrupt()` acquires the `virtio_state` Mutex just to OR one byte — that's the same lock that `bar_read`/`bar_write` hold during PIO processing.

### Step 4: Make interrupt pin state lock-free with `AtomicBool`

**File**: `crates/vmm-core/src/intr_pins.rs`

Replace `Mutex<bool>` with `AtomicBool` in `PciIntrPin::asserted` (and `IoApicPin`, `LegacyPin`):

```rust
fn assert(&self) {
    if !self.asserted.swap(true, Ordering::Acquire) {
        // was false, now true — transition, call ioctl
    }
}

fn deassert(&self) {
    if self.asserted.swap(false, Ordering::Release) {
        // was true, now false — transition, call ioctl
    }
}
```

Eliminates a Mutex acquisition on every interrupt assert/deassert. The `swap` is atomic and handles the check+set in one operation.

## Files to Modify

| File | Change |
|------|--------|
| `crates/vmm-devices/src/virtio/pci.rs` | Remove deassert from `raise_interrupt()`; move `isr_status` to `AtomicU8`; update `bar_read`/`bar_write`/`raise_queue_interrupt`/`reset` |
| `crates/vmm-core/src/intr_pins.rs` | `AtomicBool` for pin state in `PciIntrPin`, `IoApicPin`, `LegacyPin`; guard in `PciIntrPin::deassert()` |

## Performance Impact Summary

| Change | Effect |
|--------|--------|
| Remove deassert from `raise_interrupt()` | Eliminates 1 kernel ioctl per interrupt (was 2, now 1 assert + 1 deassert on ISR read) |
| Atomic ISR | Eliminates Mutex contention between workers and vCPU on hot path |
| Atomic pin state | Eliminates Mutex per assert/deassert |
| Guard on deassert | Eliminates redundant kernel ioctls |
| Fixes "nobody cared" | Prevents IRQ disable → no more soft lockups |

## Verification

1. `cargo build` — ensure it compiles
2. `cargo test` — run existing tests (isr_read_clears test will need updating since ISR moves to AtomicU8)
3. Boot a Linux guest and verify:
   - No `nobody cared` messages in dmesg
   - No `Disabling IRQ` messages
   - No soft lockups
   - virtio-blk I/O completes successfully
   - virtio-net-viona functions correctly
4. Run I/O stress test (e.g., `fio` with high iodepth) to exercise the async batch path and confirm interrupt coalescing still works
