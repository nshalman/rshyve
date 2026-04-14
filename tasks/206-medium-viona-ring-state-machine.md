# 206 - MEDIUM - Viona VRing state machine

## Problem

vmmnew's VirtioViona has no ring state validation. The kernel viona driver can receive
kicks for rings that haven't been fully configured (address not set, or ring reset
in progress). This can cause:

- Kicks to stale ring addresses after guest reset
- Race between queue setup and first notification
- No clean pause/resume path for migration

Propolis solves this with a per-ring state machine: `Init → Ready → Run → Paused → Error`.

## Design

### State enum in `crates/vmm-devices/src/virtio/viona.rs`

```rust
/// Per-ring state for viona kernel-accelerated queues.
#[derive(Debug, Clone, Copy, PartialEq)]
enum VRingState {
    /// Created but no address configured.
    Init,
    /// Address set, ready to be kicked.
    Ready,
    /// Actively processing (first kick received).
    Run,
    /// Paused for migration or reset.
    Paused,
    /// Fatal error — ring is unusable.
    Error,
}

impl VRingState {
    fn can_kick(&self) -> bool {
        matches!(self, Self::Ready | Self::Run)
    }

    fn can_set_addr(&self) -> bool {
        matches!(self, Self::Init | Self::Paused)
    }
}
```

### Per-ring tracking

```rust
struct VRing {
    state: VRingState,
    /// Kernel ring index (0=RX, 1=TX).
    idx: u16,
}

struct VionaInner {
    fd: RawFd,
    rings: [VRing; 2],  // RX and TX
    // ... existing fields
}
```

### State transitions

| Transition | Trigger | Action |
|-----------|---------|--------|
| Init → Ready | `queue_addr_set()` with valid PFN | Program ring in kernel via `VNA_IOC_RING_SET_ADDR` |
| Ready → Run | First `ring_kick()` | Just update state |
| Run → Paused | `pause()` (Lifecycle) | `VNA_IOC_RING_PAUSE` ioctl (if available) or stop polling |
| Paused → Ready | `resume()` after addr re-check | Re-program ring, clear state |
| Paused → Init | `reset()` | Clear ring addresses |
| * → Error | Kernel ioctl failure | Log error, refuse further kicks |

### Validation in hot path

```rust
fn ring_kick(&self, queue_idx: u16) {
    let inner = self.inner.lock().unwrap();
    let ring = &inner.rings[queue_idx as usize];
    if !ring.state.can_kick() {
        // Log warning, don't send ioctl to kernel
        slog::warn!(self.log, "kick on unconfigured ring";
            "queue" => queue_idx, "state" => ?ring.state);
        return;
    }
    // ... existing VNA_IOC_RING_KICK ioctl
}

fn queue_addr_set(&self, queue_idx: u16, queue: &VirtQueue) {
    let mut inner = self.inner.lock().unwrap();
    let ring = &mut inner.rings[queue_idx as usize];
    if !ring.state.can_set_addr() {
        slog::warn!(self.log, "addr set in wrong state";
            "queue" => queue_idx, "state" => ?ring.state);
        return;
    }
    // ... existing address programming
    ring.state = VRingState::Ready;
}
```

### Integration with Lifecycle (task 200)

```rust
impl Lifecycle for VirtioViona {
    fn type_name(&self) -> &'static str { "virtio-viona" }

    fn pause(&self) {
        let mut inner = self.inner.lock().unwrap();
        for ring in &mut inner.rings {
            if ring.state == VRingState::Run {
                ring.state = VRingState::Paused;
            }
        }
        // Stop interrupt polling thread
    }

    fn resume(&self) {
        let mut inner = self.inner.lock().unwrap();
        for ring in &mut inner.rings {
            if ring.state == VRingState::Paused {
                ring.state = VRingState::Ready;
            }
        }
        // Re-start interrupt polling thread
    }
}
```

## Files to modify

- `crates/vmm-devices/src/virtio/viona.rs` — Add VRingState, VRing struct, validation in kick/addr_set, Lifecycle impl

## Propolis reference

- `lib/propolis/src/hw/virtio/viona.rs:186-217` — VRingState enum and transitions

## Verification

1. Boot VM with viona NIC — normal operation unchanged
2. Issue `pause` via control socket → verify rings transition to Paused
3. Issue `resume` → rings back to Ready, traffic flows
4. Reset guest (reboot) → verify rings go through Init → Ready → Run cleanly
5. DTrace: `vmm:::viona_ring_state { printf("q%d %s->%s", arg0, arg1, arg2) }`
