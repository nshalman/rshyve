# 200 - HIGH - Implement Lifecycle trait on all devices

## Problem

The `Lifecycle` trait is defined in `crates/vmm-devices/src/lifecycle.rs` (adopted from
Propolis) but **zero devices implement it**. The main loop in `bin/vmm/src/main.rs` does
not call `start()`/`pause()`/`resume()`/`halt()` on devices during state transitions.

Without this, pause/resume is not clean (workers keep running), migration cannot
reliably quiesce devices, and the `Indicator` state machine is unused.

## Files to modify

### Device impls (add `impl Lifecycle for ...`)

1. **`crates/vmm-devices/src/virtio/block.rs`** — VirtioBlock
   - `start()`: no-op (workers spawned at construction)
   - `pause()`: send poison pill to worker channels, set `paused` AtomicBool
   - `paused()`: return future that resolves when all 4 workers exit
   - `resume()`: re-spawn worker threads, clear `paused` flag
   - `halt()`: same as pause + drop channels

2. **`crates/vmm-devices/src/virtio/viona.rs`** — VirtioViona
   - `pause()`: stop interrupt polling thread
   - `paused()`: join polling thread handle
   - `resume()`: re-start polling thread
   - `halt()`: close viona fd

3. **`crates/vmm-devices/src/virtio/rng.rs`** — VirtioRng
   - Trivial: no background work, all defaults are fine
   - Just add `impl Lifecycle for VirtioRng { fn type_name(&self) -> &'static str { "virtio-rng" } }`

4. **`crates/vmm-devices/src/nvme/mod.rs`** — NvmeCtrl
   - `pause()`: stop 8 worker threads (similar pattern to VirtioBlock)
   - `paused()`: join all workers
   - `resume()`: re-spawn workers

5. **`crates/vmm-devices/src/xhci/mod.rs`** — XhciController
   - `pause()`: set stopped flag
   - Minimal: xHCI has no background threads currently

6. **`crates/vmm-devices/src/uart/lpc.rs`** — LpcUart
   - Trivial: synchronous device, defaults are fine

### Orchestration

7. **`bin/vmm/src/main.rs`** — Store devices in a `Vec<Arc<dyn Lifecycle>>` (or a `DeviceMap`)
   - After device creation, push each into the lifecycle list
   - On `VmState::Paused`: iterate and call `pause()`, then `await` all `paused()` futures
   - On `VmState::Running` (resume): iterate and call `resume()`
   - On shutdown: iterate and call `halt()`

8. **`bin/vmm/src/control.rs`** — `pause` and `resume` commands should drive lifecycle

## Implementation pattern (from Propolis)

```rust
// In VirtioBlock:
struct VirtioBlock {
    // ... existing fields ...
    indicator: Indicator,
    paused_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl Lifecycle for VirtioBlock {
    fn type_name(&self) -> &'static str { "virtio-blk" }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.indicator.pause();
        // Signal workers to stop accepting new work
        self.paused.store(true, Ordering::Release);
    }

    fn paused(&self) -> BoxFuture<'static, ()> {
        let (tx, rx) = oneshot::channel();
        *self.paused_tx.lock().unwrap() = Some(tx);
        // Workers check `paused` flag and send on tx when drained
        Box::pin(async move { let _ = rx.await; })
    }

    fn resume(&self) {
        self.indicator.resume();
        self.paused.store(false, Ordering::Release);
    }

    fn halt(&self) {
        self.indicator.halt();
    }
}
```

## Verification

1. `cargo build` / `cargo test`
2. Boot VM, issue `pause` via control socket, verify workers stop (DTrace: no more `vioblk_*` probes)
3. Issue `resume`, verify I/O resumes
4. Verify `halt` on shutdown tears down cleanly (no zombie threads)
