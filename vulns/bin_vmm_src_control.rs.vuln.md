# Vulnerability Report: `control.rs`

## 1. TOCTOU Race Condition in State Machine (High)

**Affected functions:** `dispatch()` — `MigrateDest`, `Shutdown`, `Stop` handlers

**Root cause:** These commands use a load-then-store pattern instead of compare-and-swap (CAS), unlike `Pause`/`Resume`/`MigrateSource` which correctly use `ctrl.transition()`.

```rust
// MigrateDest (lines ~330-340) — TWO separate loads, then a blind store
if ctrl.state() != VmRunState::Paused       // load #1
    && ctrl.state() != VmRunState::Running   // load #2 — state can change between loads!
{
    return Response::err(...);
}
ctrl.state.store(VmRunState::Migrating as u8, Ordering::Release); // non-atomic with check
```

```rust
// Stop (lines ~265-270) — same pattern
let cur = ctrl.state();  // load
if cur == VmRunState::Stopped || cur == VmRunState::Stopping {
    return Response::err(...);
}
ctrl.state.store(VmRunState::Stopping as u8, Ordering::Release); // blind store
```

**Exploitation:** The migration threads use unconditional `state.store()` to reset state on completion/failure. This means a migration thread can **silently override a concurrent `stop` command**:

1. VM is migrating (`state = Migrating`)
2. Admin sends `stop` → check passes (Migrating != Stopped) → sets `state = Stopping` → calls `suspend()` ioctl
3. Migration thread fails → blindly stores `state = Running`, **overwriting Stopping**
4. VM appears "Running" despite the admin having issued a halt — the stop is lost

The double-load in `MigrateDest` also creates a window where the state changes between the two `ctrl.state()` calls, allowing the check to be bypassed or incorrectly rejected.

---

## 2. Use-After-Free via Raw Pointer (High)

**Affected functions:** `dispatch()` — `MigrateSource` and `MigrateDest` handlers

**Root cause:** The migration thread accesses `VmController.state` through a raw pointer cast to `usize`, bypassing Rust's ownership system. The spawned thread does **not** hold an `Arc<VmController>`:

```rust
// Line ~278
let state_ptr = &ctrl.state as *const AtomicU8 as usize;

thread::Builder::new().spawn(move || {
    // ... later, in multiple places:
    let state = unsafe { &*(state_ptr as *const AtomicU8) };  // dangling if VmController dropped
    state.store(VmRunState::Running as u8, Ordering::Release);
}).ok();
```

The thread clones `Arc<VmmHdl>` and `Arc<PhysMap>`, but the `state` field lives directly inside `VmController`, not behind its own `Arc`. If all `Arc<VmController>` references are dropped (e.g., the control thread exits on accept error at line ~420), the `VmController` is freed while the migration thread still holds `state_ptr`.

**Exploitation:** Trigger an accept error on the control socket (e.g., fd exhaustion) while a migration is in-flight. The control thread breaks out of its loop, drops `Arc<VmController>`, freeing the struct. The migration thread later writes through the dangling pointer — heap corruption, potentially redirectable to controlled memory.

---

## 3. Arbitrary Network Bind / SSRF via Migration (Medium)

**Affected functions:** `MigrateSource`, `MigrateDest` command handlers

**Root cause:** The `target_addr` and `listen_addr` fields are taken directly from the JSON command with no validation:

```rust
Command::MigrateSource { target_addr } => {
    // ...
    tokio::net::TcpStream::connect(&target_addr)  // attacker-controlled destination
}

Command::MigrateDest { listen_addr } => {
    // ...
    tokio::net::TcpListener::bind(&listen_addr)  // attacker-controlled bind address
}
```

**Exploitation:**
- **`migrate-source`**: SSRF — make the VMM connect to internal services (`127.0.0.1:6379`, metadata endpoints, etc.) and send migration protocol data to them
- **`migrate-dest`**: Bind `0.0.0.0:PORT` to open an unauthenticated migration listener to the network, allowing a remote attacker to inject arbitrary VM state/memory

**PoC:**
```bash
echo '{"command":"migrate-dest","listen_addr":"0.0.0.0:4444"}' | socat - UNIX-CONNECT:/path/to/control.sock
# Now anyone on the network can connect to port 4444 and send migration data
```

---

## 4. Symlink Race in Socket Creation (Medium)

**Affected function:** `spawn_control_thread()` (line ~408)

```rust
let _ = std::fs::remove_file(path);          // follows symlinks — deletes target!
if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
}
let listener = UnixListener::bind(path)?;
```

**Exploitation:** If an attacker can predict the socket path and place a symlink before the VMM starts:
```bash
ln -s /etc/important.conf /expected/socket/path
# VMM starts → remove_file follows symlink → deletes /etc/important.conf
```

---

## 5. Silent Thread Spawn Failure — State Stuck Forever (Low)

**Affected functions:** Both migration handlers

```rust
thread::Builder::new()
    .spawn(move || { /* ... */ })
    .ok();  // silently discards spawn failure!
```

If the thread fails to spawn (e.g., resource limits), the state is already set to `Migrating` and the response `{"success":true,"state":"migrating"}` has been returned. No thread exists to ever transition the state back. The VM is **permanently stuck in Migrating state** — all future `pause`, `resume`, `stop`, `shutdown` commands are rejected.

**PoC:** Exhaust thread limits before sending a `migrate-source` command. The VM becomes unmanageable.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | TOCTOU race — migration thread overwrites stop/shutdown | **High** | Yes — cancel admin stop commands |
| 2 | Use-after-free via raw pointer in migration threads | **High** | Yes — heap corruption if controller dropped |
| 3 | SSRF / arbitrary bind via migration addresses | **Medium** | Yes — open network listeners, scan internal hosts |
| 4 | Symlink race in socket creation | **Medium** | Yes — arbitrary file deletion |
| 5 | Silent spawn failure freezes state machine | **Low** | Yes — permanent DoS |

**Most likely CTF flag:** Vulnerability #1 (TOCTOU) or #3 (SSRF/arbitrary bind) — they're the easiest to demonstrate with a concrete exploit. The fix for #1 is to use `compare_exchange` everywhere (like `Pause`/`Resume` already do) and have migration threads also use CAS instead of blind stores.
