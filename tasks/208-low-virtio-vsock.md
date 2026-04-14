# 208 - LOW - Virtio-vsock device

## Problem

No host-guest communication channel exists outside of serial ports and network.
`virtio-vsock` (VirtIO socket) provides a lightweight socket interface between
host and guest without requiring network configuration.

Use cases:
- Guest agent communication (metadata, health checks)
- Log streaming from guest to host
- Replacing the mdata-over-COM2 hack with a proper channel

Propolis implements this as `PciVirtioSock`.

## Design

### VirtIO device type

- Device ID: 19 (Socket)
- PCI Device ID: 0x1012 (legacy) or 0x1053 (modern)
- 3 queues: RX (0), TX (1), Event (2)

### Config space

```rust
#[repr(C)]
struct VsockConfig {
    guest_cid: u64,  // Context ID assigned to guest
}
```

### Packet format (virtio_vsock_hdr)

```rust
#[repr(C)]
struct VirtioVsockHdr {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    type_: u16,     // VIRTIO_VSOCK_TYPE_STREAM (1)
    op: u16,        // REQUEST, RESPONSE, RST, SHUTDOWN, RW, CREDIT_UPDATE, CREDIT_REQUEST
    flags: u32,
    buf_alloc: u32, // Credit-based flow control
    fwd_cnt: u32,
}
```

### Host-side socket

On illumos, vsock may require a kernel driver or can be implemented in userspace
via a Unix socket multiplexer:

1. Host process listens on a Unix socket
2. VMM translates vsock packets to/from Unix socket connections
3. Guest CID is configured at VM creation

### Implementation approach

Start with a minimal implementation:
1. Negotiate features, handle queue setup
2. Respond to connection requests (OP_REQUEST → OP_RESPONSE or OP_RST)
3. Forward data between guest vsock and a host Unix socket
4. Implement credit-based flow control (buf_alloc / fwd_cnt)

## Files to create

- `crates/vmm-devices/src/virtio/vsock.rs` — VirtioVsock device implementation

## Files to modify

- `crates/vmm-devices/src/virtio/mod.rs` — Add `pub mod vsock;`
- `crates/vmm-devices/src/lib.rs` — Re-export
- `bin/vmm/src/main.rs` — Wire up vsock device with `-s N,virtio-vsock,guest_cid=3`
- `crates/vmm-config/src/lib.rs` — Parse vsock config

## Propolis reference

- `lib/propolis/src/hw/virtio/vsock.rs` — Full implementation
- `lib/propolis/src/vsock.rs` — Vsock protocol types

## Verification

1. Boot Linux guest with vsock device
2. Guest: `nc-vsock -l 1234`
3. Host: connect to guest CID via vsock proxy
4. Verify bidirectional data flow
