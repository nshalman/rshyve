# Vulnerability Report: `bootrom.rs`

## Finding 1: TOCTOU Race Condition in `BootromFile::open`

**Severity: Medium**
**Type: Race Condition (CWE-367)**
**Function: `BootromFile::open`**, lines ~87–115

### Root Cause

The file size is validated via `fstat` (line ~93), but file content is read later via `read_to_end` (line ~112). Between these two operations on the same open fd, another process can overwrite the file content using `pwrite()` — changing the data without changing the file length. The final guard:

```rust
ensure!(data.len() == size, ...);
```

only checks **length**, not **content integrity**. A same-length replacement sails through.

### Exploitation Strategy

1. Place a benign, page-aligned UEFI ROM at the expected path (e.g. 2 MiB).
2. Wait for the VMM to start a VM and call `BootromFile::open`.
3. Race: after `metadata()` returns but before `read_to_end` completes, use `pwrite()` from a second process to overwrite the file's contents with a malicious UEFI image of the **exact same size**.
4. `read_to_end` reads the malicious bytes. `data.len() == size` passes.
5. `load_bootrom` writes the attacker's firmware into guest physical memory at the reset vector.

The attacker needs write access to the bootrom file. In a multi-tenant environment (Triton/SmartOS), if the provisioning path for the bootrom is writable by a tenant or a co-located process, this is exploitable.

### PoC Sketch

```bash
# Terminal 1: VMM starts a VM
./vmm -l bootrom,/tmp/shared_rom.fd ...

# Terminal 2: race the overwrite
while true; do
  dd if=malicious_uefi.fd of=/tmp/shared_rom.fd bs=2M conv=notrunc 2>/dev/null
done
```

A more reliable PoC would use `inotify`/`dtrace` to trigger the overwrite immediately after the `fstat` syscall.

### Fix

Read the file, **then** validate length — or use `mmap` + `MAP_PRIVATE` for an atomic snapshot. Alternatively, verify a cryptographic hash of the content after reading.

---

## Finding 2: SEC Temp RAM Size/Base Mismatch (Logic Bug)

**Severity: Medium**
**Type: Logic Bug / Memory Mapping Error**
**Constants**, lines ~52–57

### Root Cause

The comment and the code contradict each other:

```rust
/// We map 48 MiB of writable RAM from 0xFC000000 to 0xFEFFFFFF
/// (immediately below the 16 MiB ROM at 0xFF000000).
const SEC_TEMP_RAM_SIZE: usize = 4 * 1024 * 1024; // 4 MiB  ← not 48 MiB
const SEC_TEMP_RAM_GPA: u64 = 0xFCC0_0000;         // ← not 0xFC000000
```

| Property | Comment says | Code says |
|---|---|---|
| Size | 48 MiB | 4 MiB |
| Base GPA | `0xFC00_0000` | `0xFCC0_0000` |
| End GPA | `0xFEFF_FFFF` | `0xFCFF_FFFF` |

If OVMF firmware expects writable temp RAM across the full 48 MiB region (as bhyve C provides), it will access unmapped GPAs. Depending on how the VMM handles unmapped GPA faults, this could:

- Crash the guest silently (denial of service)
- Trigger an unhandled VM-exit in the VMM host process
- Cause the VMM to expose uninitialized host memory if the fault handler has a fallback path

---

## Finding 3: No Path Sanitization in `find_bootrom_path`

**Severity: Low**
**Type: Path Traversal (CWE-22)**
**Function: `find_bootrom_path`**, lines ~155–170

User-supplied input from `-l bootrom,<path>` is passed directly to `PathBuf::from` with only `trim()`. No checks for `..`, symlinks, or absolute paths outside expected directories. If the VMM's command-line arguments are constructed from an API (e.g., a cloud provisioning layer that builds CLI args from tenant metadata), an attacker could supply:

```
bootrom,/etc/shadow
```

The file's contents would be read into `BootromFile::data`. Even though it wouldn't function as UEFI firmware, the error messages would leak whether the file exists and its size — an oracle for file enumeration.

---

## Finding 4: `u64 as usize` Truncation

**Severity: Low (theoretical)**
**Line:** ~96 (`let size = metadata.len() as usize;`)

On a 32-bit platform, a file of size `0x1_0000_1000` (4 GiB + 4 KiB) truncates to `0x1000` (4 KiB), passing all size checks. `read_to_end` then attempts to read the full 4+ GiB. Unlikely to be the target platform but violates defense-in-depth. Fix: `usize::try_from(metadata.len())?`.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | TOCTOU: content swap between stat and read | Medium | Yes, with write access to bootrom file |
| 2 | SEC temp RAM 4 MiB vs documented 48 MiB | Medium | Triggers unmapped GPA faults in firmware |
| 3 | Path traversal in `find_bootrom_path` | Low | Depends on whether CLI args are API-constructed |
| 4 | `u64 as usize` truncation | Low | Theoretical (32-bit only) |

The code is generally well-written — no `unsafe`, proper error propagation, length checks. The TOCTOU race in Finding 1 is the most practically exploitable issue.
