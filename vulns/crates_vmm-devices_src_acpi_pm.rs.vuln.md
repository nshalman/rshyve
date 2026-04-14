# Vulnerability Analysis: `acpi_pm.rs`

## Executive Summary

This file is **relatively safe from memory corruption** due to Rust's safety guarantees and the `Mutex`-protected state. However, it contains several **logic bugs** ranging from medium to low severity. There are no critical exploitable vulnerabilities (no RCE, no VM escape), but there are bugs a CTF might flag.

---

## Finding 1: No-op S5 Shutdown and System Reset (Medium)

**Type:** Logic bug / Incomplete implementation  
**Lines:** 172-178 (S5), 212-213 (reset)

**Root cause:** Both the S5 power-off path and the 0xCF9 reset handler detect the guest's request but take **no action**:

```rust
// Line 174-178: S5 detected but nothing happens
if slp_type == 5 {
    // S5 = power off. Signal via SLP_EN write.
    // The vCPU run loop will see this as a suspend.
    // S5 = power off
}

// Line 212-213: Reset detected but nothing happens
if val & 0x04 != 0 {
    // System reset requested
}
```

**Impact:** A guest OS cannot shut down or reset the VM through the standard ACPI mechanism. The guest believes the operation succeeded while the VM continues running. In a multi-tenant hypervisor, this means a guest that should be powered off remains alive and potentially consuming resources.

**Exploitation:** A malicious guest that detects this behavior knows it cannot be force-stopped via ACPI, which could be used to persist through expected shutdown events.

---

## Finding 2: `handle_pm1_cnt` Ignores Offset (Medium)

**Type:** Incorrect I/O dispatch  
**Line:** 157

```rust
fn handle_pm1_cnt(&self, _offset: u16, rwo: RWOp<'_>) {
```

The PM1 Control block is registered as 2 bytes at `PMBASE+4` (line 102). The PIO bus can dispatch with offset 0 or 1. But the handler **ignores the offset entirely** and always processes the access as a full u16 at offset 0.

**Impact with byte-width access (`inb 0x405` / `outb 0x405`):**

- **Byte read at offset 1:** Guest gets the low byte of `pm1_control` but expects the high byte (which contains SLP_TYP bits 10-12). The guest reads stale/wrong register state.
- **Byte write at offset 1:** Guest intends to write to bits 8-15 (e.g., setting SLP_TYP) but the value is applied to bits 0-7 instead. The guest can inadvertently set unexpected bits in the control register.

I verified the `WriteOp::read_u16()` implementation reads `[buf[0], buf[1]]` unconditionally. For a 1-byte write, `buf[1]` is zero-initialized, so the guest byte becomes the low byte of a u16 regardless of the intended offset.

---

## Finding 3: Width-Agnostic I/O Handlers (Low)

**Type:** Access width mismatch  
**Lines:** 130, 134, 139, 143, 162, 165

All status/enable/control handlers call `write_u16()`/`read_u16()` without checking the actual access width. The PIO bus creates `ReadOp`/`WriteOp` with the guest's actual access size (1, 2, or 4 bytes). Confirmed from `vmm-core/src/common.rs`:

- `write_u16()` always writes 2 bytes to the internal 8-byte buffer, but `buf()` only returns `len` bytes
- `read_u16()` always reads `[buf[0], buf[1]]`, with `buf[1]` being zero for 1-byte writes

No memory safety issue (buffer is always 8 bytes), but a guest performing byte-width or dword-width I/O gets silently wrong results.

---

## Finding 4: Silently Discarded Registration Errors (Low)

**Type:** Ignored return value  
**Lines:** 94, 102, 110, 118

```rust
let _ = bus_pio.register(PMBASE, 4, evt_handler);
let _ = bus_pio.register(PMBASE + 4, 2, cnt_handler);
let _ = bus_pio.register(0xCF9, 1, rst_handler);
let _ = bus_pio.register(SMI_CMD_PORT, 1, smi_handler);
```

If any registration fails (port conflict with another device), the ACPI PM handlers silently don't get installed. The guest would get `0xFFFFFFFF` for reads and silent drops for writes -- no crash, but broken ACPI that's invisible to the VMM operator.

---

## Finding 5: PM1_TMR_STS and PM1_GBL_STS Not in Clearable Mask (Low)

**Type:** ACPI spec non-compliance  
**Lines:** 30, 32, 39-40

```rust
const PM1_TMR_STS: u16 = 0x0001;   // Defined...
const PM1_GBL_STS: u16 = 0x0020;   // Defined...

const PM1_STS_CLEARABLE: u16 =     // ...but NOT included here
    PM1_WAK_STS | PM1_RTC_STS | PM1_SLPBTN_STS | PM1_PWRBTN_STS | PM1_BM_STS;
```

Per ACPI spec, TMR_STS and GBL_STS are write-1-to-clear. Once set, they can never be cleared by the guest. Minor spec deviation.

---

## Finding 6: Doc Comment / Constant Mismatch (Informational)

**Lines:** 74-76 vs 20

Doc comments say PMBASE is `0xB000` but the constant is `0x400`. Misleading but not a vulnerability.

---

## Summary Table

| # | Finding | Severity | Exploitable? |
|---|---------|----------|-------------|
| 1 | No-op S5/reset handlers | Medium | Guest persistence through shutdown |
| 2 | `_offset` ignored in PM1 CNT | Medium | Misaligned register manipulation |
| 3 | Width-agnostic I/O | Low | Silent wrong values |
| 4 | Silenced registration errors | Low | Invisible broken ACPI |
| 5 | Non-clearable status bits | Low | Stuck interrupts |
| 6 | Doc/constant mismatch | Info | N/A |

**Bottom line:** Safe Rust and proper `Mutex` usage prevent memory corruption and data races. The bugs here are logic-level -- the most interesting for a CTF is **Finding 1** (no-op shutdown = guest can't be killed via ACPI) combined with **Finding 2** (offset-ignored byte access lets a guest manipulate PM1_CNT incorrectly). Neither enables VM escape or code execution.
