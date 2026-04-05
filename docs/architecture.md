# Architecture

## Crate Structure

```
vmmnew/
  bin/vmm/              Main binary
  crates/
    bhyve-api/          Kernel ioctl FFI (forked from propolis)
    bhyve-api/sys/      Raw ioctl structs and constants
    viona-api/          viona network driver FFI
    viona-api/sys/      Raw viona structs
    vmm-core/           VmmHdl, PhysMap, MemCtx, Machine, Vcpu, CPUID
    vmm-devices/        All device implementations
    vmm-migrate/        Migration protocol, codec, state export/import
    vmm-config/         CLI parsing (clap)
```

## Key Types

### vmm-core

- **`VmmHdl`** - Safe wrapper over `/dev/vmm/{name}` fd. All kernel
  ioctls exposed as typed methods. No raw ioctl escape hatch.
- **`PhysMap`** - Guest physical address space manager. Tracks RAM,
  ROM, and MMIO regions as an interval map.
- **`MemCtx`** - Safe guest memory read/write with lifetime-bounded
  mappings. Used by devices to access guest RAM.
- **`Machine`** - Aggregate: VmmHdl + vCPUs + PhysMap + PIO/MMIO buses.
- **`Vcpu`** - Per-vCPU state. Provides register get/set, run state
  management, and the vm_run loop.
- **`Builder`** - Builder pattern for VM construction. Allocates memory
  segments, creates the VMM instance, sets up the physical map.

### vmm-devices

- **`Lifecycle` trait** - Common device lifecycle (start/pause/resume/reset).
- **`PciDevice` trait** - PCI config space + BAR handling.
- **`VirtioDevice` trait** - VirtIO backend interface (features, queues,
  config space, notifications).
- **`VirtioPciDevice<D>`** - Generic PCI transport wrapper for any
  VirtioDevice implementation.

### vmm-migrate

- **`Message`** - WebSocket protocol messages (11 types).
- **`source::run_source`** - Drives the source side of migration.
- **`destination::run_destination`** - Drives the destination side.
- **`state`** - Kernel state export/import via vmm-data ioctls.

## Data Flow

### vCPU Execution

```
vm_run() -> VmExit -> dispatch:
  PIO exit  -> PioBus -> device handler
  MMIO exit -> MmioBus -> device handler
  HLT       -> re-enter (kernel handles)
  Suspend   -> report to main thread
```

### Disk I/O (NVMe)

```
Guest write to doorbell -> MMIO exit -> NvmeController
  -> worker thread pool (8 threads)
  -> preadv/pwritev directly to guest memory (zero-copy)
  -> post completion entry to CQ
  -> fire MSI-X interrupt
```

### Network I/O (viona)

```
Guest write to queue notify -> PIO exit -> VirtioViona
  -> VNA_IOC_RING_KICK (kernel processes packet)
  -> kernel signals POLLRDBAND when done
  -> interrupt poll thread calls VNA_IOC_INTR_POLL
  -> raise PCI legacy interrupt to guest
```

### Serial / Metadata (COM2)

```
Guest writes to ttyS1 -> UART THR register -> TX FIFO
  -> mdata agent thread reads via output_byte()
  -> parses V2 protocol frame
  -> looks up key in metadata store
  -> writes response via input_byte() -> RX FIFO
  -> guest reads from RBR register
```
