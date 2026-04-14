# Vulnerability Report: `bin/vmm/src/main.rs`

## Vuln 1 — Unauthenticated VM Live Migration (Critical)

**Type:** Missing Authentication on Critical Network Service  
**Severity:** Critical  
**Lines:** 326–365

### Root Cause

The `--migrate-listen` path binds a raw TCP listener and accepts the first connection with **zero authentication, no TLS, and no peer verification**:

```rust
let listener = tokio::net::TcpListener::bind(&listen_addr_clone)
    .await?;

let (stream, peer) = listener.accept().await?;

vmm_migrate::destination::run_destination(
    stream,
    hdl.clone(),
    &vmm_core::mem::MemCtx::new(physmap),
    migrate_num_cpus,
    migrate_mem_size,
    status,
).await?;
```

The imported migration data directly overwrites:
- All guest physical memory (`MemCtx` / `PhysMap`)
- All vCPU register state (RIP, RSP, CR3, VMCS/VMCB fields)
- All device state (LAPIC, IOAPIC, PIT, RTC, etc.)

### Exploitation Strategy

Any host on the network that can reach the migration port achieves **full VM takeover**:

1. Scan for the migration listener (default: whatever `--migrate-listen` specifies, e.g. `0.0.0.0:6000`)
2. Connect before the legitimate migration source does (race — the listener accepts the **first** connection then stops)
3. Send a crafted migration stream that:
   - Writes shellcode into guest RAM at a known GPA
   - Sets BSP's RIP to that GPA, CR0/CR3/EFER for the desired execution mode
   - Sets RSP to valid guest stack
4. vCPUs resume with attacker-controlled state → arbitrary code execution inside the guest

**PoC sketch** (pseudocode):
```python
import socket, struct

sock = socket.connect(("target", 6000))
# Send migration protocol header (reverse-engineer from vmm_migrate crate)
# Phase 1: memory pages — write shellcode at GPA 0x100000
sock.send(migration_memory_frame(gpa=0x100000, data=shellcode))
# Phase 2: vCPU state — set RIP=0x100000, long mode enabled
sock.send(migration_vcpu_frame(rip=0x100000, cr0=0x80050033, efer=0xD01))
# Phase 3: device state — minimal valid LAPIC/IOAPIC state
sock.send(migration_device_frames())
```

---

## Vuln 2 — Arbitrary ACPI Table Injection via Fixed Path (High)

**Type:** Local Privilege Escalation / Arbitrary Code Injection  
**Severity:** High  
**Lines:** 248–253

### Root Cause

The VMM unconditionally reads a DSDT override from a **hardcoded path** with no integrity check, no feature flag, and no permission validation:

```rust
let dsdt_override = std::fs::read("/opt/vmmnew/test_dsdt.aml").ok();
// ...
let acpi_layout = acpi::generate_acpi_layout_with_dsdt(
    num_cpus,
    dsdt_override,   // attacker-controlled AML bytecode
);
```

### Exploitation Strategy

If a local attacker can write to `/opt/vmmnew/test_dsdt.aml` (misconfigured permissions, symlink, shared `/opt`):

1. Craft malicious AML bytecode using `iasl`
2. Write it to `/opt/vmmnew/test_dsdt.aml`
3. Wait for any VM to start — the DSDT is injected into the guest's ACPI tables
4. The guest OS executes the AML in its ACPI interpreter, which can perform arbitrary port I/O, memory read/writes, and SMM operations

This is particularly dangerous because:
- The `.ok()` silently swallows errors — no log entry on partial read or permission denial
- There is no signature or checksum verification on the loaded AML
- This is clearly debug/test code left in the production path

---

## Vuln 3 — TOCTOU Race on Migration Accept (Medium)

**Type:** Race Condition  
**Severity:** Medium  
**Lines:** 337–341, 375–382

The migration listener accepts exactly one connection, then the 200ms `thread::sleep` before LAPIC timer injection creates a window where vCPU state is partially initialized:

```rust
// vCPUs are activated + reset BEFORE migration data arrives (line 313-320)
// Then migration overwrites state
// Then:
std::thread::sleep(std::time::Duration::from_millis(200));
for vcpu in machine.vcpus() {
    machine.hdl().lapic_irq(vcpu.id(), 0xEC)?;  // timer kick
}
```

If vCPU threads haven't entered `VM_RUN` within 200ms (heavy load, scheduling delay), the LAPIC IRQ injection is lost, leaving the guest kernel without a timer tick — potential guest DoS.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | Unauthenticated migration listener | **Critical** | Yes — remote, network-accessible |
| 2 | DSDT override from hardcoded path | **High** | Yes — local, file write required |
| 3 | Migration timer race | **Medium** | Partial — guest DoS under load |

**The migration listener (Vuln 1) is the primary CTF target.** It gives full remote control over guest memory and CPU state with no credentials required.
