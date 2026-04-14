## Vulnerability Analysis: `pmtimer.rs`

### Finding: Integer Overflow in Port Calculation

**Severity:** Medium
**Type:** Integer overflow leading to port mis-binding
**Function:** `BhyvePmTimer::create()` (line 30)

#### Root Cause

```rust
let port = pmbase + PM_TMR_OFFSET;  // u16 + u16, wraps in release builds
```

`pmbase` is `u16`, `PM_TMR_OFFSET` is `0x08`. In release builds (`cargo build --release` — which is what this VMM uses per CLAUDE.md), Rust wraps integer overflow silently. If `pmbase >= 0xFFF9`, the addition wraps modulo 65536:

| `pmbase` | `pmbase + 0x08` | Actual `port` |
|----------|-----------------|---------------|
| `0xFFF8` | `0x10000`       | `0x0000`      |
| `0xFFFA` | `0x10002`       | `0x0002`      |
| `0xFFFF` | `0x10007`       | `0x0007`      |

The wrapped port is then passed directly to `VM_PMTMR_LOCATE` via ioctl (line 46), telling the kernel to intercept PIO reads on that port and return PM timer values.

#### Exploitation Strategy

Ports `0x0000–0x0007` overlap the **DMA controller** and **PIC** (Programmable Interrupt Controller). If an attacker controls the `pmbase` argument (e.g., through a configuration interface, API, or migration payload):

1. Pass `pmbase = 0xFFFF` → PM timer binds to port `0x0007`
2. Kernel now intercepts reads on that port, returning ACPI timer values instead of expected DMA/PIC data
3. Guest OS reads from port `0x0007` expecting PIC data, gets PM timer garbage
4. This **shadows legitimate device emulation**, causing device confusion or guest OS instability
5. Depending on the kernel's handling, could also be used to **prevent other devices from binding** to those ports (denial of service on device emulation)

#### Exploit Sketch

```rust
// Attacker-controlled config or migration payload sets pmbase
let timer = BhyvePmTimer::create(hdl.clone(), 0xFFFF);
// port = 0xFFFF + 0x08 = 0x0007 (wrapped)
timer.attach()?;
// Kernel now returns PM timer values for reads on port 0x0007
// Guest's PIC driver is now broken
```

#### Secondary Issues

- **No port validation**: `attach()` blindly passes any port to the kernel ioctl with no range/sanity check.
- **No `Drop` cleanup**: No detach on drop — once the port is registered with the kernel, there's no way to reclaim it if the `BhyvePmTimer` is dropped. Not directly exploitable, but contributes to resource exhaustion if timers are created/dropped repeatedly.
- **Ordering not enforced**: The comment says "must be called before vCPUs start" but nothing enforces this invariant. A race between `attach()` and vCPU execution could observe a partially-configured state.

#### Fix

```rust
pub fn create(hdl: Arc<VmmHdl>, pmbase: u16) -> Result<Arc<Self>, &'static str> {
    let port = pmbase.checked_add(PM_TMR_OFFSET)
        .ok_or("pmbase + PM_TMR_OFFSET overflows u16")?;
    Ok(Arc::new(Self { hdl, port }))
}
```

#### Verdict

The integer overflow is a real bug. Exploitability depends on whether `pmbase` is attacker-controllable — in a CTF context where configuration or migration payloads are in scope, this is the vulnerability.
