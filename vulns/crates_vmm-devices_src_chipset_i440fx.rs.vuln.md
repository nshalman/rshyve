# Vulnerability Report: `i440fx.rs`

## Finding 1: PIR Register Desync on Multi-Byte Write (Logic Bug)

**Severity: High**
**Type: Logic bug / Security bypass**
**Affected: `LpcBridge::cfg_write`, lines ~243-253**

### Root Cause

The PIR write handler only intercepts **single-byte** writes (`len == 1`). Multi-byte writes (2 or 4 bytes) that overlap the PIR range (0x60–0x63) fall through to the generic `DeviceState::cfg_write`, which updates the generic config space buffer but **not** `pir_regs`.

```rust
// Only catches len == 1:
if offset >= PIR_OFFSET && offset < PIR_OFFSET + PIR_COUNT as u8 && len == 1 {
    // updates pir_regs
}
// else → DeviceState::cfg_write (does NOT update pir_regs)
```

Meanwhile, `cfg_read` intercepts PIR reads for **any** length — reading from `pir_regs`:

```rust
// No len check here:
if offset >= PIR_OFFSET && offset < PIR_OFFSET + PIR_COUNT as u8 {
    return u32::from(pir[idx]);  // reads from pir_regs
}
```

### Exploitation

`PciIntrPin` instances hold an `Arc` clone of `pir_regs` and read it at interrupt assertion time. A guest can desync the routing:

1. **Write PIR[0]=0x0A via 1-byte write** → `pir_regs[0]` = 0x0A. Interrupts route to IRQ 10.
2. **Write all 4 PIR regs via 4-byte write at 0x60** → goes to `DeviceState` only. `pir_regs` still has 0x0A.
3. Guest reads back PIR[0] — gets 0x0A from `pir_regs`, thinks nothing changed.
4. But any subsequent 1-byte read from a **debugger/monitor** reading `DeviceState` sees the dword-written value.

The attacker controls interrupt routing independently of what the config space reports — **interrupts route to an IRQ the guest didn't explicitly configure**, or the guest can lock in a routing that appears overwritten but persists.

### Impact

In a CTF VMM escape scenario: manipulate interrupt routing to redirect device interrupts to unexpected ISA IRQ lines, potentially triggering handlers for the wrong device or causing interrupt storms.

---

## Finding 2: No PIR Value Validation (Missing Mask)

**Severity: Medium**
**Type: Missing input validation**
**Affected: `LpcBridge::cfg_write`, line ~249**

### Root Cause

The code defines masks but never applies them:

```rust
const PIR_MASK_DISABLE: u8 = 0x80;  // bit 7 = disable
const PIR_MASK_IRQ: u8 = 0x0F;      // bits 3:0 = IRQ number
```

But the write path stores the raw byte unmasked:

```rust
pir[idx] = byte;  // no masking — bits 6:4 are reserved but accepted
```

A guest can write arbitrary values including reserved bit patterns. When `PciIntrPin` reads this and extracts an IRQ number, bits 6:4 (which should be zero) could produce an IRQ number > 15 if the consumer doesn't mask properly.

---

## Finding 3: Config Address TOCTOU Race

**Severity: Medium**
**Type: Race condition (TOCTOU)**
**Affected: `pio_cfg_data_handler` + `PioCfgDecoder`, lines ~153-175**

### Root Cause

PCI config access is a two-step operation: write address to 0xCF8, then read/write data at 0xCFC. The `PioCfgDecoder` latches the address. With multiple vCPUs:

1. vCPU-0 writes 0xCF8 targeting device A
2. vCPU-1 writes 0xCF8 targeting device B (overwrites latch)
3. vCPU-0 reads 0xCFC — **accesses device B instead of A**

This is a classic PCI config TOCTOU. Without seeing `PioCfgDecoder`'s internals, it's unclear if there's internal locking. If the latch is a single shared `AtomicU32` (common pattern), this race is exploitable.

### Impact

Read or write config space of an arbitrary PCI device, bypassing intended access. A malicious guest driver racing against firmware could modify config space of a device it shouldn't control.

---

## Finding 4: Potential OOB Read via `wo.read_u32()` on Sub-Dword Write

**Severity: Medium (depends on `RWOp` implementation)**
**Type: Out-of-bounds read / Information leak**
**Affected: `pio_cfg_data_handler`, line ~170**

```rust
RWOp::Write(wo) => {
    bus.config_write(
        bdf,
        offset,
        wo.len() as u8,
        wo.read_u32(),  // always reads 4 bytes regardless of wo.len()
    );
}
```

If the guest does a 1-byte `OUT` to port 0xCFC, `wo.len()` is 1. If `read_u32()` reads 4 bytes from a 1-byte backing buffer without zero-extension, this is an OOB read of 3 adjacent bytes. The value is passed to `config_write` with `len=1` so the extra bytes are likely discarded — but the read itself may touch unmapped memory.

Symmetric issue on reads: the `_ => ro.write_u32(val)` default writes 4 bytes regardless of actual buffer size.

---

## Summary

| # | Vulnerability | Severity | Exploitable from guest? |
|---|---|---|---|
| 1 | PIR desync on multi-byte write | **High** | Yes — controls interrupt routing invisibly |
| 2 | Missing PIR value mask | Medium | Yes — write invalid IRQ numbers |
| 3 | Config address TOCTOU | Medium | Yes — with multi-vCPU race |
| 4 | `read_u32()` on short write | Medium | Depends on `RWOp` impl |

**The PIR desync (Finding 1) is the most clearly exploitable from the code provided.** The fix is to handle multi-byte writes to 0x60–0x63 by extracting individual bytes and updating `pir_regs`, or to reject non-byte-width PIR writes explicitly.
