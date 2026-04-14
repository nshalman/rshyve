# Vulnerability Analysis: `crates/vmm-devices/src/aml.rs`

## Finding 1 — Silent Integer Truncation in Package Element Counts

**Severity: Medium**
**Type: Integer truncation / AML injection**

### Affected Functions

- `name_package` (line ~239): `pkg_body.push(elements.len() as u8)`
- `name_nested_packages` (line ~268): `outer_body.push(packages.len() as u8)` and `inner_body.push(elements.len() as u8)`
- `encode_name_string` (line ~173): `result.push(n as u8)` for multi-segment name paths

### Root Cause

The `as u8` cast silently truncates `usize` to 8 bits. If `elements.len() == 256`, the NumElements byte becomes `0`, but all 256 elements' data is still appended. The PkgLength is computed from the actual data size, so it correctly reflects the full content — but NumElements disagrees.

### What Happens in the Guest

The guest kernel's ACPI interpreter (ACPICA on Linux, ACPI.sys on Windows) reads:

1. `PackageOp` + `PkgLength` (correct, covers all data)
2. `NumElements = 0` (truncated from 256)
3. Interpreter sees 0 elements but PkgLength says there's data remaining

Depending on interpreter behavior:
- **PkgLength-authoritative interpreters** skip the orphaned element data — benign but wrong
- **NumElements-trusting interpreters** stop after 0 elements, then try to parse the remaining bytes (encoded `AmlValue` data) as subsequent AML opcodes — this is the exploitable path

### Exploitation Strategy

An attacker who controls device configuration (e.g., via a VMM management API or VM definition file) could craft a configuration that causes 256 or `256+N` elements in a `_PRT` (PCI Interrupt Routing Table) or similar ACPI package. The truncated count causes the interpreter to misparse trailing element data as AML opcodes.

By carefully choosing the element values (which the attacker controls through device configuration), the "orphaned" bytes can encode arbitrary AML — including `Method` definitions that run with kernel privilege during ACPI evaluation.

### PoC Sketch

```rust
// Attacker controls element count via device config
let mut evil_elements: Vec<AmlValue> = Vec::new();
for _ in 0..256 {
    // Byte values chosen so that when interpreted as AML opcodes
    // they form: Method(EVIL, 0) { <shellcode> }
    evil_elements.push(AmlValue::Byte(0x14)); // METHOD_OP
}
// NumElements wraps to 0, but all 256 elements are emitted
// Guest ACPI interpreter sees them as top-level AML after the "empty" package
aml.name_package("_PRT", &evil_elements);
```

### Fix

```rust
assert!(elements.len() <= 255, "AML Package supports at most 255 elements");
```

Or use `u8::try_from(elements.len()).expect(...)`.

---

## Finding 2 — PkgLength Encoding Off-By-One

**Severity: Low**
**Type: Spec deviation / suboptimal encoding**

### Affected Functions

- `encode_pkg_length` (line ~108): boundary `< 63` should be `< 64` (single byte encodes 0–63 inclusive per ACPI spec §20.2.4)
- Same pattern at `< 0xFFF` (should be `<= 0xFFF`) and `< 0xFFFFF`

### Root Cause

The single-byte PkgLength uses bits [5:0], encoding values 0–63. The code uses `length < 63`, excluding 63 and forcing it into 2-byte encoding. Same off-by-one at every boundary.

### Impact

**Not directly exploitable.** The two functions (`pkg_length_size` and `encode_pkg_length`) have *matching* off-by-ones, so the PkgLength value is always consistent with the actual byte count. The encoding is suboptimal (1 extra byte at boundaries) but decodes correctly.

However, some strict ACPI validators or fuzz-tested interpreters might flag the non-canonical encoding, and future refactoring that fixes one function but not the other would create a PkgLength mismatch — a far more dangerous bug.

---

## Finding 3 — Multi-Name Segment Count Truncation

**Severity: Low**
**Type: Integer truncation**

### Affected Function

`encode_name_string` (line ~173):

```rust
n => {
    result.push(AML_MULTI_NAME_PREFIX);
    result.push(n as u8);  // truncation if n > 255
    for seg in &segments { ... }
}
```

### Impact

A name path with 256+ dot-separated segments would have SegCount = 0 (mod 256), but 256+ four-byte name segments would follow. The interpreter would read 0 segments and misparse the 1024+ bytes of name data as AML opcodes.

Requires a contrived 256-segment path, so low practical risk.

---

## Finding 4 — No Name Character Validation

**Severity: Informational**

`encode_name_seg` verifies length (1–4 chars) but not character validity. AML NameSegs must be `[A-Z0-9_]`. Passing lowercase, special characters, or non-ASCII bytes produces non-spec-compliant AML that may confuse interpreters or bypass ACPI signature checks.

---

## Summary

| # | Issue | Severity | Exploitable? |
|---|-------|----------|-------------|
| 1 | Package element count `as u8` truncation | **Medium** | Yes — if attacker controls element count, can inject AML opcodes parsed with kernel privilege |
| 2 | PkgLength off-by-one boundaries | Low | No — matched in both functions, functionally correct |
| 3 | Name segment count `as u8` truncation | Low | Theoretical — requires 256+ path segments |
| 4 | Missing name character validation | Info | Spec violation, not memory corruption |

The code is written in safe Rust with no `unsafe` blocks, so classic memory corruption (buffer overflow, use-after-free) is not possible. The primary risk surface is **logic bugs that produce malformed AML bytecode**, which the guest OS kernel interprets with full privilege. Finding 1 is the most viable attack vector.
