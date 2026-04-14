# 207 - MEDIUM - Viona multiqueue support

## Problem

vmmnew's VirtioViona supports only 2 queues (1 RX + 1 TX). Modern guests (Linux 3.8+)
negotiate `VIRTIO_NET_F_MQ` and expect multiple queue pairs for better multi-core
network performance. Propolis supports up to 11 queue pairs (22 data queues + 1 control queue).

Single-queue viona means all network I/O serializes through one RX and one TX ring,
even when the guest has multiple vCPUs.

## Design

### Feature negotiation

```rust
// In virtio/bits.rs:
pub const VIRTIO_NET_F_MQ: u64 = 1 << 22;
pub const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;
```

### Queue layout with MQ

| Queue Index | Purpose |
|-------------|---------|
| 0 | RX pair 0 |
| 1 | TX pair 0 |
| 2 | RX pair 1 |
| 3 | TX pair 1 |
| ... | ... |
| 2*N | RX pair N |
| 2*N+1 | TX pair N |
| 2*N+2 | Control queue (if CTRL_VQ negotiated) |

### Config space

Add `max_virtqueue_pairs` at config offset 0x08 (u16):

```rust
fn cfg_read(&self, offset: u16, len: u8) -> u32 {
    match offset {
        0x00..=0x05 => /* MAC */,
        0x06..=0x07 => /* status */,
        0x08..=0x09 => self.max_queue_pairs as u32,
        _ => 0,
    }
}
```

### Control queue (virtio_net_ctrl)

The guest sends control commands via a dedicated virtqueue:

```rust
#[repr(C)]
struct VirtioNetCtrlHdr {
    class: u8,
    command: u8,
}

// Class VIRTIO_NET_CTRL_MQ (4), Command VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET (0)
// Payload: u16 num_pairs
```

When the guest sends `MQ_VQ_PAIRS_SET`, we tell the kernel how many pairs are active
via `VNA_IOC_SET_MQ_PAIRS` (if available in viona API).

### Viona kernel integration

Check viona API version for MQ support:
```rust
// VNA_IOC_GET_PARAMS with V3+ should report max_pairs capability
// VNA_IOC_SET_MQ_PAIRS to configure active pair count
```

If the kernel viona doesn't support MQ (older illumos), advertise `max_queue_pairs=1`
and don't offer `VIRTIO_NET_F_MQ`.

### VirtioViona struct changes

```rust
pub struct VirtioViona {
    inner: Mutex<VionaInner>,
    max_queue_pairs: u16,   // from viona capability query
    // ...
}

struct VionaInner {
    fd: RawFd,
    rings: Vec<VRing>,  // 2*max_pairs + 1 (if ctrl_vq)
    active_pairs: u16,
    // ...
}
```

## Dependencies

- Task 206 (VRing state machine) — each ring needs state tracking
- viona kernel API V3+ for multiqueue ioctls

## Files to modify

- `crates/vmm-devices/src/virtio/viona.rs` — Add MQ support, control queue, dynamic ring count
- `crates/vmm-devices/src/virtio/bits.rs` — Add VIRTIO_NET_F_MQ, CTRL_VQ feature bits
- `crates/viona-api/src/lib.rs` — Add MQ-related ioctl wrappers (if not present)

## Propolis reference

- `lib/propolis/src/hw/virtio/viona.rs:69-179` — Control queue command handling
- `lib/propolis/src/hw/virtio/viona.rs:245-298` — DeviceParams with MQ

## Verification

1. Boot Linux guest, verify `ethtool -l eth0` shows multiple channels
2. `ethtool -L eth0 combined 4` — set 4 queue pairs
3. `iperf3` with multiple threads — verify traffic spreads across queues
4. DTrace: `vmm:::viona_mq_set_pairs { printf("pairs=%d", arg0) }`
