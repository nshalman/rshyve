# 33 — Serial output falls back to stdout — interleaves with console input

**Priority:** LOW
**File:** `bin/vmm/src/main.rs:69`
**Category:** Correctness — I/O interleaving

## Problem

```rust
Err(_) => Box::new(std::io::stdout()),
```

When the serial output file cannot be created, serial data goes to stdout. But the VMM also uses stdout for the serial console input thread (line ~1151 reads stdin for COM1 input). This can cause interleaving of serial output with the console input prompt, making the console unusable.

The error is also silently swallowed — the user has no idea their serial log file was not created.

## Fix

1. Log the error (see task #32)
2. Use stderr instead of stdout as fallback:

```rust
Err(e) => {
    warn!(log, "serial output file failed, using stderr"; "error" => %e);
    Box::new(std::io::stderr())
}
```

This avoids interleaving with stdin-based console input.
