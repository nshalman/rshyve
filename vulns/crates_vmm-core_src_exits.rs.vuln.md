# Vulnerability Analysis: `exits.rs`

## Finding 1: Unchecked `num_valid` in `InstEmul` — Out-of-Bounds Index (Medium)

**Affected**: `VmExitKind::parse()`, `VM_EXITCODE_INST_EMUL` arm (~line 191)

```rust
vm_exitcode::VM_EXITCODE_INST_EMUL => {
    let inst = unsafe { &exit.u.inst_emul };
    VmExitKind::InstEmul {
        inst: inst.inst,       // [u8; 15]
        num_valid: inst.num_valid, // u8, NOT clamped to 15
    }
}
```

**Root cause**: `num_valid` is copied verbatim from the kernel struct with no bounds check against the 15-byte `inst` array. Any consumer that does `&exit.inst[..num_valid as usize]` will panic if `num_valid > 15`, or worse — if the consumer uses unsafe indexing, it's a buffer overread.

**Exploitation**: A crafted guest can trigger an `INST_EMUL` exit. If the kernel-side struct's `num_valid` field is influenced by guest state (e.g., instruction prefix stuffing that confuses the decoder), a value > 15 reaches userspace unchecked. This is a **guest-triggered VMM process crash (DoS)**. If any downstream consumer reads `inst[..num_valid]` in an `unsafe` block, it becomes an info leak of adjacent stack/heap data.

**Fix**: Clamp after parsing:
```rust
num_valid: inst.num_valid.min(15),
```

---

## Finding 2: Integer Truncation of `inst_length` — Infinite vCPU Loop (Medium)

**Affected**: `VmExit::parse()` (~line 47)

```rust
inst_len: exit.inst_length as u8,
```

**Root cause**: `exit.inst_length` is likely `u32` or `i32` in the kernel struct. The `as u8` cast silently truncates. Value 256 becomes 0, 257 becomes 1, etc.

**Exploitation**: If `inst_length` is ever > 255 (malformed kernel response, or API version mismatch where the field semantics changed), the truncation to 0 means the VMM advances RIP by 0 bytes after handling the exit. The guest re-executes the same instruction, triggers the same exit, and the VMM enters an **infinite busy loop** consuming 100% CPU on that vCPU thread. This is a guest-triggered DoS against the host VMM process.

**Fix**: Validate and clamp:
```rust
inst_len: exit.inst_length.min(15) as u8, // x86 max is 15
```

---

## Finding 3: `bytes` Field Passed to Kernel Without Validation (Low)

**Affected**: `VmEntry::to_raw()`, both `InoutFulfill` and `MmioFulfill` paths

```rust
payload.inout.bytes = io.bytes;  // No check that bytes ∈ {1, 2, 4}
payload.mmio.bytes = bytes;      // No check that bytes ∈ {1, 2, 4, 8}
```

**Root cause**: The `bytes` field from the parsed exit is echoed back to the kernel in the entry struct without validating it against legal x86 access widths. If a malformed exit provides `bytes = 0` or `bytes = 255`, this invalid value is passed into the kernel ioctl.

**Impact**: Kernel-dependent. Could cause kernel-side buffer over-read/write when the kernel uses `bytes` to determine how much data to copy. Low severity because the kernel should validate this itself, but defense-in-depth is missing.

---

## Finding 4: Signed-to-Unsigned Cast in `exit_code()` (Low/Informational)

**Affected**: `VmExitKind::exit_code()` (~line 149)

```rust
Self::Unknown(c) => *c as u32,
```

An `Unknown(-1)` produces `exit_code() = 0xFFFFFFFF`. If this feeds into a DTrace probe ID, array index, or metrics counter, it could cause unexpected behavior. Minor, but worth noting for CTF completeness.

---

## Overall Assessment

This is well-written Rust code. The `unsafe` blocks are minimal, justified, and limited to union field access. Rust's type system prevents classic memory corruption. The vulnerabilities found are **logic bugs** (missing input validation) rather than memory safety violations. The most exploitable issue for a CTF is **Finding 1** — if you can find a consumer of `InstEmul` that indexes `inst[..num_valid]` without a bounds check (especially in `unsafe`), that's your entry point for a guest-to-VMM escalation. Finding 2 is the easiest to trigger: a guest-caused infinite loop is a straightforward DoS.
