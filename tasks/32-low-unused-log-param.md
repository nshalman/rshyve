# 32 — `_log` parameter accepted but unused in spawn_serial_drain

**Priority:** LOW
**File:** `bin/vmm/src/main.rs:62`
**Category:** Code quality — dead parameter

## Problem

```rust
fn spawn_serial_drain(
    uart: Arc<LpcUart>,
    _log: Logger,
    output_path: Option<&std::path::Path>,
) -> thread::JoinHandle<()> {
```

The `_log` parameter is passed from the caller but never used. The underscore prefix suppresses the unused-variable warning, masking the issue. The function could use the logger for:
- Warning when output file creation fails (line 69 silently falls back to stdout)
- Logging serial output write errors

## Fix

Either use it or remove it:

Option A — Use it (recommended, addresses the file-open silent failure):

```rust
fn spawn_serial_drain(
    uart: Arc<LpcUart>,
    log: Logger,
    output_path: Option<&std::path::Path>,
) -> thread::JoinHandle<()> {
    let output: Box<dyn Write + Send> = if let Some(path) = output_path {
        match std::fs::File::create(path) {
            Ok(f) => Box::new(f),
            Err(e) => {
                warn!(log, "failed to open serial output file, using stdout";
                    "path" => %path.display(), "error" => %e);
                Box::new(std::io::stdout())
            }
        }
    } else {
        Box::new(std::io::stdout())
    };
```

Option B — Remove the parameter and update the call site.
