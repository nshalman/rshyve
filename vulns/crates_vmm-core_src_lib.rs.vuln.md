Now I have enough to write a definitive report. Here are the confirmed vulnerabilities:

---

# VMM Security Vulnerability Report

## Finding 1: RIP-Relative Address Miscalculation (HIGH - Most Exploitable)

**File:** `crates/vmm-core/src/inst_emul.rs:504-506`  
**Type:** Logic bug leading to incorrect guest memory access  
**The developer even knew it was wrong:**

```rust
// RIP-relative: addr = RIP + inst_len + disp
// But inst_len isn't fully known yet... use current pos   <-- ACKNOWLEDGED BUG
let addr = self.rip
    .wrapping_add(self.pos as u64)        // uses PARTIAL parse position
    .wrapping_add(disp as i64 as u64);
```

**Root cause:** RIP-relative addressing in x86-64 computes `addr = RIP + full_instruction_length + displacement`. But the decoder uses `self.pos` (bytes consumed *so far*) instead of the final instruction length. If there are immediate bytes or other operands remaining after the ModR/M+SIB+disp32 decoding, `self.pos` is too small.

**Exploitation:** A guest crafts an instruction with RIP-relative addressing where the opcode has additional bytes after the displacement (e.g., an immediate operand). The computed effective address is off by `(final_inst_len - self.pos)` bytes, causing:
- MMIO reads/writes to wrong device registers
- Guest memory reads/writes to adjacent pages
- If the miscomputed address crosses a RAM/MMIO boundary, a RAM write intended for one location hits an MMIO device, or vice versa

**PoC sketch:** A MOV instruction like `mov [rip+0x1000], imm32` at a specific RIP. The displacement is consumed, but the 4-byte immediate hasn't been parsed yet, so the effective address is 4 bytes short of the correct target.

---

## Finding 2: Integer Overflow in SIB Index Scaling (HIGH)

**File:** `crates/vmm-core/src/inst_emul.rs:590`

```rust
let index_val = if index == 0x04 {
    0
} else {
    self.read_gpr(index_ext).ok()? * scale   // <-- wrapping mul in release
};
```

**Root cause:** Regular `*` operator, not `wrapping_mul` or `checked_mul`. In release mode, this silently wraps on overflow. A guest sets a register like RSI to `0xFFFF_FFFF_FFFF_FFFF` and uses `[base + RSI*8]` addressing.

**Result:** `0xFFFF_FFFF_FFFF_FFFF * 8` wraps to `0xFFFF_FFFF_FFFF_FFF8`, producing an effective address that's completely different from the intended one. This lets a guest access arbitrary guest physical addresses via the instruction emulator, bypassing any address-based MMIO routing.

---

## Finding 3: Guest-Triggered Panic via PIO/MMIO Byte Width (MEDIUM)

**File:** `crates/vmm-core/src/pio.rs:114` and `crates/vmm-core/src/mmio.rs:103`

```rust
// pio.rs - val is u32, to_le_bytes() returns [u8; 4]
let data = &val.to_le_bytes()[..bytes as usize];   // panics if bytes > 4

// mmio.rs - val is u64, to_le_bytes() returns [u8; 8]  
let data = &val.to_le_bytes()[..bytes as usize];   // panics if bytes > 8
```

**Root cause:** `bytes` is `u8` (range 0-255) but no validation before slicing a 4- or 8-byte array. If the VM exit structure provides `bytes=0`, `WriteOp::from_buf` panics on empty slice. If `bytes > 4` (PIO) or `bytes > 8` (MMIO), the array slice panics.

**Impact:** Denial of service — a malicious guest can crash the entire VMM process by triggering an I/O exit with an invalid byte width. Whether `bytes` is guest-controlled depends on how the kernel populates the VM exit info, but on bhyve the instruction length/operand size fields come from hardware decode which can be influenced by crafted instructions.

---

## Finding 4: SIB base=5 Mishandled Without mod Context (MEDIUM)

**File:** `crates/vmm-core/src/inst_emul.rs:594-610`

```rust
fn decode_sib(&mut self, _disp_size: usize) -> Option<(u64, bool)> {
    // ...
    // Base of 5 with mod=00 means disp32 (no base register)
    let base_val = if base == 0x05 {
        // This depends on mod field (already consumed) — for mod=00,
        // base=5 means [disp32 + index*scale]. Read disp32.
```

The `_disp_size` parameter (which should carry the `mod` field) is **completely unused** — note the `_` prefix. The SIB special case for base=5 has different meanings depending on the ModR/M mod field:
- **mod=00**: `[disp32 + index*scale]` (no base register)
- **mod=01**: `[RBP + disp8 + index*scale]`
- **mod=10**: `[RBP + disp32 + index*scale]`

The code unconditionally reads a disp32 when base=5, which is incorrect for mod=01 and mod=10. This causes double-displacement reads and wrong effective addresses for any instruction using `[RBP + index*scale + disp8/disp32]`.

**Exploitation:** Guest code using RBP as a base register with SIB addressing will compute wrong addresses, potentially accessing wrong memory regions.

---

## Finding 5: Unvalidated `data.len() as u8` Truncation (LOW)

**File:** `crates/vmm-core/src/inst_emul.rs:654,671`

```rust
mmio_bus.handle_write(addr, data.len() as u8, val);
let val = mmio_bus.handle_read(addr, buf.len() as u8);
```

If `data.len() > 255` (unlikely in current code paths but possible with future changes), the `as u8` cast silently truncates, sending a wrong byte count to the MMIO handler.

---

## Summary

| # | Vulnerability | File:Line | Severity | Guest-Triggerable |
|---|---|---|---|---|
| 1 | RIP-relative addr off by remaining inst bytes | inst_emul.rs:504 | **HIGH** | Yes |
| 2 | SIB index*scale wrapping overflow | inst_emul.rs:590 | **HIGH** | Yes |
| 3 | PIO/MMIO panic on invalid byte width | pio.rs:114, mmio.rs:103 | **MEDIUM** | Likely |
| 4 | SIB base=5 ignores mod field | inst_emul.rs:594 | **MEDIUM** | Yes |
| 5 | data.len() as u8 truncation | inst_emul.rs:654 | **LOW** | No |

**Most exploitable for CTF:** Finding 1 (RIP-relative miscalculation). The developer acknowledged the bug in a comment. Craft a guest instruction with RIP-relative addressing that has trailing immediate bytes — the emulator computes the wrong effective address, letting you redirect MMIO accesses to unintended device registers or memory regions.
