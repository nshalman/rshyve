# Vulnerability Analysis: `acpi.rs`

## Finding 1: Integer Truncation in MADT APIC IDs (Medium)

**Affected function:** `build_madt()`, lines within the LAPIC entry loop

**Root cause:** The loop variable `i` (type `u32`) is truncated to `u8` for both the Processor ID and APIC ID fields:

```rust
for i in 0..num_cpus {
    buf.push(MADT_TYPE_LOCAL_APIC);
    buf.push(8);
    buf.push(i as u8);    // processor ID — truncated!
    buf.push(i as u8);    // APIC ID — truncated!
    buf.extend_from_slice(&MADT_LAPIC_ENABLED.to_le_bytes());
}
```

When `num_cpus > 255`, APIC IDs wrap around. CPU 256 gets APIC ID 0 (same as CPU 0), CPU 257 gets APIC ID 1, etc. This produces **duplicate APIC IDs** in the MADT.

**Impact:** A guest OS parsing duplicate APIC IDs may:
- Send IPIs to the wrong physical CPU
- Corrupt per-CPU data structures (scheduler queues, TLB shootdown targets)
- Trigger kernel panics or undefined scheduler behavior
- In some kernels, duplicate APIC IDs can cause a use-after-free in CPU hotplug paths

**Exploitation:** If the VMM accepts a vCPU count from user configuration (e.g., cloud API) without capping at 255, an attacker specifies `num_cpus = 256` or higher. The guest kernel sees two CPUs claiming APIC ID 0, leading to exploitable confusion in interrupt routing.

**Fix:** Validate `num_cpus <= 255` for MADT type 0, or emit type 9 (x2APIC) entries for CPUs >= 255.

---

## Finding 2: Unvalidated DSDT Override Corrupts Adjacent Tables (Medium)

**Affected function:** `generate_acpi_layout_inner()`

**Root cause:** The `dsdt_override` parameter is used without validating minimum size:

```rust
let dsdt_offset = tables.len();          // e.g., 64 (after FACS)
let dsdt = dsdt_override.unwrap_or_else(build_dsdt);
let dsdt_size = dsdt.len();              // could be 0..9
tables.extend_from_slice(&dsdt);

// ... FADT, MADT, etc. are appended after DSDT ...

// Much later, at the end of the function:
tables[dsdt_offset + 9] = 0;  // "Clear DSDT checksum"
```

If `dsdt_override.len() < 10`, the write to `tables[dsdt_offset + 9]` lands **inside the FADT** (the next table after the short DSDT), zeroing a FADT header byte:

| `dsdt_size` | Byte corrupted in FADT | Field |
|---|---|---|
| 0 | offset 9 | checksum (benign — also zeroed intentionally) |
| 1 | offset 8 | revision (5 -> 0) |
| 2-5 | offset 7-4 | length field bytes |
| 6-8 | offset 3-1 | signature "FACP" |

**Secondary effect:** With `dsdt_size = 0`, FADT.dsdt and FADT.x_dsdt both point to offset 64 — which is **the FADT itself**. The guest ACPI interpreter attempts to parse FADT raw bytes as AML bytecode. While most parsers check the "DSDT" signature, implementations that don't (or that have signature-bypass bugs) would execute FADT constant data as AML opcodes.

**Exploitation sketch:**
```rust
// Attacker provides empty DSDT via generate_acpi_layout_with_dsdt
let layout = generate_acpi_layout_with_dsdt(4, Some(vec![0x06]));
// dsdt_size=1 → FADT revision corrupted to 0
// Guest ACPI falls back to ACPI 1.0 parsing, ignoring extended fields
// X_DSDT (64-bit) is ignored; legacy DSDT (32-bit) pointer is used
```

**Fix:** Validate `dsdt_override.len() >= ACPI_HDR_SIZE` (36 bytes) and check the "DSDT" signature.

---

## Finding 3: MADT Header Length Truncation (Low)

**Affected function:** `build_madt()`

```rust
let total = ACPI_HDR_SIZE + madt_specific + lapic_entries + ...;
write_header(&mut buf, b"APIC", total as u32, 1);
//                                ^^^^^^^^^^^^ silent truncation
```

With `num_cpus >= 536,870,912` (~0x2000_0000), the computed `total` exceeds `u32::MAX`. The `as u32` truncation causes the ACPI header length field to be drastically smaller than the actual buffer. The `assert_eq!(buf.len(), total)` passes (comparing `usize`), but the guest sees a small table while ~4GB of data bleeds into adjacent guest memory.

This is theoretically exploitable but requires an unrealistic CPU count.

---

## Overall Assessment

The code is **well-structured** for a VMM ACPI generator — checksums are computed correctly, table layouts match the ACPI spec, and Rust's memory safety prevents classic buffer overflows. The `assert_eq!` guards on table sizes catch offset miscounts at build time.

The **primary real-world risk** is Finding 1 (APIC ID truncation), which is triggerable with a modestly-high-but-plausible vCPU count of 256. Finding 2 requires control of the `dsdt_override` API parameter, which is a narrower attack surface. No command injection, path traversal, or race conditions are present — the code is purely deterministic table generation with no I/O or concurrency.
