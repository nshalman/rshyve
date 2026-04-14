# Vulnerability Report: `pio.rs`

## Vulnerability 1: Guest-Triggered VMM Crash via Invalid I/O Size

| Field | Value |
|-------|-------|
| **Type** | Denial of Service / Panic-based crash |
| **Severity** | **High** — Guest-to-host escape (DoS) |
| **Affected function** | `handle_out()`, line ~110 |
| **CWE** | CWE-129 (Improper Validation of Array Index) |

### Root Cause

```rust
pub fn handle_out(&self, port: u16, bytes: u8, val: u32) {
    let data = &val.to_le_bytes()[..bytes as usize];  // <-- HERE
```

`val.to_le_bytes()` returns `[u8; 4]`. The `bytes` parameter is `u8` (range 0–255) and is used directly as a slice upper bound with **no validation**. Any value of `bytes > 4` causes a slice-bounds panic, crashing the VMM process.

x86 I/O only supports widths 1, 2, and 4 — but this code trusts the caller (vCPU exit handler) to enforce that. If the bhyve kernel passes through the raw exit value, or if a future refactor changes the call site, a malicious guest triggers a panic.

### Exploitation Strategy

1. From inside the guest, execute an OUT instruction targeting any port (registered or not — the panic happens before the handler lookup matters).
2. The vCPU exit delivers `bytes` derived from the instruction decode. If the kernel relays the raw `VM_EXIT_INOUT` size field without clamping, craft an instruction sequence that causes the kernel to report `bytes > 4` (e.g., via REP OUTSB with a crafted operand size prefix that confuses the exit info).
3. The VMM process panics → the guest's entire VM dies, and potentially other VMs if they share the process.

### PoC Sketch (guest-side)

```c
// Attempt to trigger an unusual I/O exit size
// The actual trigger depends on bhyve kernel exit reporting
static inline void pio_out_trigger(uint16_t port) {
    // REP OUTSB with crafted prefix — kernel may report
    // unexpected size in vm_exit->u.inout.bytes
    __asm__ volatile(
        "mov $1, %%ecx\n"
        "rep outsb\n"
        : : "d"(port), "S"(&(char){0})
        : "ecx", "memory"
    );
}

int main() {
    iopl(3);  // need I/O privilege
    pio_out_trigger(0x80);  // target any port
}
```

### Fix

Clamp or reject invalid sizes before slicing:

```rust
pub fn handle_out(&self, port: u16, bytes: u8, val: u32) {
    if bytes == 0 || bytes > 4 {
        return; // invalid I/O width, ignore
    }
    let data = &val.to_le_bytes()[..bytes as usize];
    // ...
```

---

## Vulnerability 2: Potential Panic in `handle_in` for Unregistered Ports

| Field | Value |
|-------|-------|
| **Type** | Denial of Service / Panic |
| **Severity** | **Medium** — depends on `ReadOp::write_u32` implementation |
| **Affected function** | `handle_in()`, line ~96 |

### Root Cause

```rust
let mut ro = ReadOp::new(bytes as usize);  // buffer sized to `bytes`
// ...
ro.write_u32(0xFFFF_FFFF);  // writes 4 bytes into possibly 1- or 2-byte buffer
```

When `bytes` is 1 or 2 and the port is unregistered, `ReadOp` is allocated with 1 or 2 bytes, then `write_u32` attempts to write 4 bytes. Whether this panics depends on `ReadOp`'s implementation (not shown). If `write_u32` does a raw 4-byte copy, it panics on the undersized buffer.

### Fix

Use size-appropriate fill:

```rust
ro.buf_mut().fill(0xFF);  // fill with 1s regardless of size
```

---

## Non-Issues (Verified Safe)

- **Race condition in `lookup`**: The Arc-clone-then-drop-lock pattern is correct. A handler invoked after `unregister` is safe because the Arc keeps it alive.
- **Offset calculation** (`port - start as u16`): Safe because ASpace is bounded to `u16::MAX` and `region_at` guarantees `start <= port`.
- **Mutex poisoning**: Handlers are called outside the lock, so handler panics can't poison the mutex.
- **`handle_in` with `bytes > 4`**: The `match` falls through to `_ => 0xFFFF_FFFF`, returning a safe default.

---

**Bottom line**: Vulnerability 1 is the CTF target — an unchecked `bytes` parameter lets a guest crash the VMM via a slice-bounds panic in `handle_out`.
