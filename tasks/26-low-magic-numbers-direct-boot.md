# 26 — Magic numbers throughout direct_boot.rs

**Priority:** LOW
**File:** `bin/vmm/src/direct_boot.rs:296-302, 375-378`
**Category:** Code quality — readability, verifiability

## Problem

CPU register values and GDT descriptors are raw hex with inline comments:

```rust
vcpu.set_reg(VM_REG_GUEST_CR0, 0x8000_0031)?;  // PG | ET | NE | PE
vcpu.set_reg(VM_REG_GUEST_CR4, 0x20)?;           // PAE
vcpu.set_reg(VM_REG_GUEST_EFER, 0x500)?;         // LMA | LME
```

GDT entries:
```rust
0x00AF_9B00_0000_FFFF,  // code64
0x00CF_9300_0000_FFFF,  // data
```

These values are not immediately verifiable without consulting the Intel SDM.

## Fix

Define named constants:

```rust
// CR0 bits
const CR0_PE: u64 = 1 << 0;   // Protection Enable
const CR0_NE: u64 = 1 << 5;   // Numeric Error
const CR0_ET: u64 = 1 << 4;   // Extension Type
const CR0_PG: u64 = 1 << 31;  // Paging

// CR4 bits
const CR4_PAE: u64 = 1 << 5;  // Physical Address Extension

// EFER bits
const EFER_LME: u64 = 1 << 8;  // Long Mode Enable
const EFER_LMA: u64 = 1 << 10; // Long Mode Active

// Usage:
vcpu.set_reg(VM_REG_GUEST_CR0, CR0_PG | CR0_ET | CR0_NE | CR0_PE)?;
vcpu.set_reg(VM_REG_GUEST_CR4, CR4_PAE)?;
vcpu.set_reg(VM_REG_GUEST_EFER, EFER_LMA | EFER_LME)?;
```

For GDT descriptors, either use a builder or document the bit fields:

```rust
/// 64-bit code segment: base=0, limit=0xFFFFF, G=1, L=1, P=1, DPL=0, type=0xB (exec/read/accessed)
const GDT_CODE64: u64 = 0x00AF_9B00_0000_FFFF;
/// Data segment: base=0, limit=0xFFFFF, G=1, D=1, P=1, DPL=0, type=0x3 (read/write/accessed)
const GDT_DATA: u64 = 0x00CF_9300_0000_FFFF;
```
