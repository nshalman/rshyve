# 17 — Block device multi-queue (MQ) support

**Spec:** Section 5.2.3.1 — `VIRTIO_BLK_F_MQ` feature for multiple request queues.
**Severity:** Medium (performance feature — eliminates single-queue lock contention)
**Effort:** ~100 lines

## Problem

The block device uses a single request queue (queue 0) with 4 worker threads sharing one `mpsc::Receiver` behind an `Arc<Mutex<Receiver>>` (`block.rs:103`). Under multi-vCPU workloads this creates two bottlenecks:

1. **Queue lock contention**: all vCPUs serialize on `virtio_state.lock()` in `bar_write` (`pci.rs:341`) when kicking queue 0.
2. **Worker Mutex contention**: all 4 workers serialize on the `mpsc::Receiver` Mutex (`block.rs:358`) to dequeue the next I/O request.

With MQ, each vCPU (or group of vCPUs) gets its own virtqueue, its own `VirtioCompletion`, and its own worker pool. No shared locks on the data path.

The `VirtioPciDevice` already accepts a `num_queues` parameter and allocates multiple `VirtQueue` instances (`pci.rs:55-58`). The block device just needs to use more than 1 queue and advertise `VIRTIO_BLK_F_MQ`.

## Spec rules

1. If `VIRTIO_BLK_F_MQ` is negotiated, `num_queues` at config offset 0x22 tells the driver how many request queues the device provides.
2. Each queue operates independently — the driver may submit requests to any queue.
3. The device MUST be able to handle requests on any queue regardless of which vCPU submits them.
4. Queue 0 is always the first request queue (no control queue for basic block).

## Files to modify

1. `crates/vmm-devices/src/virtio/bits.rs` — add `VIRTIO_BLK_F_MQ` constant
2. `crates/vmm-devices/src/virtio/block.rs` — multiple worker pools, per-queue completion, config `num_queues` field
3. `bin/vmm/src/main.rs` — pass `num_queues` to `VirtioPciDevice::new()`

## Changes

### bits.rs — add feature bit

```rust
/// Block device supports multiple request queues.
pub const VIRTIO_BLK_F_MQ: u64 = 1 << 12;
```

### block.rs — per-queue workers and config

Change from a single shared channel to a per-queue channel:

```rust
pub struct VirtioBlock {
    fd: i32,
    capacity: u64,
    sector_size: u32,
    read_only: bool,
    nodelete: bool,
    num_queues: u16,
    negotiated_features: Mutex<u32>,
    /// Per-queue I/O channels. Index by queue_idx.
    io_txs: Vec<mpsc::Sender<BlkIoRequest>>,
    physmap: Arc<PhysMap>,
    pub interrupt: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
    /// Per-queue completion handlers, created lazily.
    completions: Mutex<Vec<Option<Arc<VirtioCompletion>>>>,
    _file: File,
}
```

Update `new()` to create per-queue worker pools:

```rust
pub fn new(
    file: File,
    opts: &VirtioBlockOpts,
    physmap: Arc<PhysMap>,
    num_queues: u16,
) -> std::io::Result<Self> {
    // ...existing setup...

    let mut io_txs = Vec::with_capacity(num_queues as usize);

    for q in 0..num_queues {
        let (tx, rx) = mpsc::channel::<BlkIoRequest>();
        let rx = Arc::new(Mutex::new(rx));

        // 2 workers per queue (scale down from 4-for-1 to keep
        // total thread count reasonable: 2 * num_queues)
        let workers_per_queue = if num_queues == 1 { 4 } else { 2 };
        for i in 0..workers_per_queue {
            let rx = Arc::clone(&rx);
            let physmap = Arc::clone(&physmap);
            thread::Builder::new()
                .name(format!("virtio-blk-q{q}-io-{i}"))
                .spawn(move || {
                    blk_io_worker(rx, raw_fd, read_only, physmap);
                })
                .expect("failed to spawn virtio-blk I/O worker");
        }
        io_txs.push(tx);
    }

    Ok(Self {
        // ...
        num_queues,
        io_txs,
        completions: Mutex::new(vec![None; num_queues as usize]),
        // ...
    })
}
```

Update `get_completion()` to be per-queue:

