# Vulnerability Analysis: `vmm-config/src/lib.rs`

## Summary

This file is a CLI argument parser written in safe Rust (no `unsafe`). Rust's type system and memory safety eliminate most classic C-style vulnerabilities. However, there are real issues worth reporting.

---

## Finding 1: Plaintext Password Exposed via Process Listing

**Severity: Medium** (information disclosure / credential leak)

**Affected code (line ~121):**
```rust
/// Root password for mdata agent (plaintext, set via cloud-init)
#[arg(long)]
pub mdata_root_pw: Option<String>,
```

**Root cause:** The root password is accepted as a command-line argument. On illumos/Linux, CLI args are world-readable via `/proc/<pid>/cmdline` and `ps auxww`. Any unprivileged user on the host can harvest VM root passwords from running VMM processes.

**Exploitation:**
```bash
# Attacker with any local shell account:
ps auxww | grep vmm | grep -o -- '--mdata-root-pw [^ ]*'
# or
cat /proc/$(pgrep vmm)/cmdline | tr '\0' '\n' | grep -A1 mdata-root-pw
```

**Impact:** Credential disclosure. Attacker obtains root password for guest VMs, enabling lateral movement if guests are network-accessible.

---

## Finding 2: No Upper-Bound Validation on CPU Count or Memory Size

**Severity: Low** (local denial of service)

**Affected functions:** `num_cpus()` (line ~139), `parse_mem_size()` (line ~20)

**Root cause:** CPU count is validated `> 0` but has no upper bound — a value like `4294967295` (u32::MAX) passes validation. Memory size uses `checked_mul` to prevent overflow but doesn't cap the result — `16384T` parses successfully.

These values flow to kernel resource allocation (bhyve VM creation, memory mapping). Requesting absurd resources can exhaust host memory or kernel objects.

**Exploitation:**
```bash
./vmm -c 4294967295 -m 16384T my_vm
```

**Impact:** DoS against the hypervisor host. Mitigated if the VMM runs inside a zone with resource caps, but the parser itself doesn't defend against it.

---

## Finding 3: Unvalidated `vm_name` Used as Positional Argument

**Severity: Low** (depends on downstream usage)

**Affected code (line ~131):**
```rust
pub vm_name: String,
```

**Root cause:** `vm_name` is accepted without character validation. If downstream code passes it to `ioctl` names, file paths, or log format strings without sanitization, this could enable path traversal (`../../etc/shadow`) or log injection (`\nINFO: admin logged in`).

**Impact:** Depends entirely on how `vm_name` is consumed. Worth checking `main.rs` and `control.rs` for uses.

---

## What's NOT Vulnerable

- **No integer overflow in `parse_mem_size`**: `checked_mul` correctly guards the multiplication.
- **No string slicing panic**: The suffix match on ASCII bytes is UTF-8 safe, and empty-after-trim is checked.
- **No memory corruption**: Pure safe Rust, no `unsafe`, no raw pointers.
- **No command injection in this file**: Inputs are parsed but not executed here.

---

## Verdict

For a CTF, **Finding 1 (password in process listing)** is the most immediately exploitable — it requires only unprivileged local access and zero interaction. If the flag is hidden elsewhere in the codebase, check how `vm_name` and the PCI slot strings (`-s`) are consumed downstream, as those are the most likely injection surfaces.
