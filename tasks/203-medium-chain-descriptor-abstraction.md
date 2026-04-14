# 203 - MEDIUM - Chain descriptor abstraction for virtqueues

## Problem

vmmnew's virtio devices manually walk descriptor chains using raw `ChainBuf` enums:

```rust
pub enum ChainBuf {
    Readable { addr: u64, len: u32 },
    Writable { addr: u64, len: u32 },
}
```

Each device collects these into a `Vec<ChainBuf>` and processes them ad-hoc.
Problems:
- No byte accounting (how many read/write bytes remain?)
- No indirect descriptor support (VirtIO spec 2.7.5.3)
- Each device re-implements chain traversal
- Error-prone: easy to mix up readable vs writable regions

Propolis has a `Chain` type that encapsulates all of this.

## Design

### Add to `crates/vmm-devices/src/virtio/queue.rs`

```rust
/// A walked descriptor chain from the available ring.
///
/// Separates readable (device-reads-from-guest) and writable
/// (device-writes-to-guest) buffer regions. Tracks byte positions
/// for sequential read/write operations.
pub struct Chain {
    /// Descriptor head index (for pushing to used ring).
    head: u16,
    /// Readable regions (guest → device).
    read_bufs: Vec<(u64, u32)>,  // (gpa, len)
    /// Writable regions (device → guest).
    write_bufs: Vec<(u64, u32)>,
    /// Current read position (buf index, offset within buf).
    read_pos: (usize, u32),
    /// Current write position.
    write_pos: (usize, u32),
    /// Total bytes written (for used ring len field).
    bytes_written: u32,
}

impl Chain {
    /// Read data FROM the chain (guest-provided readable buffers) into `dst`.
    /// Returns number of bytes read.
    pub fn read(&mut self, dst: &mut [u8], physmap: &PhysMap) -> u32;

    /// Write data TO the chain (guest-provided writable buffers) from `src`.
    /// Returns number of bytes written.
    pub fn write(&mut self, src: &[u8], physmap: &PhysMap) -> u32;

    /// Read a fixed-size struct from the chain.
    pub fn read_obj<T: FromBytes>(&mut self, physmap: &PhysMap) -> Option<T>;

    /// Write a fixed-size struct to the chain.
    pub fn write_obj<T: AsBytes>(&mut self, val: &T, physmap: &PhysMap) -> bool;

    /// Remaining readable bytes.
    pub fn remain_read(&self) -> u32;

    /// Remaining writable bytes.
    pub fn remain_write(&self) -> u32;

    /// Descriptor head index.
    pub fn head(&self) -> u16;

    /// Total bytes written (for used ring entry).
    pub fn bytes_written(&self) -> u32;
}
```

### Pop with Chain

```rust
impl VirtQueue {
    /// Pop next available descriptor and walk the full chain.
    /// Returns None if queue is empty.
    pub fn pop_chain(&mut self, physmap: &PhysMap) -> Option<Chain> {
        let head = self.pop_avail(physmap)?;
        let chain = self.walk_chain(physmap, head);
        Some(chain)
    }

    /// Walk a descriptor chain, following NEXT flags.
    /// Also handles INDIRECT flag (descriptor points to indirect table).
    fn walk_chain(&self, physmap: &PhysMap, head: u16) -> Chain {
        let mut read_bufs = Vec::new();
        let mut write_bufs = Vec::new();
        let mut idx = head;
        loop {
            let desc = self.read_desc(physmap, idx);
            if desc.flags & VRING_DESC_F_INDIRECT != 0 {
                // Walk indirect table
                self.walk_indirect(physmap, desc.addr, desc.len,
                                   &mut read_bufs, &mut write_bufs);
            } else if desc.flags & VRING_DESC_F_WRITE != 0 {
                write_bufs.push((desc.addr, desc.len));
            } else {
                read_bufs.push((desc.addr, desc.len));
            }
            if desc.flags & VRING_DESC_F_NEXT == 0 { break; }
            idx = desc.next;
        }
        Chain { head, read_bufs, write_bufs, read_pos: (0, 0),
                write_pos: (0, 0), bytes_written: 0 }
    }
}
```

### Indirect descriptor support

VirtIO spec 2.7.5.3: A descriptor with `INDIRECT` flag points to a buffer containing
a table of descriptors (same format, but `addr` is GPA of the indirect table, `len`
is total byte size of indirect table). These are walked linearly (NEXT flag chains
within the indirect table).

```rust
fn walk_indirect(&self, physmap: &PhysMap, table_addr: u64, table_len: u32,
                 read_bufs: &mut Vec<(u64, u32)>,
                 write_bufs: &mut Vec<(u64, u32)>) {
    let count = table_len as usize / 16;  // 16 bytes per descriptor
    for i in 0..count {
        let desc_addr = table_addr + (i as u64 * 16);
        let desc: VirtqDesc = physmap.read_obj(desc_addr);
        if desc.flags & VRING_DESC_F_WRITE != 0 {
            write_bufs.push((desc.addr, desc.len));
        } else {
            read_bufs.push((desc.addr, desc.len));
        }
        if desc.flags & VRING_DESC_F_NEXT == 0 { break; }
    }
}
```

## Files to modify

- `crates/vmm-devices/src/virtio/queue.rs` — Add `Chain`, `pop_chain()`, `walk_chain()`, `walk_indirect()`
- `crates/vmm-devices/src/virtio/block.rs` — Refactor to use `Chain` instead of manual `collect_chain()`
- `crates/vmm-devices/src/virtio/rng.rs` — Refactor to use `Chain`

## Dependencies

- Consider adding `zerocopy` crate for `FromBytes`/`AsBytes` derives (or use existing `read_volatile` pattern)

## Propolis reference

- `lib/propolis/src/hw/virtio/queue.rs:656+` — Chain implementation
- `lib/propolis/src/hw/virtio/queue.rs:499-531` — Indirect descriptor handling

## Verification

1. `cargo test` — add unit tests for Chain with mock PhysMap
2. Test indirect descriptors: Linux guests use VIRTIO_RING_F_INDIRECT_DESC by default
3. Boot VM, run `fio` workload — verify block I/O still works with Chain path
4. Verify `remain_read() == 0` after fully consuming a request
