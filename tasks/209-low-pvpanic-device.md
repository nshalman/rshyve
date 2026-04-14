# 209 - LOW - QEMU pvpanic device

## Problem

When a guest kernel panics, the VMM has no way to detect it — the guest just hangs
or reboots. The QEMU pvpanic device is a simple I/O port that the guest writes to
on panic, allowing the VMM to log the event and optionally take action (halt, notify
control plane).

Linux guests have built-in pvpanic support via `CONFIG_PVPANIC`.

## Design

### Device spec

- **I/O port:** 0x505 (single byte)
- **Read:** Returns supported events bitmask
- **Write:** Guest reports an event

### Event bits

```rust
const PVPANIC_PANICKED: u8 = 1 << 0;      // Guest has panicked
const PVPANIC_CRASH_LOADED: u8 = 1 << 1;   // Guest has crash kernel loaded
```

### Implementation

```rust
pub struct PvPanic {
    log: slog::Logger,
    event_handler: Box<dyn Fn(u8) + Send + Sync>,
}

impl PioDevice for PvPanic {
    fn pio_read(&self, _port: u16, _len: u8) -> u32 {
        (PVPANIC_PANICKED | PVPANIC_CRASH_LOADED) as u32
    }

    fn pio_write(&self, _port: u16, val: u32, _len: u8) {
        let event = val as u8;
        if event & PVPANIC_PANICKED != 0 {
            slog::crit!(self.log, "GUEST PANIC DETECTED via pvpanic");
            (self.event_handler)(event);
        }
    }
}
```

### Control plane integration

On panic detection:
- Log critical message
- Send event via control socket: `{"event": "guest_panic", "timestamp": ...}`
- Optionally halt VM (configurable)

### ACPI discovery

Guest discovers pvpanic via ACPI DSDT. Add to ACPI table generation:
```
Device (PANC) {
    Name (_HID, "QEMU0001")
    Name (_CRS, ResourceTemplate () {
        IO (Decode16, 0x0505, 0x0505, 0x01, 0x01)
    })
}
```

## Files to create

- `crates/vmm-devices/src/pvpanic.rs` — ~50 lines

## Files to modify

- `crates/vmm-devices/src/lib.rs` — Add `pub mod pvpanic;`
- `crates/vmm-devices/src/acpi.rs` — Add PANC device to DSDT
- `bin/vmm/src/main.rs` — Register pvpanic on PIO bus at port 0x505
- `bin/vmm/src/control.rs` — Forward panic events

## Verification

1. Boot Linux guest with `CONFIG_PVPANIC=y`
2. `dmesg | grep pvpanic` — device detected
3. Trigger panic: `echo c > /proc/sysrq-trigger`
4. VMM log shows "GUEST PANIC DETECTED"
5. Control socket receives panic event
