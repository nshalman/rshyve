# Vulnerability Analysis: `inst_emul.rs`

## Vulnerability 1: SIB Base=5 Address Confusion (Critical)

**Affected function:** `decode_sib()` (line ~380) and `decode_modrm_addr()` (lines ~310-370)

**Root cause:** The SIB decoder always treats `base==5` as `[disp32 + index*scale]` (the mod=00 encoding), but for mod=01 and mod=10, base=5 should mean RBP is the base register. The `_disp_size` parameter was clearly intended to distinguish these cases but is ignored — the caller always passes `0`.

**What happens for mod=01, rm=100, SIB base=5 (e.g., `MOV [rbp+0x10], rax` with SIB):**

1. `decode_modrm_addr` enters the mod=01 branch, calls `decode_sib(0)`
2. `decode_sib` reads the SIB byte, sees base=5, **incorrectly** reads 4 bytes as disp32 instead of using RBP
3. Back in `decode_modrm_addr`, it reads **another** byte as disp8
4. Result: 5 bytes consumed instead of 1; the computed address is based on garbage bytes from the instruction stream instead of RBP

**Exploitation:** A guest crafts instruction bytes near an MMIO region that use SIB addressing with RBP base + disp8. The emulator computes a completely wrong target address (attacker-controlled via the instruction bytes it misinterprets as disp32). This redirects an MMIO write/read to an arbitrary guest physical address — potentially hitting a different device's register space or guest RAM.

**Proof of concept encoding:**
```
48 89 44 2D 10
```
This is `MOV [rbp + rbp*1 + 0x10], rax` (REX.W, opcode 0x89, ModRM=0x44 mod=01/reg=0/rm=100, SIB=0x2D base=5/index=5/scale=1, disp8=0x10).

The emulator will:
- Read bytes `10 XX XX XX` as disp32 in `decode_sib` (consuming the disp8 + 3 following bytes)
- Then try to read yet another byte as the "real" disp8
- Compute: `addr = disp32_garbage + rbp*1 + next_byte` instead of `addr = rbp + rbp*1 + 0x10`

---

## Vulnerability 2: Unchecked `num_valid` — Guest-Triggered VMM Panic (High)

**Affected function:** `emulate_inst()` (line ~51)

```rust
let bytes = &inst[..num_valid as usize];
```

`inst` is `&[u8; 15]` but `num_valid` is `u8` (0–255). If `num_valid > 15`, this panics with a slice out-of-bounds. Rust's bounds check prevents memory corruption but **the VMM process crashes**, killing the guest and potentially all guests if the VMM is shared.

**Exploitation:** If the kernel-to-userspace interface allows a malformed `num_valid` (e.g., via a kernel bug, or if the value is sourced from guest-writable memory), the guest crashes the VMM.

---

## Vulnerability 3: REX Prefix Ordering — Operand Size Mismatch (High)

**Affected function:** `decode_and_emulate()` prefix parsing loop (lines ~130-165)

**Root cause:** On real x86-64, a legacy prefix (0x66, 0x67, segment override) appearing **after** a REX prefix voids the REX. The emulator's prefix loop doesn't enforce ordering — it accumulates all prefix state regardless of position.

**Concrete scenario:** Guest encodes instruction bytes `48 66 89 ...` (REX.W, then 0x66, then MOV r/m, r).

| | Real CPU | Emulator |
|---|---|---|
| REX voided by 0x66? | Yes | No |
| Operand size | 2 bytes | 8 bytes |

The emulator does an **8-byte MMIO write** when the real instruction would do a 2-byte write. This overwrites 6 adjacent bytes of MMIO register space with register data, potentially corrupting device state (PCI config, virtio registers, etc.).

---

## Vulnerability 4: mod=11 Mishandled as Memory Access (Medium)

**Affected function:** `emulate_mov_modrm()` (line ~218)

```rust
let (addr, _) = match self.decode_modrm_addr(modrm) {
```

The `is_register_operand` flag is discarded (`_`). For mod=0b11, `decode_modrm_addr` returns the register's **value** as `addr`, then the MOV handlers use it as a **memory address** for a physmap/MMIO write. This converts a register-to-register MOV into an arbitrary guest-memory write.

**Triggering:** Requires mod=0b11 instruction bytes to reach the emulator, which is unusual since reg-to-reg MOVs don't access memory (so they shouldn't cause NPT faults). Exploitability depends on whether the kernel can pass such bytes.

---

## Vulnerability 5: RIP-Relative Addressing Off-by-Immediate (Medium)

**Affected function:** `decode_modrm_addr()`, mod=0b00, rm=0b101 branch (line ~325)

```rust
// RIP-relative: addr = RIP + inst_len + disp
// But inst_len isn't fully known yet... use current pos
let addr = self.rip
    .wrapping_add(self.pos as u64)
    .wrapping_add(disp as i64 as u64);
```

The comment admits the bug. For `MOV [rip+disp32], imm32` (opcode 0xC7), the immediate hasn't been consumed when this runs, so `self.pos` doesn't include it. RIP-relative is defined relative to the **end** of the instruction. The computed address is off by the immediate size (4 bytes), causing writes to the wrong MMIO address.

---

## Summary

| # | Vulnerability | Severity | Guest-Triggerable | Impact |
|---|---|---|---|---|
| 1 | SIB base=5 address confusion | **Critical** | Yes — craft instruction bytes | Arbitrary MMIO/memory address redirect |
| 2 | `num_valid` unchecked | **High** | Likely | VMM panic (DoS) |
| 3 | REX prefix ordering | **High** | Yes — craft prefix sequence | Oversized MMIO access, device corruption |
| 4 | mod=11 discarded | **Medium** | Unlikely | Reg-to-reg becomes arbitrary write |
| 5 | RIP-relative off-by-imm | **Medium** | Yes | Wrong MMIO target address |

**Vuln #1 (SIB base=5) is the most exploitable** — the guest fully controls the instruction bytes that the emulator misparses, giving direct control over the target address of MMIO reads/writes.
