Now I have the full picture. Here's the vulnerability report:

---

# Vulnerability Report: `vcpu_tasks.rs` + `inst_emul.rs`

## Vuln 1: SIB Decode Bug — Wrong Address for `base=5` with `mod!=00` (HIGH)

**Affected:** `inst_emul.rs:569-613` (`decode_sib`) called from `decode_modrm_addr` lines 513-563

**Root cause:** `decode_sib()` unconditionally treats SIB `base=5` as "no base register + disp32", but this encoding is only correct when `mod=00`. For `mod=01` and `mod=10`, SIB `base=5` means RBP as the base register. The `_disp_size` parameter was intended to disambiguate but is unused (dead code smell).

**What goes wrong:**

For an instruction like `MOV [RBP + RSI*4 + disp8], RAX` (mod=01, SIB, base=5):
1. `decode_sib()` reads 4 bytes as a phantom disp32 instead of using RBP
2. The caller then reads 1 more byte as disp8
3. Effective address = **garbage disp32 + index*scale + disp8** instead of **RBP + index*scale + disp8**
4. `advance_rip()` uses `self.pos` which consumed 4 extra bytes, so **RIP jumps 4 bytes too far**

**Exploitation:** A guest crafts an instruction with SIB base=5 and mod=01 or mod=10 targeting MMIO. The emulator:
- **Writes to / reads from a completely wrong MMIO address** (controlled by instruction bytes that get misinterpreted as disp32)
- **Skips 4 bytes of guest code**, allowing the guest to redirect execution by planting code at the "skipped-to" offset

```
; Guest exploit: target MMIO at 0xFEB00000 (virtio cfg space)
; Actual write goes to address derived from instruction bytes, not RBP
mov byte [rbp + rsi*1 + 0x10], 0xFF   ; mod=01, SIB base=5
; Emulator computes addr from bytes[pos..pos+4] instead of RBP
; Also skips 4 bytes of code after this instruction
```

**Impact:** Guest-controlled write to arbitrary MMIO offset. If device registers at the misdirected address control DMA or interrupt routing, this is a guest-to-host escalation primitive.

---

## Vuln 2: RIP-Relative Address Off-by-N for Instructions with Immediates (HIGH)

**Affected:** `inst_emul.rs:490-507` (inside `decode_modrm_addr`, `rm=0b101` case)

**Root cause:** RIP-relative addressing computes `addr = RIP + self.pos + disp32`, but `self.pos` only reflects bytes consumed *so far* (up through the displacement). For instructions with trailing immediates (e.g., `MOV [RIP+disp32], imm32`), the correct formula is `addr = RIP + total_inst_len + disp32`.

```rust
// inst_emul.rs:504-506 — the bug
let addr = self.rip
    .wrapping_add(self.pos as u64)      // pos doesn't include the immediate yet!
    .wrapping_add(disp as i64 as u64);
```

**What goes wrong with `MOV DWORD [RIP+disp], imm32` (0xC7):**

1. `emulate_mov_imm` calls `decode_modrm_addr` — address calculated with `self.pos` *before* the 4-byte immediate is consumed
2. The immediate is consumed afterward
3. The write goes to `addr - 4` (off by the immediate size)
4. `advance_rip()` is correct because `self.pos` is fully updated by then

**Exploitation:** Guest targets MMIO register X with a RIP-relative MOV. The VMM writes to **X - 4** instead. If adjacent device registers have different privilege levels (e.g., writing 4 bytes before a virtio `QueueNotify` register hits `QueueSelect`, or 4 bytes before PCI BAR config hits the command register), the guest accesses the wrong register.

```
; Guest writes 0xDEADBEEF to what it thinks is MMIO addr A
; VMM actually writes to A - 4
mov dword [rip + calculated_disp], 0xDEADBEEF
```

---

## Vuln 3: HLT Exit Spin Loop — Guest-to-Host CPU DoS (MEDIUM)

**Affected:** `vcpu_tasks.rs:246-251`

```rust
VmExitKind::Hlt => {
    // If we get here, just re-enter.
    VmEntry::Run
}
```

**Root cause:** When HLT exits reach userspace (pre-API v6 or misconfigured), the VMM immediately re-enters without sleeping or waiting for an interrupt. A guest in a HLT loop causes the host vCPU thread to busy-spin at 100% CPU.

**Exploitation:** `cli; .loop: hlt; jmp .loop` from the guest pins the host CPU thread. With N vCPUs all doing this, N host cores are consumed. Simple DoS, no preconditions.

---

## Vuln 4: Unchecked `num_valid` — VMM Crash (MEDIUM)

**Affected:** `inst_emul.rs:52` and `vcpu_tasks.rs:211`

```rust
let bytes = &inst[..num_valid as usize];  // panics if num_valid > 15
```

**Root cause:** `num_valid` is `u8` (0-255) from the kernel `vm_inst_emul` struct, but `inst` is `[u8; 15]`. No bounds check anywhere in the path from `exits.rs:269` through `vcpu_tasks.rs:201` to `inst_emul.rs:52`. If the kernel ever provides `num_valid > 15` (kernel bug, struct versioning mismatch, or corrupted memory), the VMM panics and the VM crashes.

---

## Vuln 5: RDMSR Silent Error Suppression (LOW)

**Affected:** `vcpu_tasks.rs:172-178`

```rust
let _ = vcpu.set_reg(VM_REG_GUEST_RAX, 0);
let _ = vcpu.set_reg(VM_REG_GUEST_RDX, 0);
```

Both `set_reg` errors are discarded. If one succeeds and the other fails, the guest sees a partial result (one register zeroed, the other stale). The MSR `code` is also completely ignored — all MSRs are handled identically with no allowlist. Not directly exploitable but weakens defense-in-depth.

---

## Summary

| # | Vulnerability | Severity | Type | Exploitable From Guest? |
|---|---|---|---|---|
| 1 | SIB base=5 decode bug | **High** | Wrong MMIO address + RIP skip | Yes |
| 2 | RIP-relative off-by-N | **High** | Wrong MMIO address | Yes |
| 3 | HLT spin loop | Medium | Host CPU DoS | Yes |
| 4 | Unchecked num_valid | Medium | VMM crash (panic) | Indirectly |
| 5 | RDMSR error suppression | Low | Inconsistent state | Marginal |

**For the CTF, focus on Vulns 1 and 2** — both let a guest cause the VMM instruction emulator to read/write the wrong MMIO device register. The SIB bug (Vuln 1) is the more powerful primitive because the address offset is fully controlled by the guest through the instruction bytes that get misinterpreted as disp32.
