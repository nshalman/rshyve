# 204 - MEDIUM - Extract block backend trait

## Problem

`VirtioBlock` directly opens a `File` and issues `pread`/`pwrite` calls. The storage
backend is hardwired — no way to use a ramdisk, network block device, or zvol without
duplicating the entire VirtioBlock implementation.

Propolis separates this cleanly: `block::Backend` trait with multiple implementations
(FileBackend, InMemoryBackend, CrucibleBackend), attached to the virtio-blk device
via an `attachment` layer.

## Design

### New trait in `crates/vmm-devices/src/block.rs` (new module)

```rust
/// Information about a block backend.
pub struct BackendInfo {
    /// Total size in bytes.
    pub capacity: u64,
    /// Sector/block size in bytes.
    pub block_size: u32,
    /// Read-only flag.
    pub read_only: bool,
}

/// Operations that a block backend must support.
pub trait BlockBackend: Send + Sync + 'static {
    /// Query backend info (capacity, block size, read-only).
    fn info(&self) -> BackendInfo;

    /// Read `buf.len()` bytes starting at `offset`.
    fn read(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;

    /// Write `buf.len()` bytes starting at `offset`.
    fn write(&self, offset: u64, buf: &[u8]) -> std::io::Result<usize>;

    /// Flush pending writes to stable storage.
    fn flush(&self) -> std::io::Result<()>;

    /// Discard/UNMAP a region (optional, default no-op).
    fn discard(&self, _offset: u64, _len: u64) -> std::io::Result<()> {
        Ok(())
    }

    /// Write zeroes to a region (optional, default falls back to write).
    fn write_zeroes(&self, offset: u64, len: u64) -> std::io::Result<()> {
        let zeros = vec![0u8; len.min(1 << 20) as usize]; // 1MB chunks
        let mut remaining = len;
        let mut off = offset;
        while remaining > 0 {
            let chunk = remaining.min(zeros.len() as u64) as usize;
            self.write(off, &zeros[..chunk])?;
            off += chunk as u64;
            remaining -= chunk as u64;
        }
        Ok(())
    }
}
```

### FileBackend in `crates/vmm-devices/src/block/file.rs`

```rust
pub struct FileBackend {
    file: File,
    info: BackendInfo,
}

impl FileBackend {
    pub fn open(path: &Path, read_only: bool, block_size: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let capacity = file.metadata()?.len();
        Ok(Self {
            file,
            info: BackendInfo { capacity, block_size, read_only },
        })
    }
}

impl BlockBackend for FileBackend {
    fn info(&self) -> BackendInfo { self.info }
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        self.file.read_at(buf, offset)
    }
    fn write(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        self.file.write_at(buf, offset)
    }
    fn flush(&self) -> io::Result<()> { self.file.sync_all() }
}
```

### Refactor VirtioBlock

Change `VirtioBlock` to accept `Arc<dyn BlockBackend>`:

```rust
pub struct VirtioBlock {
    backend: Arc<dyn BlockBackend>,
    // ... completion, workers, etc remain the same
}

impl VirtioBlock {
    pub fn new(backend: Arc<dyn BlockBackend>, ...) -> Self { ... }
}
```

The inline fast-path and worker thread dispatch stay exactly the same — they just
call `backend.read()` / `backend.write()` instead of `file.read_at()` / `file.write_at()`.

### Wire up in main.rs

```rust
// Before:
let blk = VirtioBlock::new(path, ...);

// After:
let backend = Arc::new(FileBackend::open(path, read_only, 512)?);
let blk = VirtioBlock::new(backend, ...);
```

## Files to create

- `crates/vmm-devices/src/block.rs` or `crates/vmm-devices/src/block/mod.rs` — trait + BackendInfo
- `crates/vmm-devices/src/block/file.rs` — FileBackend

## Files to modify

- `crates/vmm-devices/src/lib.rs` — Add `pub mod block;`
- `crates/vmm-devices/src/virtio/block.rs` — Accept `Arc<dyn BlockBackend>` instead of File
- `bin/vmm/src/main.rs` — Construct FileBackend, pass to VirtioBlock

## Future backends (not in this task)

- `RamBackend` — in-memory, useful for testing and ephemeral disks
- `ZvolBackend` — direct zvol I/O (ioctl-based, bypasses filesystem)

## Verification

1. `cargo build` / `cargo test`
2. Boot VM with virtio-blk file backend — identical behavior to before
3. Benchmark: `fio` results should be unchanged (trait dispatch is negligible)
