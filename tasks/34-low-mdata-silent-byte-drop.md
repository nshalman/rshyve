# 34 — mdata agent silently drops bytes after 1000 retries

**Priority:** LOW
**File:** `bin/vmm/src/mdata.rs:131-140`
**Category:** Silent failure — guest data corruption

## Problem

```rust
fn send_byte_paced(uart: &LpcUart, byte: u8) {
    for _ in 0..1000 {
        if uart.input_byte(byte) {
            return;
        }
        thread::sleep(Duration::from_micros(100));
    }
    // Last resort -- drop the byte (guest isn't reading)
}
```

After 1000 retries (~100ms), the byte is silently dropped with no logging. The guest receives truncated mdata responses — network configuration, SSH keys, or user-script data may be corrupted.

## Fix

Log the dropped byte and increment a counter:

```rust
fn send_byte_paced(uart: &LpcUart, byte: u8, log: &Logger) -> bool {
    for _ in 0..1000 {
        if uart.input_byte(byte) {
            return true;
        }
        thread::sleep(Duration::from_micros(100));
    }
    warn!(log, "mdata: dropped byte after 1000 retries — guest UART FIFO full";
        "byte" => format!("0x{byte:02x}"));
    false
}
```

In the caller, if `send_byte_paced` returns false, consider aborting the current mdata response rather than sending a partial/corrupted response.
