# Live Migration: Technical Implementation Details

This document provides a deep technical reference for the live migration
implementation in the Rust bhyve VMM on illumos/SmartOS. It is intended for
engineers working on VM live migration with kernel-accelerated virtio (viona),
NVMe device emulation, and ZFS disk replication.

For the high-level protocol overview and usage instructions, see
[migration.md](migration.md).


## Table of Contents

1. [System Architecture](#system-architecture)
2. [Process Topology](#process-topology)
3. [Migration Protocol in Detail](#migration-protocol-in-detail)
4. [Wire Format](#wire-format)
5. [RAM Transfer](#ram-transfer)
6. [ZFS Disk Replication](#zfs-disk-replication)
7. [Pause Ordering](#pause-ordering)
8. [Viona Ring State (The Hardest Problem)](#viona-ring-state-the-hardest-problem)
9. [NVMe Controller State Migration](#nvme-controller-state-migration)
10. [Virtio-blk Feature Flag Propagation](#virtio-blk-feature-flag-propagation)
11. [Time Synchronization](#time-synchronization)
12. [Kernel Device State](#kernel-device-state)
13. [Post-Restore Interrupt Injection](#post-restore-interrupt-injection)
14. [CPU Feature Compatibility](#cpu-feature-compatibility)
15. [Error Recovery](#error-recovery)
16. [Synchronous Transport (No Tokio in Zones)](#synchronous-transport-no-tokio-in-zones)
17. [Comparison with Propolis](#comparison-with-propolis)
18. [Measured Timing](#measured-timing)
19. [Key Source Files](#key-source-files)
20. [Lessons Learned](#lessons-learned)


## System Architecture

The VMM is a Rust userspace process that runs inside a bhyve zone on illumos.
It emulates the following devices:

- **NVMe disk controller** -- 8 worker threads performing `preadv`/`pwritev`
  against a ZFS zvol block device
- **Virtio-net via viona** -- kernel-accelerated networking where the illumos
  kernel owns the ring threads and processes packets directly
- **Virtio-blk** -- userspace virtio block device using a hybrid sync/async
  model with `VirtioCompletion` (Mutex<u16> pattern from Propolis)
- **Serial console, framebuffer, XHCI tablet** -- standard device emulation

Live migration must transfer the complete state of all these devices, plus
guest RAM, vCPU registers, and system devices (IOAPIC, LAPIC, timers), from
one physical host to another while the guest experiences minimal downtime.


## Process Topology

Migration involves **two hosts** and **four processes**:

```
Source Host                         Destination Host
+---------------------------+       +---------------------------+
| Global Zone               |       | Global Zone               |
|                           |       |                           |
| orchestrator, source side | TCP   | orchestrator, dest side   |
|   - ZFS send/recv         |<----->|   - ZFS recv              |
|   - Unix<->TCP bridge     |       |   - Unix<->TCP bridge     |
|                           |       |                           |
| +---bhyve zone----------+|       |+---bhyve zone-----------+ |
| | Source VMM             ||       || Dest VMM               | |
| |  - exports RAM         || Unix  ||  - imports RAM          | |
| |  - exports device state||<----->||  - imports device state | |
| |  - pauses vCPUs        ||bridge ||  - resumes vCPUs        | |
| +------------------------+|       |+------------------------+ |
+---------------------------+       +---------------------------+
```

Why four processes? The agents run in the **global zone** (GZ) because:

- ZFS `send`/`recv` operations require GZ privileges
- Cross-host TCP networking requires GZ network access
- The bhyve zone is a restricted environment without `/dev/poll`

The VMMs run inside bhyve zones. Communication between a VMM and its
co-located agent uses Unix domain sockets. The agents bridge Unix sockets
to inter-host TCP.

The source agent orchestrates the overall flow:

1. ZFS full send + iterative incrementals (pre-pause)
2. Start destination VM via SSH (`vmadm start`)
3. Connect to destination agent TCP
4. Trigger `migrate-source` on the source VMM via control socket
5. Bridge VMM Unix socket to agent TCP throughout the RAM/state transfer
6. Post-pause ZFS barrier for final incremental
7. Clean up snapshots after completion


## Migration Protocol in Detail

The protocol uses WebSocket binary frames with RON (Rusty Object Notation)
serialization. It runs over a synchronous path -- no tokio or epoll is
required, which is critical because bhyve zones lack `/dev/poll`.

### Phase 1: Sync

Protocol negotiation and preamble exchange.

```
Source                              Destination
  |--- ProtocolOffer ["vmm-migrate-ron/3"] -->|
  |<-- ProtocolSelect "vmm-migrate-ron/3" ----|
  |--- MigrationPreamble (cpus, mem, devices, features) -->|
  |<-- Okay ----------------------------------|
```

The preamble includes:

- `num_cpus` and `mem_size` -- must match exactly or migration is rejected
- `devices` -- every device that carries state, as a PCI bus/device/function
  plus its type name. The destination refuses a set that is not its own,
  before it accepts a single page
- `cpu_features` -- CPUID leaf 1 ECX/EDX, leaf 7 EBX/ECX/EDX, and XCR0
  bits masked by the source's `--cpu-baseline` profile

Both ends report their post-mask tables, because that is what the guest
saw and what it will see. The destination refuses any bit the source has
and it lacks; `allow_cpu_feature_mismatch` on the `migrate-dest` command
overrides that and logs the missing bits.

### Phase 2: Pre-Pause RAM (RamPushPrePause)

Full RAM push while the VM is still running. All pages are sent in batches
of 64 pages (256 KB per batch), optionally zstd-compressed at level 1.

```
Source                              Destination
  |--- PageBatch (base_gpa, count, data) ---->|
  |--- PageBatch ... (all pages) ------------>|
  |--- MemEnd -------------------------------->|
  |<-- MemDone --------------------------------|
```

After the full push, iterative convergence passes send only dirty pages.
The kernel's `VM_TRACK_DIRTY_PAGES` ioctl returns a bitmap of pages
modified since the last query. Each pass:

1. Sleep 500ms (let the guest dirty more pages, accumulate a meaningful delta)
2. Query dirty bitmap
3. Send dirty pages as sparse batches
4. Log the dirty page count

Convergence stops when dirty pages drop below 1024, or after 5 iterations.
Between passes, both contiguous and sparse page batches are used: contiguous
pages share a single `PageBatch` message; non-contiguous pages use a
`MemFetch` message (list of GPAs) followed by a `PageBatch`.

### Phase 3: Pause

The source sends a `PauseSignal` message to the destination, then:

1. **Pause device rings** -- `pause_for_migration()` on all devices
2. **Pause vCPUs** -- `VM_PAUSE` kernel ioctl

This ordering is critical. See [Pause Ordering](#pause-ordering).

### Phase 4: Post-Pause ZFS Barrier

After vCPU pause, the VMM connects to a barrier Unix socket. The GZ agent
accepts the connection, performs a final ZFS incremental send/recv, then
writes a single byte back. The VMM blocks on read until this byte arrives.

```
Source VMM (paused)          Source GZ Agent
  |-- connect(barrier.sock) -->|
  |   (blocking read)          | zfs snapshot @vmm-mig-postpause
  |                            | zfs send -i @vmm-mig-final @postpause | ssh dest 'zfs recv -F'
  |<-- [0x01] (done) ---------|
  |   (continues protocol)     |
```

This mechanism ensures the destination's zvol contains every disk write
the guest performed up to the exact pause point. Without it, the guest's
filesystem journal (ext4 jbd2, ZFS ZIL) may reference data blocks that
were written after the last pre-pause ZFS incremental but before the pause.

### Phase 5: Post-Pause RAM (RamPushPostPause)

One final dirty-page pass captures pages modified between the last
convergence pass and the vCPU pause. Typically a small number of pages.

### Phase 6: Time Data

TSC offset and `boot_hrtime` adjustment. See
[Time Synchronization](#time-synchronization).

### Phase 7: Device State

A single `Serialized` message containing all kernel and userspace device
state:

- Per-vCPU: registers, MSRs, LAPIC, VMM_ARCH (pending interrupts),
  run state (RUN/HALT/INIT), SIPI vector
- System devices: IOAPIC, ATPIT, ATPIC, HPET, PM timer, RTC
- Virtio queue state: ring addresses, avail/used indices, MSI-X vectors,
  PCI BAR addresses
- NVMe controller state: registers, queue state, MSI-X table

Device queue state is exported **after** vCPU pause so ring indices
reflect the quiesced state. If exported before pause, kernel ring threads
continue consuming entries, leaving the destination with stale indices.

### Phase 8: Resume

The destination:

1. Pauses the local VM (it was created in Init state)
2. Imports time data
3. Imports kernel device state (system devices, then per-vCPU)
4. Restores virtio queue state and NVMe controller state
5. Resumes vCPUs

The guest continues running on the destination host.


## Wire Format

Each WebSocket binary frame consists of a payload followed by a single
tag byte identifying the message type. The tag is at the **end** of the
frame so variable-length payloads need no separate length prefix.

```
[payload bytes (0..N)] [tag: 1 byte]
```

Message types (from `codec.rs`):

| Tag | Name       | Payload |
|-----|------------|---------|
| 0   | Okay       | (empty) |
| 1   | Error      | RON-encoded MigrateError |
| 2   | Serialized | RON-encoded structured data |
| 11  | PageBatch  | `[base_gpa: u64][page_count: u32][flags: u32][data...]` |
| 12  | PauseSignal| (empty) |
| 7   | MemFetch   | RON-encoded Vec<u64> of GPAs |
| 9   | MemEnd     | (empty) |
| 10  | MemDone    | (empty) |

PageBatch flags:

| Bit | Meaning |
|-----|---------|
| 0   | Data is zstd-compressed (decompress to `page_count * 4096` bytes) |

Structured data (preamble, time data, device state) is RON-serialized into
`Serialized` messages. RON was chosen over bincode/postcard because it
produces human-readable wire dumps during debugging, at the cost of slightly
larger payloads for metadata messages (RAM pages are binary regardless).


## RAM Transfer

### Dirty Page Tracking

The kernel provides `VM_TRACK_DIRTY_PAGES` which returns a bitmap of pages
modified since the last call. Each bit in the bitmap corresponds to a 4 KiB
page. The bitmap is allocated in userspace and passed to the kernel:

```rust
let bm_len = bitmap_size(region_len);  // (num_pages + 7) / 8
let mut bm = vec![0u8; bm_len];
hdl.track_dirty_pages(gpa, len, &mut bm)?;
```

### Batching and Compression

Pages are sent in batches of up to 64 (configurable via `BATCH_SIZE`).
Each batch is optionally compressed with zstd at level 1. Compression is
only used if the compressed output is smaller than the raw input -- this
avoids expanding incompressible data (encrypted or random pages).

```rust
fn compress(raw: &[u8]) -> (Vec<u8>, u32) {
    match zstd::bulk::compress(raw, ZSTD_LEVEL) {
        Ok(c) if c.len() < raw.len() => (c, PAGE_BATCH_FLAG_ZSTD),
        _ => (raw.to_vec(), 0),
    }
}
```

### Sparse Page Transfer

During convergence passes, only dirty pages are sent. If the dirty pages
are non-contiguous, a `MemFetch` message (containing the list of GPAs)
precedes the `PageBatch`. The destination uses the GPA list to write each
page to the correct location.

If the dirty pages happen to be contiguous, a single `PageBatch` with
`base_gpa` and `page_count` suffices (no `MemFetch` needed).


## ZFS Disk Replication

The guest's disk is a ZFS zvol. Replication uses `zfs send`/`zfs recv`
piped over SSH between the GZ agents.

### Replication Phases

| Phase | When | Duration (10GB zvol) |
|-------|------|---------------------|
| Full send (`@vmm-mig-base`) | Pre-pause | ~40s |
| Incremental 1-3 (`@vmm-mig-iter-N`) | Pre-pause (1s apart) | ~1s each |
| Final pre-pause (`@vmm-mig-final`) | Pre-pause | ~1s |
| Post-pause (`@vmm-mig-postpause`) | Post-pause (barrier) | ~1s |

### Snapshot Lifecycle

```
Source:  @vmm-mig-base -> @vmm-mig-iter-1 -> ... -> @vmm-mig-final -> @vmm-mig-postpause
                          (each iter destroys the previous snapshot)
```

Old migration snapshots are cleaned from both source and destination before
starting, to handle interrupted previous migrations.

### Barrier Mechanism

The post-pause ZFS sync is the critical piece that ensures disk consistency.
The mechanism works as follows:

1. Source agent creates a Unix socket at
   `/zones/<uuid>/root/tmp/vmm-migrate-zfs-barrier.sock`
2. Source agent spawns a `zfs-barrier` thread that blocks on `accept()`
3. Source agent triggers `migrate-source` on the VMM, passing the barrier
   path in the command JSON
4. VMM runs the migration protocol, pauses vCPUs, then `connect()`s to
   the barrier socket
5. The barrier thread wakes up, runs `zfs snapshot + zfs send -i`, then
   writes `[0x01]` back to the VMM
6. VMM reads the byte, unblocks, and continues with post-pause RAM transfer

**Critical constraint**: The destination VM must NOT be running during ZFS
recv (the zvol would be "busy" and recv would fail). The source agent starts
the destination VM only after ZFS sync is complete. On the destination, the
VMM is started with `--migrate-listen` which creates the VM instance (RAM,
devices) but does not start vCPUs -- it waits for the migration protocol
to import state.


## Pause Ordering

Device workers and rings must be paused **before** vCPUs. This is
implemented in `source.rs`:

```rust
set_phase(status, MigrationPhase::Pause);
device_pre_pause_fn()?;  // Park workers, wait for quiesce, then
                         // VNA_IOC_RING_PAUSE each kernel ring
hdl.pause()?;            // Pause vCPUs (VM_PAUSE)
```

The final dirty pass then runs after every writer has stopped, by
construction rather than as a side effect of the flush. Before that
change it ran while the console and vsock reader threads could still
fill a receive ring, so the exported `used_idx` could count an entry
whose ring slot and buffer were never shipped.

### Why This Order Matters

If vCPUs are paused first and rings paused second, there is a window where:

1. vCPUs freeze mid-execution (e.g., the guest's NAPI handler is halfway
   through processing rx buffers)
2. Viona's kernel ring thread continues consuming avail ring entries
3. The guest has detached descriptors from the ring (they are being
   processed by the guest driver) but the ring thread doesn't know this
4. On the destination, these descriptors appear in the avail ring from a
   **previous posting cycle** (descriptor IDs are reused)
5. The dest kernel consumes them, returns them via the used ring, but the
   guest's `desc_state` for those IDs is NULL
6. Result: "id N is not a head!" crash in the guest's virtio driver

By pausing rings first, the kernel stops consuming while the guest is
still running. The guest may notice a brief rx stall (tens of milliseconds
before the vCPU pause follows), but this is harmless and the migration
completes immediately after.

### Implementation

`pause_for_migration()` on the `VirtioPciDevice` calls through to the
backend's `pause_rings_for_export()`. For viona, this issues
`VNA_IOC_RING_PAUSE` on each queue. For virtio-blk (userspace), the
`VirtioCompletion` quiescing handles in-flight requests.


## Viona Ring State (The Hardest Problem)

Viona is a kernel-accelerated virtio-net driver in illumos. The kernel
manages its own ring threads that independently consume avail ring entries
and deliver packets. After migration, the destination's viona kernel
driver must resume packet delivery with correct ring state.

### The Virtio Ring Model

```
Guest (driver)                    Host (device / viona kernel)
  |                                 |
  | posts rx buffers to avail ring  |
  |-------- avail_ring->idx ------->|
  |                                 | consumes buffers, fills with packets
  |                                 | returns via used ring
  |<------- used_ring->idx ---------|
  | processes returns, reposts      |
  |                                 |
  (continuous cycle)
```

The kernel maintains `vr_cur_aidx` (its own cursor into the avail ring)
which tracks how far it has consumed. The delta between the guest's
`avail_ring->idx` (in guest memory) and `vr_cur_aidx` represents entries
posted by the guest but not yet consumed by the kernel.

### What We Tried (and Why It Failed)

**Attempt 1: Export `avail_ring->idx` from guest memory.**
Set the kernel's avail tracking to the guest memory value. This makes the
kernel think it has consumed everything the guest posted, leaving zero
available buffers. Network is dead on destination.

**Attempt 2: Synthesize used ring entries.**
Write descriptor IDs from the avail ring into the used ring for the gap
between `used_idx` and `avail_idx`. Failed because descriptor IDs are
**reused across ring cycles** -- the same ID appears in both real and
synthetic entries, causing "id N is not a head!" (double-completion) in
the guest's virtio driver.

**Attempt 3: Set kernel `avail = used_idx`.**
Make the kernel re-consume descriptors from `used_idx` forward. Some
descriptors were already detached by the guest (mid-NAPI at vCPU pause),
so the kernel returns descriptors the guest's driver has already freed.
Same "id N is not a head!" crash.

**Attempt 4: Single-entry kickstart.**
Write one used entry to trigger NAPI refill. Same descriptor-reuse
problem.

### What Works

Export `vr_cur_aidx` (the kernel's actual consumed position) via
`VNA_IOC_RING_GET_STATE` instead of reading `avail_ring->idx` from guest
memory. On the destination, set `vrs_avail_idx` to this value via
`VNA_IOC_RING_SET_STATE`.

The delta between `vr_cur_aidx` and the guest's `avail_ring->idx`
represents entries the guest posted but the kernel hasn't consumed yet.
On the destination, these entries are immediately available -- their
descriptors are valid because the guest set them up and the source kernel
never touched them.

**Example**: `k_avail = 1844`, `guest_avail = 2071`. On the destination,
the kernel sees 227 immediately available rx buffers.

### Kernel Ioctls

| Ioctl | Purpose |
|-------|---------|
| `VNA_IOC_RING_PAUSE` | Pause kernel ring thread (stop consuming avail entries) |
| `VNA_IOC_RING_GET_STATE` | Read `vr_cur_aidx` (consumed) and `vr_cur_uidx` (returned) |
| `VNA_IOC_RING_SET_STATE` | Set ring addresses + avail/used indices on destination |
| `VNA_IOC_RING_SET_MSI` | Program MSI-X interrupt delivery per queue |
| `VNA_IOC_SET_NOTIFY_IOP` | Program PIO notification port (legacy transport) |
| `VNA_IOC_SET_NOTIFY_MMIO` | Program MMIO notification address (modern transport) |
| `VNA_IOC_RING_KICK` | Start ring worker (transitions VRS_INIT to VRS_RUN) |

### Restoration Sequence on Destination

```
1. reset_all_rings()          -- RING_PAUSE + RING_RESET on all queues
2. set_kernel_features()      -- program VERSION_1/negotiated features
3. ring_set_state() per queue -- set addresses, sizes, avail_idx, used_idx
4. ring_set_msi() per queue   -- program MSI-X addr/data for interrupts
5. ring_kick() per queue      -- start kernel ring workers
6. set_notify_addrs()         -- program PIO port and/or MMIO address
7. start_intr_poll()          -- start userspace interrupt polling thread
```


## NVMe Controller State Migration

The NVMe controller is fully emulated in userspace with 8 I/O worker
threads per controller. After migration, the controller must resume from
the source's exact state.

### State Serialized

```rust
NvmeControllerState {
    cc: u32,           // Controller Configuration
    csts: u32,         // Controller Status
    aqa: u32,          // Admin Queue Attributes
    asq_base: u64,     // Admin SQ base address
    acq_base: u64,     // Admin CQ base address
    admin_sq: Option<NvmeSqState>,
    admin_cq: Option<NvmeCqState>,
    io_sqs: Vec<Option<NvmeSqState>>,   // Up to 15 I/O SQ
    io_cqs: Vec<Option<NvmeCqState>>,   // Up to 15 I/O CQ
    num_io_queues: u16,                 // Negotiated queue count
    pci_command: u16,                   // PCI command register
    pci_bar_addrs: [u64; 6],            // PCI BAR addresses
    msix_entries: Vec<(u64, u64)>,      // Per-vector (addr, data)
    msix_enabled: bool,
}
```

Each submission queue (SQ) and completion queue (CQ) carries:

```rust
NvmeSqState { base: u64, size: u16, head: u16, tail: u16, cq_id: u16 }
NvmeCqState { base: u64, size: u16, head: u16, tail: u16, phase: bool, iv: u16 }
```

### CQ Tail Handling

After migration, the CQ tail and CQ head can be out of sync because the
guest's internal CQ head (maintained by the NVMe driver in kernel memory)
may differ from the doorbell-tracked head the VMM sees. The source's CQ
tail is preserved as-is.

Linux's NVMe driver uses a **generation counter** in the CID (command ID)
field. If a stale CQE (completion queue entry) is encountered after
migration, the gen counter won't match. Linux logs a warning ("cid does
not match outstanding request") but handles it gracefully -- the stale
entry is ignored. This means we do NOT need to reset CQ tail or drain
the CQ.

### PCI BAR Restoration

NVMe uses BAR0 (MMIO, controller registers) and BAR4 (MMIO, MSI-X table).
During restore, the PCI command register, BAR addresses, and MSI-X table
are reprogrammed before any I/O can occur. Without this, the guest's
MMIO writes to NVMe registers hit unmapped addresses and are silently
dropped.


## Virtio-blk Feature Flag Propagation

During normal operation, when the guest writes to the virtio feature
register, the PCI transport propagates `event_idx` and `indirect_supported`
flags to all VirtQueues. However, `restore_migrate_queues()` bypasses the
feature register write path -- it directly sets `guest_features` in the
`VirtioPciState`.

**The bug**: Without explicit propagation, `indirect_supported` stays
`false` on the destination. Linux's virtio-blk driver uses indirect
descriptors for **all** I/O requests. When `collect_chain()` encounters
an indirect descriptor with `indirect_supported == false`, it silently
returns `None`. Every request is dropped. Disk I/O stalls completely with
no error message.

**The fix** (in `restore_migrate_queues` in `pci/migrate.rs`): After setting
`guest_features`, explicitly call `set_event_idx()` and
`set_indirect_supported()` on all queues:

```rust
// Propagate feature flags to queues (bypass the register-write path)
let event_idx = (vs.guest_features & VIRTIO_F_EVENT_IDX) != 0;
let indirect = (vs.guest_features & VIRTIO_F_INDIRECT_DESC) != 0;
for q in &mut vs.queues {
    q.set_event_idx(event_idx);
    q.set_indirect_supported(indirect);
}
```

This was a silent failure -- no error, no log, just dead I/O. It was only
discovered by tracing `collect_chain()` returns and noticing every call
returned `None` on the destination.


## Time Synchronization

The guest's sense of time must be continuous across migration. The kernel
manages TSC (Time Stamp Counter) and `boot_hrtime` (the host hrtime at
which the VM was created). These must be adjusted on the destination.

### The Algorithm (Propolis Pattern)

```
guest_uptime = src.vt_hrtime - src.vt_boot_hrtime
migrate_delta = max(0, dst_wall_clock - src_wall_clock)
new_boot_hrtime = dst.vt_hrtime - (guest_uptime + migrate_delta)
```

For TSC adjustment:

```
tsc_delta = migrate_delta_ns * dst_guest_freq / 1_000_000_000
new_guest_tsc = scaled_src_tsc + tsc_delta
```

If the source and destination have different TSC frequencies, the source
TSC is **scaled** to the destination's frequency before adding the delta:

```
scaled_guest_tsc = src_tsc * dst_freq / src_freq
```

The kernel's `vdi_time_info_v1` structure is read on both sides. The
source exports its snapshot; the destination reads its own current
snapshot, computes the adjustment, and writes the adjusted values.

### Import Order

Time data must be imported **before** device state, because timer
devices (HPET, LAPIC timer, PM timer) depend on the correct time base.
The kernel's `vm_denormalize_hrtime()` converts relative timer values
to absolute host times using `boot_hrtime`, so it must be set first.


## Kernel Device State

State is transferred as raw bytes via the `vmm-data` kernel interface.
Each device class has a version number, and the kernel handles struct
versioning internally.

### System Devices

| Device | Class Constant | Version | Notes |
|--------|---------------|---------|-------|
| IOAPIC | `VDC_IOAPIC` | 1 | Must be imported before LAPIC |
| ATPIT  | `VDC_ATPIT`  | 1 | 8254 timer |
| ATPIC  | `VDC_ATPIC`  | 1 | 8259 PIC |
| HPET   | `VDC_HPET`   | 1 | High precision event timer |
| PM Timer | `VDC_PM_TIMER` | 1 | ACPI power management timer |
| RTC    | `VDC_RTC`    | 2 | Real-time clock |

### Per-vCPU State

vCPU state is exported/imported using individual register get/set calls
rather than the bulk vmm-data interface. This is more reliable for
field-based classes like `VDC_REGISTER` and `VDC_MSR`.

**Import order matters**:

1. MSRs first -- for `GS_BASE`, `KERNEL_GS_BASE`, `STAR`, `LSTAR`
2. LAPIC -- timer state needs correct time base
3. General registers + segment descriptors
4. Run state (`VRS_RUN`, `VRS_HALT`, `VRS_INIT`) + SIPI vector
5. VMM_ARCH -- pending interrupt state (NMI, EXTINT, exceptions)

Registers that fail to import on the destination (e.g., XCR0 with AVX
bits on a host lacking AVX-512) are logged as warnings but do not abort
migration. The guest may crash later if it uses unsupported instructions,
but this is preferable to aborting a migration that might succeed.

### Segment Descriptors

Segment registers (CS, DS, ES, FS, GS, SS, TR, LDTR, GDTR, IDTR) are
exported with full descriptor state: base, limit, and access rights.
A marker value (`0xFFFFFFFF`) in the register stream separates general
registers from segment descriptors:

```
[reg_id: u32, value: u64] ...   // general registers (12 bytes each)
[0xFFFFFFFF, count: u64]         // marker + segment descriptor count
[reg_id: u32, base: u64, limit: u32, access: u32] ...  // 20 bytes each
```


## Post-Restore Interrupt Injection

After restoring all device state but before resuming vCPUs, the destination
injects interrupts to wake the guest's device drivers. Without this, the
guest may not notice pending used ring entries and I/O stalls.

### Timer Kick

A LAPIC timer interrupt (vector `0xEC`) is injected to each vCPU 200ms
after resume. This restarts the guest's scheduler tick in
`nohz`/tickless mode, which may have been sleeping when the source was
paused.

### Virtio Interrupt Pumping

A background thread (`post-migrate-virtio-kick`) runs 10 iterations at
100ms intervals after vCPU start. Each iteration calls
`post_restore_kick()` on all lifecycle devices, which re-injects MSI-X
interrupts on all enabled queues. This ensures:

- Viona's NAPI handler runs and replenishes rx buffers
- Virtio-blk's completion handler processes stale used ring entries

For virtio-blk specifically, the first 32 completions after migration
force-fire interrupts (bypassing EVENT_IDX suppression). This handles
the case where the guest's expected `used_event` threshold is ahead of
the current `used_idx`, which would normally suppress interrupts.


## CPU Feature Compatibility

The source sends CPUID feature bits in the preamble. The destination
computes the set difference: features present on the source but missing
on the destination.

```
missing_leaf7_ebx = src.leaf7_ebx & ~dst.leaf7_ebx
```

If any bits are missing, the destination logs a warning with the specific
missing bits. Currently this is a **warning** rather than a hard failure,
because some missing features (e.g., AVX-512 subtypes) may not actually
be in use by the guest.

### CPU Baseline Profiles

The `--cpu-baseline` flag masks the source's guest-visible CPUID to a
common denominator:

| Profile | Masks Out | Use Case |
|---------|-----------|----------|
| `host`  | Nothing | Same-model migration |
| `avx2`  | AVX-512 family | Migration between ~2014+ Intel CPUs |
| `sse42` | AVX-512 + AVX2 | Maximum compatibility |


## Error Recovery

If migration fails after the source VM has been paused, the source
automatically resumes the VM. This is handled in `run_source_sync()`:

```rust
if result.is_err() {
    let phase = status.lock().map(|s| s.phase).unwrap_or(MigrationPhase::Error);
    if phase as u8 >= MigrationPhase::Pause as u8 {
        let _ = hdl.resume();
    }
}
```

The check on phase ensures we only resume if we actually paused. If the
error occurred during pre-pause RAM transfer, the VM is still running
and no resume is needed.

A rollback also re-initialises every kernel ring the pause stopped. A
paused viona ring is `VRS_STOP`, and `viona_ioc_ring_kick` answers
EBUSY for one, so a guest kick cannot revive it: only the reset plus
state write the rollback does can. Without that the source guest
resumed with a NIC whose rings never moved again.

The destination VM (if started) must be destroyed by the caller. A
`migrate-dest` that fails puts the VM back in the state it was in, so a
live guest is never left `Stopped` and unreachable.


## Synchronous Transport (No Tokio in Zones)

Bhyve zones on illumos lack `/dev/poll`, which tokio requires for its
event loop. The migration protocol therefore uses **synchronous**
WebSocket I/O via the `tungstenite` crate.

`source.rs` and `destination.rs` are the only protocol implementation.
They are generic over `wire::Transport`, a `Read + Write` stream whose
read deadline the protocol can move: `std::os::unix::net::UnixStream`
inside a zone, `std::net::TcpStream` elsewhere. The GZ agents, which do
have `/dev/poll`, still use tokio to bridge Unix sockets to TCP with
`tokio::io::copy`.

There used to be a second, async implementation for direct TCP. The two
drifted: the async twin pauses no devices, flushes nothing, exports no
device state and runs no ZFS barrier, and its CPU feature check compared
different leaves. A `migrate-source` with a `host:port` target selected
it. It is deleted, and the one driver serves both transports.

Read deadlines come from `wire::Deadlines`: generous before the pause,
short after it. `SO_SNDTIMEO` reports success on illumos and the write
still blocks for ever, so only read deadlines are set.


## Comparison with Propolis

Propolis (Oxide's bhyve VMM for the Oxide rack) served as the primary
reference for this implementation. Key differences:

### Viona Ring State

Propolis has a bug in viona migration: the `From<&VirtQueue>` impl for
`vioc_ring_state` uses `..Default::default()`, which zeroes out
`vrs_avail_idx` and `vrs_used_idx` in `RING_SET_STATE`. This means
Propolis does not actually migrate ring indices -- it passes 0/0 to the
destination kernel.

In our implementation, we explicitly export `vr_cur_aidx` and
`vr_cur_uidx` via `VNA_IOC_RING_GET_STATE` and set them on the
destination via `VNA_IOC_RING_SET_STATE`. This is what makes post-migration
networking work immediately.

### ZFS Disk Replication

Propolis targets the Oxide rack where storage is provided by Crucible
(a distributed block store with built-in replication). It does not handle
local disk migration. Our implementation uses ZFS send/recv with a
post-pause barrier mechanism for exact-pause-point consistency.

### Transport

Propolis uses async tokio throughout. We use synchronous I/O because
bhyve zones lack `/dev/poll`. The protocol semantics are similar but the
transport layer is different.


## Measured Timing

Measured on NUC hardware (Intel N100/i5, 2.6 GHz, 1 Gbps network):

| Phase | Duration |
|-------|----------|
| ZFS full send (10 GB zvol) | ~40s |
| ZFS iterative incrementals (3x) | ~1s each |
| Start destination VM | ~2s |
| Post-pause ZFS final incremental | ~1s |
| RAM transfer (1 GB) | ~5s |
| **Total guest downtime (pause to resume)** | **~450ms** |
| Network recovery after resume | Immediate (first ping succeeds) |

The guest downtime is dominated by the post-pause ZFS barrier (~1s) plus
final dirty RAM transfer. For VMs with larger memory or higher write rates,
the pre-pause convergence loop may need more iterations.


## Key Source Files

| File | Purpose |
|------|---------|
| `crates/vmm-migrate/src/source.rs` | Source migration protocol (pre-pause RAM, barrier, device export) |
| `crates/vmm-migrate/src/destination.rs` | Destination migration protocol (topology check, RAM import, state restore, resume) |
| `crates/vmm-migrate/src/wire.rs` | Transport trait, read deadlines, framing, message expectations |
| `crates/vmm-migrate/src/cpu.rs` | The one CPU feature comparison both ends use |
| `crates/vmm-migrate/src/codec.rs` | Wire format: Message types, preamble, device payloads |
| `crates/vmm-devices/src/lifecycle/state.rs` | The device state types the wire carries |
| `crates/vmm-migrate/src/protocol.rs` | Protocol version, dirty bitmap iterator |
| `crates/vmm-migrate/src/state.rs` | Kernel device state export/import (vmm-data API, register get/set) |
| `crates/vmm-devices/src/virtio/pci.rs` | Virtio PCI transport: export/restore BAR state, feature flags, queue indices |
| `crates/vmm-devices/src/virtio/viona.rs` | Viona kernel interface: RING_SET_STATE, RING_PAUSE, interrupt polling |
| `crates/vmm-devices/src/nvme/mod.rs` | NVMe controller: export/restore registers, queues, MSI-X table |
| `crates/vmm-devices/src/lifecycle.rs` | Lifecycle trait: pause_for_migration, export/restore hooks |
| `bin/vmm/src/control.rs` | Control socket: migrate-source command, device queue export, pre-pause callback |
| `bin/vmm/src/main.rs` | Destination restore callback, post-migrate interrupt kick thread |


## Lessons Learned

### 1. Pause ordering is non-negotiable

Device rings must be paused before vCPUs. The gap between vCPU pause and
ring pause allows the kernel to consume avail ring entries while the
guest's descriptor state is frozen mid-NAPI. This produces
impossible-to-reproduce crashes on the destination because the exact
state depends on timing.

### 2. Never manipulate guest memory rings

Synthesizing used ring entries, advancing `used_ring->idx`, or modifying
avail ring entries all fail due to descriptor ID reuse across ring cycles.
The virtio spec permits reuse, and Linux's virtio driver exploits this.
The only safe approach is to set the kernel's tracking indices correctly
and let the existing ring contents flow naturally.

### 3. Export kernel state, not guest memory state

For viona, export `vr_cur_aidx` (the kernel's consumed position) rather
than `avail_ring->idx` (the guest's posted position). The former
represents what the kernel has actually processed; the latter includes
entries the kernel hasn't touched. The delta is the available buffer
count on the destination.

### 4. ZFS must sync to the exact pause point

Disk writes between the last ZFS incremental and the vCPU pause cause
filesystem journal inconsistencies. The guest writes data blocks, then
writes journal entries pointing to those blocks. If the journal entries
are captured but the data blocks are not, the destination filesystem is
corrupted. The post-pause barrier mechanism ensures the final ZFS send
captures everything up to the pause.

### 5. NVMe generation counters handle stale completions gracefully

Linux's NVMe driver includes a generation counter in the CID field.
Stale CQEs after migration produce a warning but not a crash. This
means we can preserve the source's CQ tail as-is without draining or
resetting. This was a relief after the viona ring state debugging.

### 6. Feature flags do not propagate automatically during restore

The virtio PCI transport updates `VirtQueue` feature flags during the
normal guest driver negotiation path (register writes). The restore
path bypasses this, so `indirect_supported` stays false. This caused
a completely silent I/O stall -- no errors, no logs, just dead disk.
Always verify the full device initialization path when implementing
restore shortcuts.

### 7. Post-restore interrupts need persistence

A single interrupt injection after restore is often insufficient.
The guest's NAPI handler or blk completion handler may not run
immediately (scheduler latency, interrupt coalescing). Pumping
interrupts 10 times at 100ms intervals ensures the guest eventually
processes all pending work, even under high load.

### 8. Synchronous I/O works fine for migration

The lack of `/dev/poll` in bhyve zones initially seemed like a major
obstacle. In practice, the migration protocol is inherently sequential
(send pages, wait for ack, send more pages) and does not benefit from
async I/O. The synchronous path using `tungstenite` is simpler, has
fewer failure modes, and produces clearer stack traces during debugging.
