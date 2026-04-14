# Vulnerability Analysis: `fwcfg.rs`

## Finding 1: TOCTOU Race Condition (Medium)

**Affected functions:** `insert_named()` (line ~116), `rebuild_file_dir()` (line ~191)

### Root Cause

Both functions use a **drop-and-reacquire** pattern on the mutex to avoid deadlock, creating a time-of-check-to-time-of-use window:

```rust
// insert_named: modifies state, drops lock, then calls rebuild
fn insert_named(&self, name: &str, data: Vec<u8>) {
    let mut inner = self.inner.lock().unwrap();
    // ... mutate inner ...
    drop(inner);                    // <-- WINDOW OPENS
    self.rebuild_file_dir();        // <-- re-locks inside
}

// rebuild_file_dir: reads state, drops lock, re-locks to write
fn rebuild_file_dir(&self) {
    let inner = self.inner.lock().unwrap();
    // ... serialize file_dir into buf ...
    drop(inner);                    // <-- WINDOW OPENS
    let mut inner = self.inner.lock().unwrap();
    inner.items.insert(SEL_FILE_DIR, buf);  // stale buf overwrites newer data
}
```

### Exploitation Strategy

With two concurrent `insert_named` calls:

1. **Thread A** inserts item A (selector 0x20), drops lock
2. **Thread A** enters `rebuild_file_dir`, reads `file_dir = [A]`, builds `buf1` (count=1), drops lock
3. **Thread B** inserts item B (selector 0x21), drops lock
4. **Thread B** enters `rebuild_file_dir`, reads `file_dir = [A, B]`, builds `buf2` (count=2), drops lock
5. **Thread B** re-locks, inserts `buf2` (correct: count=2)
6. **Thread A** re-locks, inserts `buf1` (stale: count=1) — **overwrites the correct directory**

The guest now sees only 1 entry but item B's data still exists at selector 0x21. Guest firmware iterating the directory misses item B entirely. If item B was an ACPI table, SMBIOS entry, or other critical config, the guest operates with missing/wrong configuration.

Additionally, a guest reading the directory byte-by-byte (via port 0x511) can observe a **torn read** — count from version N, entries from version N+1 — causing the firmware to parse garbage entries or read beyond the actual entry list (getting zeros from `Vec::get()` bounds checks).

### Severity: Medium
Not directly exploitable for code execution (Rust's bounds checking prevents memory corruption), but can cause guest misconfiguration, which in a CTF context could be leveraged for further exploitation depending on what items are missing.

---

## Finding 2: Selector Overflow — Built-in Item Overwrite (Medium)

**Affected function:** `insert_named()` line ~119

```rust
let selector = inner.next_file_selector;
inner.next_file_selector += 1;  // u16 wraps at 0xFFFF in release builds
```

### Root Cause

No bounds check on `next_file_selector`. In release mode (where Rust wraps on overflow), after 0xFFE0 insertions, the selector wraps to `0x0000` and subsequent inserts overwrite:

| Collision | Selector | Overwrites |
|-----------|----------|------------|
| 0x0000 | `SEL_SIGNATURE` | "QEMU" signature — firmware may reject device |
| 0x0001 | `SEL_ID` | Interface version — firmware may use wrong protocol |
| 0x0019 | `SEL_FILE_DIR` | File directory — attacker-controlled directory content |

### Exploitation Strategy

If any code path allows a guest (or unprivileged caller) to trigger `insert_named` repeatedly, they can wrap the selector counter and overwrite the file directory (0x0019) with controlled data. The directory is also rebuilt afterward, but the `items[0x0019]` entry already contains the attacker's data before `rebuild_file_dir` runs — creating another TOCTOU window where the guest reads the injected data.

### Severity: Medium
Requires ~65,500 insertions. Practical only if `insert_named` is guest-triggerable through some other VMM subsystem.

---

## Finding 3: Integer Truncation on Item Size (Low)

**Affected function:** `insert_named()` line ~121

```rust
let size = data.len() as u32;
```

If `data.len() > u32::MAX` (4 GiB), the size field silently truncates. The full data is stored in `items`, but the directory advertises a smaller size. Guest firmware allocating `size` bytes and reading from the data port would get a buffer that's too small for the actual content, or conversely, the directory would underreport the available data.

### Severity: Low
Theoretical — requires >4 GiB item, unlikely in practice.

---

## Finding 4: `insert_legacy` Lacks Selector Validation (Low)

**Affected function:** `insert_legacy()` line ~132

```rust
pub fn insert_legacy(&self, selector: u16, data: Vec<u8>) {
    let mut inner = self.inner.lock().unwrap();
    inner.items.insert(selector, data);  // any selector, including SEL_FILE_DIR
}
```

No check prevents overwriting `SEL_FILE_DIR` (0x19), `SEL_SIGNATURE` (0x00), or `SEL_ID` (0x01). A misconfigured caller can corrupt the file directory or device identity.

### Severity: Low
Host-side API misuse, not guest-exploitable.

---

## What's Safe

- **No guest-writable data path**: The data port (0x511) ignores writes, so the guest cannot inject data.
- **Bounds-safe reads**: `Vec::get(offset)` returns `None` past the end, preventing any out-of-bounds read.
- **Offset overflow protected**: `saturating_add(1)` prevents `cur_offset` from wrapping.
- **Rust memory safety**: No raw pointers, no `unsafe`, no UB paths.

## CTF Verdict

**Primary target: Finding 1 (TOCTOU)**. If the CTF scenario involves concurrent item insertion (e.g., hot-plugging devices that add fw_cfg entries), you can race the directory rebuild to make the guest miss critical configuration items. The fix is to inline the serialization within a single lock scope instead of the drop-and-reacquire pattern.
