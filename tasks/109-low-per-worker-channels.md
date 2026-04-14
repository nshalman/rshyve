# PERF: Per-worker channels (eliminate Mutex contention)

**Priority:** Low
**File:** `crates/vmm-devices/src/virtio/block.rs`

## Problem

4 worker threads share one `mpsc::Receiver` behind `Arc<Mutex<mpsc::Receiver>>` (lines
102-103). Every `recv()` requires a Mutex acquire. Under high iodepth, workers contend
on this Mutex instead of doing I/O.

## Fix

Replace the single shared channel with per-worker channels and round-robin dispatch.

### VirtioBlock struct changes:

```rust
pub struct VirtioBlock {
    // Replace:  io_tx: mpsc::Sender<BlkIoRequest>,
    // With:
    io_txs: Vec<mpsc::Sender<BlkIoRequest>>,
    next_worker: std::sync::atomic::AtomicUsize,
    // ... rest unchanged
}
```

### In `VirtioBlock::new()`:

```rust
let mut io_txs = Vec::with_capacity(NUM_WORKERS);
for i in 0..NUM_WORKERS {
    let (tx, rx) = mpsc::channel::<BlkIoRequest>();
    io_txs.push(tx);
    let physmap = Arc::clone(&physmap);
    thread::Builder::new()
        .name(format!("virtio-blk-io-{i}"))
        .spawn(move || blk_io_worker_dedicated(rx, raw_fd, read_only, physmap))
        .expect("failed to spawn worker");
}
```

### Worker function (no Mutex needed):

```rust
fn blk_io_worker_dedicated(
    rx: mpsc::Receiver<BlkIoRequest>,  // owned, not Arc<Mutex<>>
    fd: i32,
    read_only: bool,
    physmap: Arc<PhysMap>,
) {
    loop {
        let req = match rx.recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        // ... same processing as before ...
    }
}
```

### Dispatch in `notify_queue()`:

```rust
for (head, header, chain) in requests {
    let worker = self.next_worker.fetch_add(1, Ordering::Relaxed) % NUM_WORKERS;
    let _ = self.io_txs[worker].send(BlkIoRequest { ... });
}
```

## Verification

- `cargo test -p vmm_devices` passes
- `fio --iodepth=128 --numjobs=4` -- all 4 workers active (dtrace thread probes)
- Compare p99 latency before/after under high load
