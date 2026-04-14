# Vulnerability Analysis: `intr_pins.rs`

## Finding 1: `PciIntrPin::deassert()` Missing Guard — Spurious Kernel IRQ Manipulation

**Severity: Medium**
**Type: Logic Bug / Missing State Check**
**Lines: 303-316**

### Root Cause

`PciIntrPin::deassert()` unconditionally sets `*asserted = false` and fires the kernel ioctl, without checking whether the pin is currently asserted:

```rust
// PciIntrPin::deassert() — BUGGY
fn deassert(&self) {
    let mut asserted = self.asserted.lock().expect("pci pin lock");
    *asserted = false;          // <-- no guard: always runs
    match self.atpic_irq() {    // <-- always calls kernel ioctl
        ...
    }
}
```

Compare to the **correct** pattern in `IoApicPin::deassert()` (line 241) and `LegacyPin::deassert()` (line 186):

```rust
// IoApicPin::deassert() — CORRECT
fn deassert(&self) {
    let mut asserted = self.asserted.lock()...;
    if *asserted {              // <-- guarded
        *asserted = false;
        let _ = self.hdl.ioapic_deassert_irq(self.irq);
    }
}
```

### Exploitation

A device emulation path that calls `deassert()` on an already-deasserted `PciIntrPin` sends a spurious `isa_deassert_irq` / `ioapic_deassert_irq` to the kernel. If the kernel tracks level counts, this drives the count below the logical zero, causing the IRQ to become permanently stuck — it can never be re-asserted because the kernel thinks it's still "owed" a deassert. Result: **guest IRQ line goes permanently dead**.

---

## Finding 2: TOCTOU Race in Default `pulse()` for `PciIntrPin`

**Severity: Medium**
**Type: Race Condition (TOCTOU)**
**Lines: 39-43 (trait default) consumed by PciIntrPin which has no override**

### Root Cause

`PciIntrPin` does **not** override `pulse()`, so it inherits the default:

```rust
fn pulse(&self) {
    if !self.is_asserted() {   // lock #1: acquire, check, release
        self.assert();          // lock #2: acquire, mutate, release
        self.deassert();        // lock #3: acquire, mutate, release
    }
}
```

Three separate lock acquisitions. Between step 1 and step 2, another thread can call `assert()`, setting `*asserted = true`. Then:
- `self.assert()` sees `*asserted == true`, skips the kernel ioctl (no-op).
- `self.deassert()` **unconditionally** (Finding 1) sets `*asserted = false` and fires a spurious deassert ioctl.

Net result: the pin is deasserted at the kernel level but was never asserted in this call. Combined with Finding 1, this corrupts kernel IRQ state.

Contrast with `LegacyPin::pulse()` (line 199) which correctly holds the lock for the entire operation.

---

## Finding 3: PIR Reprogramming Race — Stuck Interrupt

**Severity: Medium-High**
**Type: Race Condition**
**Lines: 284-301 (assert), 303-316 (deassert)**

### Root Cause

`PciIntrPin::assert()` and `deassert()` each independently read the PIR register via `atpic_irq()` to determine the ATPIC IRQ number. If guest firmware reprograms the PIR register between an assert and deassert:

```
Thread A: assert()  → reads PIR → atpic_irq = 5 → isa_assert_irq(5, 20)
Guest firmware: reprograms PIR[idx] from 5 to 9
Thread B: deassert() → reads PIR → atpic_irq = 9 → isa_deassert_irq(9, 20)
```

IRQ 5 is **asserted but never deasserted** at the kernel ATPIC. IRQ 9 gets a spurious deassert. The guest sees a **permanent interrupt storm on IRQ 5**.

### Exploitation Strategy (CTF)

1. Assign a PCI device to a slot that maps to a known PIRQ index.
2. From the guest, trigger the device to assert its interrupt.
3. Immediately reprogram the LPC bridge PIR register (PCI config write to offset 0x60-0x63).
4. Trigger the device to deassert.
5. IRQ is now stuck asserted. Guest takes an interrupt storm, consuming 100% CPU in the interrupt handler — **guest DoS**.

---

## Finding 4: `Entry::level` Overflow in Release Builds

**Severity: Low**
**Type: Integer Overflow**
**Lines: 81-83**

```rust
PinOp::Assert => {
    self.level += 1;   // wraps silently in --release
    self.level == 1
}
```

In release mode, `usize` overflow wraps to 0, then the next increment to 1 triggers a false "0→1 transition". Not practically exploitable on 64-bit (requires 2^64 operations), but represents a correctness gap. The `#![allow(clippy::mutex_atomic)]` annotation at line 13 hints the author was aware of some lint concerns but didn't address overflow.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | `PciIntrPin::deassert()` missing `if *asserted` guard | Medium | Yes — spurious kernel ioctl corrupts IRQ state |
| 2 | TOCTOU in default `pulse()` for PciIntrPin | Medium | Yes — race window + Finding 1 = state corruption |
| 3 | PIR reprogramming race → stuck interrupt | Medium-High | Yes — guest firmware can trigger permanent IRQ storm |
| 4 | `Entry::level` overflow in release | Low | Theoretical only on 64-bit |

**Most exploitable for CTF**: Finding 3 (PIR race). It's guest-triggerable, requires no privilege beyond PCI config space access (which any guest OS has), and produces a clear observable effect (interrupt storm / guest hang).
