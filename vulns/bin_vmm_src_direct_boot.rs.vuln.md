# Vulnerability Report: `direct_boot.rs`

## Vulnerability 1: Integer Underflow in Initrd Placement (High)

**Affected function:** `load_kernel()`, lines ~165-175

**Root cause:** Unsigned integer subtraction wraps in release builds when `rd_size > lowmem_limit`, and the subsequent safety check is bypassed by the wrapped value.

```rust
let lowmem_limit = (mem_size as u64).min(0xC000_0000);
let rd_size = rd.data.len() as u64;
let rd_addr = (lowmem_limit - rd_size) & !0xFFF;  // ← wraps if rd_size > lowmem_limit
ensure!(
    rd_addr >= kernel_gpa + kernel_bytes.len() as u64,  // ← always true when wrapped
    "initrd overlaps kernel (need more RAM)"
);
mem.write(rd_addr, &rd.data)?;  // ← writes to ~0xFFFF_FFFF_FFFx_xxxx
```

**Trigger:** Provide a guest `mem_size` that is small (e.g., 2 MB = `0x20_0000`) and an initrd file larger than `lowmem_limit` (e.g., 4 MB). Then:

| Value | Result |
|---|---|
| `lowmem_limit` | `0x20_0000` |
| `rd_size` | `0x40_0000` |
| `lowmem_limit - rd_size` | wraps to `0xFFFF_FFFF_FFE0_0000` |
| `rd_addr` (page-aligned) | `0xFFFF_FFFF_FFE0_0000` |
| bounds check | passes (huge > kernel end) |

`mem.write()` then writes 4 MB of attacker-controlled initrd data to guest physical address `0xFFFF_FFFF_FFE0_0000`, far beyond allocated guest RAM. Depending on `MemCtx::write` implementation, this is either an out-of-bounds write into host memory (critical) or a crash (DoS).

**Additionally**, the address stored in `boot_params` is truncated:
```rust
bp[BP_RAMDISK_IMAGE..BP_RAMDISK_IMAGE + 4]
    .copy_from_slice(&(addr as u32).to_le_bytes());  // truncates u64 → u32
```
The guest kernel sees a completely different (truncated) initrd address than where the bytes were actually written.

**Exploitation strategy:**
1. Craft a legitimate bzImage (passes all header checks)
2. Create an oversized initrd relative to `mem_size`
3. Control initrd contents — these become the out-of-bounds write payload
4. If `MemCtx` uses a contiguous mmap, the write lands at a calculable offset past the guest RAM allocation, potentially corrupting host heap metadata or adjacent VMM structures

**Fix:** Check for underflow before subtraction:
```rust
ensure!(rd_size <= lowmem_limit, "initrd too large for guest RAM");
```

---

## Vulnerability 2: Unvalidated `pref_address` From Untrusted Kernel (Medium)

**Affected function:** `KernelImage::open()` line ~120, `load_kernel()` line ~159

`pref_address` is read directly from the bzImage at offset `0x258` and used as the guest write target with zero validation:

```rust
let pref_address = u64::from_le_bytes(
    data[PREF_ADDRESS_OFFSET..PREF_ADDRESS_OFFSET + 8].try_into().unwrap(),
);
// ... later in load_kernel:
let kernel_gpa = kernel.pref_address;  // attacker-controlled
mem.write(kernel_gpa, kernel_bytes)?;   // writes anywhere in guest memory
```

**Exploitation:** A crafted bzImage sets `pref_address = 0x7000` (boot_params address). The kernel payload is written there first, then `write_boot_params` partially overwrites it, but attacker-controlled bytes survive in the uninitialised gaps. The entry point (`pref_address + 0x200 = 0x7200`) points into this region — the guest boots directly into attacker-supplied code at ring 0 with identity-mapped 4 GB.

More critically, `pref_address = 0x9000` (PML4) lets the attacker control page tables before the guest kernel runs, enabling arbitrary guest-physical-to-host-physical remapping if the VMM trusts guest page tables for any DMA path.

---

## Vulnerability 3: Unvalidated `setup_sects` Causes Panic/DoS (Low)

**Affected function:** `KernelImage::open()` line ~115, `kernel_bytes()` line ~131

The only file-size check is `data.len() > 0x290` (657 bytes minimum). But `kernel_offset = (setup_sects + 1) * 512` can be up to 131,072. A file of 700 bytes with `setup_sects = 1` sets `kernel_offset = 1024`, and `&self.data[1024..]` panics at runtime.

**Impact:** DoS — crash the VMM process. Not exploitable for code execution in safe Rust.

---

## Summary

| # | Vulnerability | Severity | Impact |
|---|---|---|---|
| 1 | Integer underflow in initrd placement | **High** | OOB write past guest RAM; potential host memory corruption |
| 2 | Unvalidated `pref_address` from kernel image | **Medium** | Arbitrary guest memory write; guest page table hijack |
| 3 | Missing `setup_sects` vs file-length check | **Low** | DoS (panic) |

**The primary CTF target is Vuln #1** — the integer underflow is a classic unsigned arithmetic bug that bypasses its own safety check, with attacker-controlled write address and payload.
