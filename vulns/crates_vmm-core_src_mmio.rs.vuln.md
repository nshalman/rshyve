# Vulnerability Report: `mmio.rs` — Guest-Triggered VMM Panic (DoS)

## Vulnerability 1: Out-of-Bounds Slice Panic in `handle_write` (High)

**Affected function:** `handle_write` — line ~101

**Root cause:**

```rust
pub fn handle_write(&self, addr: u64, bytes: u8, val: u64) {
    let data = &val.to_le_bytes()[..bytes as usize];  // <-- PANIC if bytes > 8
```

`val.to_le_bytes()` returns `[u8; 8]`. The slice `[..bytes as usize]` requires `bytes <= 8`. Since `bytes` is a `u8` (range 0–255), any value > 8 causes an index-out-of-bounds panic, crashing the VMM process.

**Compare with `handle_read`** which is safe — it handles unexpected `bytes` values via the `_ => 0xFFFF...` fallback arm. `handle_write` has no such guard.

**Exploitation strategy:**

A malicious guest triggers an MMIO write VM-exit with a non-standard access width. On x86, this can happen via:
- SIMD instructions (MOVAPS/MOVDQU) generating 16-byte or 32-byte writes to an MMIO-mapped address
- REP MOVSB across an MMIO region boundary where the exit handler reports the full transfer size
- Unusual instruction encodings that hardware decodes to non-power-of-2 sizes

If the VCPU exit handler passes the hardware-reported access size directly to `handle_write` without clamping to {1, 2, 4, 8}, the VMM panics. This is a **guest-to-host denial of service** — the guest kills its own VMM process.

**Exploit sketch:**

```c
// Inside guest kernel or userspace with /dev/mem access
// Map the MMIO BAR for a virtio device
volatile char *mmio = mmap_device_mmio(BAR_ADDR);

// Use a 16-byte SSE store to the MMIO region
__m128i payload = _mm_set1_epi32(0xDEADBEEF);
_mm_store_si128((__m128i *)mmio, payload);  // triggers 16-byte MMIO write exit
// VMM panics: index 16 out of range for slice of length 8
```

**Fix:**

```rust
pub fn handle_write(&self, addr: u64, bytes: u8, val: u64) {
    if bytes == 0 || bytes > 8 {
        return; // ignore invalid width, same as unregistered address
    }
    let data = &val.to_le_bytes()[..bytes as usize];
    // ...
}
```

---

## Vulnerability 2: Cross-Region MMIO Access (Medium)

**Affected function:** `handle_write` and `handle_read` — lines ~80, ~101

**Root cause:** The lookup uses only the base address `addr` to find the handler, but never validates that the full access range `[addr, addr+bytes)` falls within the registered region. A handler at `[0x1000, 0x1010)` could receive a 4-byte read at offset `0x000F`, meaning bytes 1–3 spill into unregistered (or a different device's) address space.

The handler receives offset `0x0F` and a 4-byte operation — it may index into its internal register file at offsets 0x0F–0x12, causing out-of-bounds access within the device emulation logic.

**Exploitation:** Craft an MMIO access near a region boundary to confuse the device handler into reading/writing beyond its intended register space.

---

## Vulnerability 3: Mutex Poison Cascading Crash (Low)

**Affected:** All public methods — `register`, `unregister`, `handle_read`, `handle_write`, `lookup`

All use `.lock().unwrap()`. If any handler panics (e.g., from Vuln #1 in another thread), the Mutex is poisoned and **every subsequent MMIO operation panics**, making recovery impossible.

---

## Summary

| # | Type | Severity | Exploitable? |
|---|------|----------|--------------|
| 1 | Panic from unchecked slice index | **High** | Yes — guest DoS via wide MMIO write |
| 2 | Cross-region boundary access | **Medium** | Depends on handler internals |
| 3 | Mutex poison cascade | **Low** | Amplifies #1 |

**The primary CTF target is Vulnerability #1** — it's a single-instruction guest-to-host crash with no preconditions beyond triggering an MMIO write wider than 8 bytes.
