# 08 — Control socket has zero authentication

**Priority:** HIGH
**File:** `bin/vmm/src/control.rs` (entire file)
**Category:** Security — local privilege escalation

## Problem

The Unix domain socket control interface accepts commands from any process that can connect. There is no authentication token, no HMAC, and no access control beyond filesystem DAC (which may be permissive in zone environments).

Dangerous commands available to any local process:
- `pause` / `resume` — DoS the VM
- `shutdown` / `stop` — kill the VM
- `migrate-source` — exfiltrate all VM memory to an attacker-controlled host
- `migrate-dest` — replace the VM with an attacker-controlled instance

## Fix

### Phase 1 — Socket permissions (immediate)

Set socket permissions to 0600 at creation time:

```rust
// After binding the socket
std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
```

Also verify ownership matches the VMM process uid.

### Phase 2 — Shared secret (short term)

Generate a random token at VMM startup, write it to a file readable only by the controlling process (e.g., `vmadm`):

```rust
let token = uuid::Uuid::new_v4().to_string();
std::fs::write(token_path, &token)?;
std::fs::set_permissions(token_path, Permissions::from_mode(0o600))?;
```

Require the token in every control socket command:

```json
{"command": "pause", "token": "uuid-here"}
```

### Phase 3 — Peer credential validation (longer term)

Use `SO_PEERCRED` / `getpeerucred()` on the Unix socket to verify the connecting process is running as the expected uid/gid. On illumos, use `ucred_get()` from `door_ucred`.
