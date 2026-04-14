# 30 — slog-bunyan dependency declared but never used

**Priority:** LOW
**File:** `Cargo.toml:48`
**Category:** Build — unnecessary dependency

## Problem

```toml
slog-bunyan = "2.4"
```

This dependency is declared in workspace `[dependencies]` but never referenced in any crate's `[dependencies]` section. It adds unnecessary download time and compile time.

## Fix

Remove from workspace Cargo.toml:

```diff
-slog-bunyan = "2.4"
```

Verify with:
```bash
cargo build --workspace 2>&1 | grep -i bunyan
```

If it was intended for future use (bunyan JSON log format), add it when actually needed.
