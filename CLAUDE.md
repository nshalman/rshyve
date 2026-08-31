When working with virtio use 1.3 specification:  https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html

Propolis is the upstream reference: https://github.com/oxidecomputer/propolis

## Coding Standards

### Virtio Device Development
- VirtioDevice trait uses **u64** features (legacy transport truncates to u32)
- **Transitional PCI transport**: BAR0 (PIO) for legacy, BAR2 (MMIO 16KB) for modern. PCI capabilities at 0x40+ advertise modern config structures.
- **EVENT_IDX** (bit 29) and **INDIRECT_DESC** (bit 28) are included in device `device_features()` but **stripped by pci.rs for legacy transport**. Modern guests (VERSION_1) get them. Viona strips EVENT_IDX since it handles rings in-kernel.
- EVENT_IDX ring addresses: device reads `used_event` from **avail ring** (driver→device threshold), device writes `avail_event` to **used ring** (device→driver kick suppression). Getting these backwards was a critical bug.
- All guest memory access goes through `PhysMap::lookup()` → `SubMapping` with volatile reads/writes
- Descriptor chains use `collect_chain()` which handles both direct and indirect descriptors
- Async completions use `VirtioCompletion` (Mutex<u16> pattern from Propolis)
- Block device uses hybrid sync/async: ≤4 requests inline, >4 dispatched to workers

### Memory Safety
- `SubMapping::read<T>/write<T>` enforce alignment and use `read_volatile/write_volatile`
- `SubMapping::read_bytes/write_bytes` use byte-by-byte volatile access
- All integer arithmetic on guest-supplied values must use `checked_add/checked_mul`
- No `unsafe` raw pointer casts across thread boundaries — use `Arc` instead

### Error Handling
- Use `slog` structured logging everywhere (no `eprintln!` in production paths)
- Never silently swallow errors with `let _ =` on security-critical paths
- Migration thread spawn failures must roll back VM state and return error
- Control socket is restricted to 0600 permissions

### PCI / MSI-X
- MSI-X is optional (`msix: Option<Arc<MsixTable>>`) — pass `None` for legacy INTx
- When MSI-X is present, BAR0 device config shifts from 0x14 to 0x18
- `raise_queue_interrupt_for(queue_idx)` routes via MSI-X vector or falls back to INTx
- MSI-X table/PBA live in BAR4 (MMIO)

### Block Device Multiqueue
- `VirtioBlock::new()` takes `num_queues` parameter
- Per-queue worker pools: 4 workers for 1 queue, 2 per queue for MQ
- Per-queue `VirtioCompletion` instances (created lazily on first notification)
- `VIRTIO_BLK_F_MQ` advertised when num_queues > 1, config at offset 0x22

## Test infrastructure

Lab procedures, benchmarks, and the Windows boot harness are in
[docs/testing.md](docs/testing.md). Host addresses and keys are
variables there; keep the real values in `CLAUDE.local.md`, which is
gitignored.
