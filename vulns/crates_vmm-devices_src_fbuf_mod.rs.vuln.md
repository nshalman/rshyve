# Vulnerability Report: `fbuf/mod.rs`

## Vulnerability 1: Guest-to-Host OOM via Resolution Manipulation (High)

**Affected function**: `send_framebuffer_update` (around line 395-420)
**Type**: Denial of Service / Memory Exhaustion

### Root Cause

The guest controls display resolution via BAR0 writes with no upper-bound validation — only a `> 0` check. The VNC server uses this guest-controlled resolution to compute frame size and allocates a heap buffer to pad the difference:

```rust
// Resolution from guest-controlled atomics (max 65535x65535)
let stride = u32::from(width) * 4;
let frame_bytes = stride as usize * usize::from(height);
// ...
let avail = fb.pixels.len().min(frame_bytes);  // avail = 16 MiB (fixed)
if avail < frame_bytes {
    let padding = vec![0u8; frame_bytes - avail];  // <-- OOM HERE
    writer.write_all(&padding)?;
}
```

**Math**: `65535 * 4 * 65535 = ~17.2 GB`. The backing buffer is fixed at 16 MiB. The padding allocation is `17.2 GB - 16 MiB ≈ 17.2 GB`.

### Exploitation Strategy

1. Guest writes `0xFFFF` to BAR0 offset `0x04` (WIDTH register, 2-byte write)
2. Guest writes `0xFFFF` to BAR0 offset `0x06` (HEIGHT register, 2-byte write)
3. Wait for a VNC client to connect and issue `FramebufferUpdateRequest`
4. `send_framebuffer_update` attempts `vec![0u8; ~17_179_000_000]`
5. VMM process is OOM-killed, destroying the VM and any co-located state

On the headnode (~2.5 GB free per CLAUDE.md), this is an instant kill.

### Exploit Sketch (guest kernel module or MMIO poke)

```c
// From inside the guest, write to the PCI BAR0 MMIO region
// Assuming BAR0 is mapped at some address via PCI enumeration
volatile uint16_t *width_reg  = (void *)(bar0_base + 0x04);
volatile uint16_t *height_reg = (void *)(bar0_base + 0x06);

*width_reg  = 0xFFFF;  // 65535
*height_reg = 0xFFFF;  // 65535
// Now wait — next VNC frame update crashes the host VMM process
```

### Fix

Cap resolution to the maximum the 16 MiB framebuffer can hold:

```rust
// In bar0_write, reject resolutions that exceed BAR1_SIZE
let max_pixels = BAR1_SIZE as usize / 4; // 4 bytes per pixel
if (new_w as usize) * (new_h as usize) > max_pixels {
    return; // reject
}
```

And/or in `send_framebuffer_update`, clamp `frame_bytes` to `fb.pixels.len()` instead of allocating padding.

---

## Vulnerability 2: Resolution TOCTOU Race (Medium)

**Affected function**: `bar0_write` (lines ~280-310)
**Type**: Race condition (load-modify-store without CAS)

When writing only width or only height, the code does a non-atomic read-modify-write:

```rust
let old = self.resolution.load(Ordering::Relaxed);
let h = unpack_height(old);
self.resolution.store(pack_resolution(new_w, h), Ordering::Release);
```

Two concurrent MMIO writes (e.g., one vCPU writes width, another writes height) can lose one update. In combination with Vuln 1, this could be used to sneak in a large dimension even if a fix were applied that validates `width * height` — by racing a large width write against a small height write, the validation might see `large_width * small_height` as valid, then a second write increases height, resulting in an unchecked `large * large`.

**Fix**: Use `AtomicU32::compare_exchange` in a loop (CAS loop).

---

## Vulnerability 3: Unix Socket Created Without Permission Restriction (Low)

**Affected function**: `vnc_server_loop` (line ~340)

```rust
let _ = std::fs::remove_file(&dev.vnc_path);
let listener = UnixListener::bind(&dev.vnc_path)?;
```

No `fchmod`/umask restriction is applied. If `vnc_path` is in a world-writable directory (e.g., `/tmp/`), any local user can connect to the VNC socket and view/interact with the VM display — including reading screen contents and (once keyboard/pointer forwarding is implemented) injecting input.

There's also a minor TOCTOU between `remove_file` and `bind` where another process could race to create a socket at the same path.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | Guest-controlled OOM via `vec![0u8; ~17GB]` | **High** | Yes — guest writes 2 BAR0 registers, VNC update kills VMM |
| 2 | Resolution TOCTOU (non-atomic RMW) | Medium | Can bypass single-dimension validation |
| 3 | World-accessible VNC socket | Low | Requires local access to host |

**The primary CTF flag is Vulnerability 1** — it's a clean guest-to-host DoS with a two-write trigger and no preconditions beyond a VNC client being connected.
