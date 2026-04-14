# 13 — Zero tests for 22 source files including security-critical ones

**Priority:** MEDIUM
**Category:** Testing — coverage gaps in critical infrastructure

## Problem

The following files have zero test modules. Many are on the security boundary (guest memory, instruction emulation, VM exits):

### Security-critical (must test):
| File | Lines | Why critical |
|------|-------|-------------|
| `crates/vmm-core/src/mem.rs` | 530 | Guest memory access — the trust boundary |
| `crates/vmm-core/src/inst_emul.rs` | 738 | Instruction emulation — incorrect emulation = guest escape |
| `crates/vmm-core/src/exits.rs` | 401 | VM exit parsing — wrong parsing = UB |
| `crates/vmm-core/src/vcpu.rs` | 291 | vCPU execution context |
| `crates/vmm-core/src/intr_pins.rs` | 411 | Interrupt routing — lost interrupts = guest hangs |

### Operational-critical (should test):
| File | Lines | Why critical |
|------|-------|-------------|
| `bin/vmm/src/main.rs` | 1253 | Entire VM lifecycle |
| `bin/vmm/src/vcpu_tasks.rs` | 332 | vCPU event loop |
| `bin/vmm/src/control.rs` | 620+ | Control socket — state machine correctness |
| `crates/vmm-migrate/src/source.rs` | 393 | Migration source — data integrity |
| `crates/vmm-migrate/src/destination.rs` | 381 | Migration destination |
| `crates/vmm-core/src/cpuid.rs` | 337 | CPUID masking |
| `crates/vmm-core/src/machine.rs` | 241 | VM builder |
| `crates/vmm-devices/src/metrics.rs` | 361 | Metrics — Prometheus format correctness |
| `crates/vmm-devices/src/virtio/viona.rs` | 583 | Kernel NIC driver |

## Fix — Priority order

### Phase 1: mem.rs tests (highest value)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submapping_bounds_check() {
        // Create a test mapping, verify read<u64> from too-small sub fails
    }

    #[test]
    fn submapping_alignment_check() {
        // Verify read<u64> from misaligned pointer fails (after task #03 fix)
    }

    #[test]
    fn physmap_overlap_rejection() {
        // Register overlapping regions, verify second is rejected
    }

    #[test]
    fn lookup_returns_none_for_unmapped() {
        // Verify lookup of unmapped GPA returns None
    }
}
```

### Phase 2: inst_emul.rs tests

Test each emulated instruction (MOV, MOVZX, MOVSX, XLATB, etc.) with known register states and expected outcomes.

### Phase 3: control.rs state machine tests

Test state transitions: Running -> Paused -> Running, Running -> Migrating, invalid transitions rejected.

### Phase 4: exits.rs tests

Test VM exit parsing with synthetic exit data for each exit type.

## Note

Many of these files depend on kernel ioctls (bhyve `/dev/vmm`). Tests need either:
- Mock `VmmHdl` trait (requires refactoring hdl.rs to use a trait)
- Integration test environment with actual bhyve (illumos only)
- Test only the pure logic (parsing, state machines) separate from ioctl calls