```rust
fn get_completion(
    &self,
    queue_idx: u16,
    queue: &VirtQueue,
) -> Arc<VirtioCompletion> {
    let mut slots = self.completions.lock().expect("completions lock");
    let slot = &mut slots[queue_idx as usize];
    if let Some(ref c) = *slot {
        return Arc::clone(c);
    }

    let intr = Arc::clone(&self.interrupt);
    let interrupt_fn = move || {
        if let Ok(guard) = intr.lock() {
            if let Some(ref f) = *guard {
                f();
            }
        }
    };

    let c = VirtioCompletion::new(queue, Arc::clone(&self.physmap), interrupt_fn);
    *slot = Some(Arc::clone(&c));
    c
}
```

Update `notify_queue()` to dispatch to the correct per-queue channel:

```rust
fn notify_queue(
    &self,
    queue_idx: u16,
    queues: &mut [VirtQueue],
    physmap: &PhysMap,
) -> bool {
    let queue = match queues.get_mut(queue_idx as usize) {
        Some(q) => q,
        None => return false,
    };

    let completion = self.get_completion(queue_idx, queue);
    let tx = match self.io_txs.get(queue_idx as usize) {
        Some(tx) => tx,
        None => return false,
    };

    // ... rest of the method is the same, but uses `tx` for dispatch
    // instead of the single `self.io_tx` ...
}
```

Update `cfg_read()` to report `num_queues` at config offset 0x22:

```rust
fn cfg_read(&self, offset: u16, len: u8) -> u32 {
    let mut config = [0u8; 36]; // extended from 28 to include num_queues
    config[..8].copy_from_slice(&self.capacity.to_le_bytes());
    if self.sector_size != 512 {
        config[0x18..0x1C].copy_from_slice(&self.sector_size.to_le_bytes());
    }
    // num_queues at offset 0x22 (2 bytes LE)
    if self.num_queues > 1 {
        config[0x22..0x24].copy_from_slice(&self.num_queues.to_le_bytes());
    }
    // ... read from config array as before ...
}
```

Update `device_features()`:

```rust
fn device_features(&self) -> u32 {
    let mut f = bits::VIRTIO_BLK_F_FLUSH as u32;
    if self.read_only { f |= bits::VIRTIO_BLK_F_RO as u32; }
    if self.sector_size != 512 { f |= bits::VIRTIO_BLK_F_BLK_SIZE as u32; }
    if self.num_queues > 1 { f |= bits::VIRTIO_BLK_F_MQ as u32; }
    f
}
```

Update `config_size()` to include the `num_queues` field:

```rust
pub fn config_size(&self) -> u16 {
    if self.num_queues > 1 {
        // Config through num_queues at offset 0x22 (2 bytes) = 0x24
        0x24
    } else if self.sector_size != 512 {
        BLK_CONFIG_SIZE_EXT
    } else {
        BLK_CONFIG_SIZE
    }
}
```

Update `reset()` to clear all per-queue completions:

```rust
fn reset(&self) {
    *self.negotiated_features.lock().expect("lock") = 0;
    let mut slots = self.completions.lock().expect("lock");
    for slot in slots.iter_mut() {
        *slot = None;
    }
}
```

### main.rs — pass num_queues

```rust
// When creating the block device, use vcpu_count queues (capped at 4):
let blk_queues = vcpu_count.min(4) as u16;
let block = VirtioBlock::new(disk_file, &blk_opts, physmap.clone(), blk_queues)?;
let blk_pci = VirtioPciDevice::new(
    block,
    bits::VIRTIO_DEV_TYPE_BLOCK,
    blk_queues as usize,  // was: 1
    256,
    block.config_size(),
    // ...
);
```

## Dependencies

- None. MQ works with the current legacy PCI transport. The guest driver discovers the queue count via config space and uses `queue_select` to configure each queue.
- Task 16 (MSI-X) complements this well: with MSI-X each queue gets its own interrupt vector, avoiding shared-IRQ overhead. But MQ provides value without MSI-X.

## Test

```rust
#[test]
fn mq_feature_offered_when_multi_queue() {
    // Create VirtioBlock with num_queues=2
    // device_features() should include VIRTIO_BLK_F_MQ bit
}

#[test]
fn mq_feature_not_offered_when_single_queue() {
    // Create VirtioBlock with num_queues=1
    // device_features() should NOT include VIRTIO_BLK_F_MQ bit
}

#[test]
fn mq_config_reports_num_queues() {
    // Create VirtioBlock with num_queues=4
    // cfg_read(0x22, 2) should return 4
}

#[test]
fn mq_config_size_includes_num_queues_field() {
    // Create VirtioBlock with num_queues=2
    // config_size() should be 0x24 (includes num_queues at 0x22)
}
```

Run: `cargo test -p vmm-devices -- block`
