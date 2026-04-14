# 31 — `strip = true` in release profile prevents core dump analysis

**Priority:** LOW
**File:** `Cargo.toml:24`
**Category:** Operations — debuggability in production

## Problem

```toml
[profile.release]
strip = true
```

For a VMM running in production, stripping all debug info makes core dump analysis nearly impossible. When the VMM crashes, `mdb`/`gdb` cannot show function names, line numbers, or variable values.

## Fix

Option A — Strip debug info but keep symbol names:

```toml
[profile.release]
strip = "debuginfo"  # Keep symbol table, strip DWARF
```

Option B — Generate separate debug info (best of both worlds):

```toml
[profile.release]
strip = false
split-debuginfo = "packed"  # .dwp file alongside binary
```

Then strip the deployed binary but keep the `.dwp` file accessible for post-mortem analysis.

Option C — Keep `strip = true` but document that debug builds should be used for crash analysis. Add a `profile.release-debug` profile:

```toml
[profile.release-debug]
inherits = "release"
strip = false
debug = true
```
