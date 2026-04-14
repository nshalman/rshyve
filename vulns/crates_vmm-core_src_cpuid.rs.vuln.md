# Vulnerability Report: `cpuid.rs` — XSAVE Area Size Mismatch

## Vulnerability 1: XSAVE Buffer Size Underreport (High Severity)

**Type:** Logic bug leading to guest memory corruption (buffer overflow)
**Affected function:** `build_cpuid_entries()`, lines ~155-190 (Leaf 0xD handling)

### Root Cause

The XSAVE state component mask (`eax_d0`) only clears AVX-512 (bits 5,6,7) and AMX (bits 17,18), but **does not clear other extended state components** that may be present on the host:

| XCR0 Bit | Component | Offset | Size | Masked? |
|----------|-----------|--------|------|---------|
| 2 | AVX YMM_Hi128 | 576 | 256 | Only in Sse42 |
| 3 | MPX BNDREGS | 960 | 64 | **NO** |
| 4 | MPX BNDCSR | 1024 | 64 | **NO** |
| 9 | PKRU | 2688 | 8 | **NO** |
| 11 | CET_U | varies | varies | **NO** |
| 12 | CET_S | varies | varies | **NO** |

Meanwhile, the XSAVE area size is hardcoded:

```rust
// Line ~172-178
let mut xsave_size: u32 = 576; // x87 + SSE
if eax_d0 & (1 << 2) != 0 {
    xsave_size = 832; // + AVX YMM hi128
}
ebx_d0 = xsave_size;  // current XSAVE size
ecx_d0 = xsave_size;  // max XSAVE size
```

But the subleaf enumeration loop (lines ~185-190) **still emits entries for the unmasked components**:

```rust
for bit in 2..32u32 {
    if eax_d0 & (1 << bit) == 0 { continue; }  // PKRU bit 9 is still set!
    let (a, b, c, d) = host_cpuid(0xD, bit);
    if a == 0 { continue; }
    entries.push(entry(0xD, bit, VCE_FLAG_MATCH_INDEX, a, b, c, d));
}
```

### Inconsistent CPUID State Presented to Guest

On a Skylake-SP or later host with PKRU (common), the guest sees:

- **Leaf 0xD, subleaf 0:** `EAX` bit 9 set (PKRU available), `EBX=ECX=832`
- **Leaf 0xD, subleaf 9:** offset=2688, size=8

The guest kernel enables PKRU in XCR0, allocates XSAVE buffers of 832 bytes (trusting `ECX`), then XSAVE writes PKRU state at offset 2688 — **1,856 bytes past the buffer end**.

### Exploitation Strategy

1. Boot a VM with `--cpu-baseline avx2` on a host with PKRU support
2. Guest kernel reads CPUID leaf 0xD subleaf 0, sees PKRU available, `max_size=832`
3. Guest allocates 832-byte XSAVE buffers (e.g., in `task_struct` for context switching)
4. Guest enables PKRU in XCR0
5. Every context switch does XSAVE, writing at offset 2688 into an 832-byte buffer
6. **Adjacent kernel memory corrupted** — heap metadata, adjacent `task_struct` fields, etc.
7. Craft a controlled PKRU value to overwrite a specific target for privilege escalation

For a VMM escape angle: if the VMM itself uses the reported XSAVE size during live migration to allocate buffers for serializing vCPU state, the same overflow occurs in the VMM process's address space.

### Fix

Either mask all unhandled XCR0 bits, or compute the XSAVE size dynamically:

```rust
// Whitelist only the components this baseline supports
let allowed_bits: u32 = match baseline {
    CpuBaseline::Avx2 => 0x7,   // bits 0,1,2 only
    CpuBaseline::Sse42 => 0x3,  // bits 0,1 only
    _ => unreachable!(),
};
eax_d0 &= allowed_bits;
```

---

## Vulnerability 2: `u32` to `i32` Cast Bypass (Low Severity)

**Affected function:** `apply_cpuid_baseline()`, line ~217

```rust
for vcpuid in 0..num_cpus as i32 {
```

If `num_cpus > i32::MAX` (0x80000000+), the cast wraps to a negative value, the range is empty, and **no vCPU gets the CPUID baseline applied**. The VM runs with unmasked host CPUID, defeating migration safety. Unrealistic in practice but a correctness bug.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|--------------|----------|-------------|
| 1 | XSAVE size underreport → guest buffer overflow | **High** | Yes — guest kernel memory corruption, potential privesc or VMM escape during migration |
| 2 | `u32→i32` cast skips all vCPUs | Low | Only with absurd `num_cpus` |

The primary finding (XSAVE mismatch) is the CTF target. The code correctly masks feature *advertisement* bits in leaves 1 and 7, but fails to reconcile the XSAVE state area (leaf 0xD) — advertising components whose offsets extend far beyond the reported buffer size.
