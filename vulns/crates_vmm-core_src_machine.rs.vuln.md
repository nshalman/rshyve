# Vulnerability Analysis: `machine.rs`

## Finding 1: MMIO Hole RAM Overlap — Guest Memory Mapped Over Device MMIO Region

**Severity:** High  
**Type:** Memory layout confusion / Device isolation bypass  
**Affected:** `Builder::build()`, lines ~193-210 (the validation + RAM mapping logic)

### Root Cause

The `lowmem_limit` validation permits values up to and including `FOUR_GIB` (0x1_0000_0000):

```rust
anyhow::ensure!(
    self.lowmem_limit > 0 && self.lowmem_limit <= FOUR_GIB,  // BUG: allows 0xC000_0001..=0x1_0000_0000
    ...
);
```

But the MMIO hole occupies `0xC000_0000` to `0x1_0000_0000`. The default `lowmem_limit` is correctly `0xC000_0000`, placing RAM below the hole. However, any caller can use `.lowmem_limit(0x1_0000_0000)` (or any value > `DEFAULT_LOWMEM_LIMIT`), which causes:

```rust
let lowmem_size = self.mem_size.min(self.lowmem_limit as usize);
map.add_ram(&hdl, 0, lowmem_size)?;  // Maps RAM from 0 to 4 GiB — COVERS the MMIO hole
```

This maps guest RAM over the entire `0xC000_0000..0x1_0000_0000` region where PCI BARs, the local APIC, IOAPIC, and other MMIO devices reside.

### Impact

- **Device isolation broken:** Guest reads/writes to the MMIO hole hit RAM instead of device emulation, or vice versa (depending on EPT precedence). Either outcome is bad:
  - If RAM wins: MMIO devices (virtio, PCI config space) become invisible to the guest — denial of service, or the guest can manipulate "device state" by writing to raw RAM backing.
  - If MMIO wins: The guest has a "shadow" RAM region it can't access, but the VMM *thinks* is valid guest memory — confused deputy during migration/snapshot where the VMM reads stale RAM believing it's guest state.
- **Potential guest escape vector:** If the VMM's device emulation reads guest RAM at MMIO-hole addresses (e.g., DMA descriptors pointing into `0xC000_0000..0xFFFF_FFFF`), the overlap means those reads hit memory the guest can also write via the RAM path, enabling TOCTOU attacks on device emulation.

### Exploitation Strategy

```
1. Configure VM with: .lowmem_limit(FOUR_GIB).memory(4 * GiB)
2. Guest writes to 0xC000_0000..0xFFFF_FFFF — this hits RAM, not MMIO devices
3. VMM device emulation (virtio-blk, virtio-net) still reads those addresses
   as guest memory for DMA operations
4. Guest controls "device register" contents by writing to RAM at MMIO addresses
5. Craft DMA descriptors at MMIO addresses that point back into VMM-trusted
   regions → confused deputy → potential host memory read/write
```

### Fix

```rust
anyhow::ensure!(
    self.lowmem_limit > 0 && self.lowmem_limit <= DEFAULT_LOWMEM_LIMIT,
    "lowmem_limit must be (0, {:#x}], got {:#x}",
    DEFAULT_LOWMEM_LIMIT, self.lowmem_limit,
);
```

---

## Finding 2: VM Name Not Sanitized — Path Traversal in Device Node Creation

**Severity:** Medium  
**Type:** Path traversal  
**Affected:** `Builder::new()` / `Builder::build()`, lines ~143, 193

### Root Cause

The VM name is accepted as arbitrary `&str` and passed directly to `VmmHdl::create()`:

```rust
pub fn new(name: &str) -> Self {
    Self { name: name.to_string(), ... }
}
// ...
let hdl = Arc::new(VmmHdl::create(&self.name, &self.opts)?);
```

In bhyve, `vm_create()` creates a device node at `/dev/vmm/<name>`. If `name` contains `../` or null bytes, the kernel call may create or open device nodes outside `/dev/vmm/`, depending on the kernel's ioctl handler.

### Exploitation Strategy

```
Builder::new("../../tmp/evil_device").build()
→ VmmHdl::create("../../tmp/evil_device", ...)
→ kernel creates /dev/vmm/../../tmp/evil_device → /dev/tmp/evil_device (if unsanitized)
```

Depends on whether the illumos bhyve kernel module validates the name — but the userspace code should not rely on kernel sanitization.

### Fix

Validate the name in `build()`:
```rust
anyhow::ensure!(
    !self.name.is_empty()
        && !self.name.contains('/')
        && !self.name.contains('\0')
        && self.name.len() <= 64,
    "invalid VM name: {:?}", self.name,
);
```

---

## Finding 3: No Page Alignment Enforcement on Memory Sizes

**Severity:** Low  
**Type:** Integer alignment bug  
**Affected:** `Builder::build()`, lines ~200-210

### Root Cause

Neither `mem_size` nor `lowmem_limit` are checked for page alignment (typically 4 KiB). The bhyve kernel expects page-aligned sizes for `vm_mmap_memseg`. If a non-aligned `lowmem_limit` like `0xC000_0001` is provided:

```rust
let lowmem_size = self.mem_size.min(0xC000_0001); // Not page-aligned
map.add_ram(&hdl, 0, lowmem_size)?;               // Kernel may round up/down

if self.mem_size > 0xC000_0001 {
    let highmem_size = self.mem_size - 0xC000_0001;  // Also not aligned
    map.add_ram(&hdl, FOUR_GIB, highmem_size)?;
}
```

If the kernel rounds `lowmem_size` up to the next page, the extra bytes overlap into the MMIO hole. If it rounds `highmem_size` down, guest memory is smaller than `mem_size`, causing the VMM to access unmapped memory.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | MMIO hole RAM overlap via `lowmem_limit` | **High** | Yes — attacker-controlled config maps RAM over MMIO devices |
| 2 | VM name path traversal | Medium | Depends on kernel sanitization |
| 3 | Non-page-aligned memory sizes | Low | Edge case, likely caught by kernel |

**Finding 1 is the flag-worthy vulnerability.** The validation explicitly permits a `lowmem_limit` value that breaks the fundamental x86 memory layout invariant documented in the file's own header comment. The fix is a one-line change to the validation bound.
