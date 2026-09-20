# Architecture

Two binaries over one set of library crates. `rshyve` is the full
bhyve-parity VMM; `firehyve` is a microVM with a short device catalog.
The crate split is what keeps them apart: `firehyve` has no dependency
path to the parity-only crates, and CI enforces that with `cargo-deny`
scoped to its manifest (`deny-firehyve.toml`).

## Workspace

```
bin/rshyve/                 Parity VMM: mdata agent, control socket, bootrom
bin/firehyve/               microVM: direct kernel boot, no control socket
bin/vmm-migrate-agent/      Global-zone helper that drives a migration
tools/fhrun/                Runs a Linux binary in a microVM like a child process
tools/fhrun-init/           In-guest PID 1 for fhrun (linux-musl, own workspace)
tools/virtiofs-test/        In-guest test harness (linux-musl, own workspace)

crates/bhyve-api/           Kernel VMM ioctl wrappers            from Propolis
crates/bhyve-api/sys/       Raw ioctl structs and constants       from Propolis
crates/viona-api/           viona NIC driver ioctls               from Propolis
crates/dladm/               libdladm VNIC lookup                 FFI from Propolis
crates/vmm-api-common/      Types shared by the control planes
crates/vmm-core/            VmmHdl, PhysMap, MemCtx, Machine, Vcpu, CPUID, MSRs
crates/vmm-machine/         Machine construction phases; names no PCI driver
crates/vmm-devices/         PCI, chipset, ACPI, AML, fw_cfg, SMBIOS, UART, pvpanic
crates/vmm-devices-testsupport/  Shared test doubles
crates/vmm-virtio/          virtio transport and queues: blk, viona, rng, console,
                            fs, vsock
crates/vmm-boot/            Direct bzImage and PVH kernel loading
crates/vmm-config/          CLI parsing, bhyve flag-compatible
crates/vmm-metrics/         Counters the binaries publish

crates/vmm-storage/         AHCI and NVMe                        rshyve only
crates/vmm-hid/             xHCI, PS/2, framebuffer and VNC      rshyve only
crates/vmm-passthru/        PCI passthrough                      rshyve only
crates/vmm-migrate/         Migration protocol and state         rshyve only
crates/vmm-hyperv/          Hyper-V enlightenments               rshyve only
crates/vmm-tpm/             vTPM 2.0 over libtpms                rshyve only
crates/vmm-tpm-sys/         libtpms build and FFI                rshyve only
```

`tools/fhrun-init` and `tools/virtiofs-test` are excluded from the root
workspace: they cross-compile to `x86_64-unknown-linux-musl` and carry
their own lockfiles. CI covers them in jobs of their own.

## Key types

### vmm-core

- **`VmmHdl`** — safe wrapper over `/dev/vmm/{name}`. Every kernel ioctl
  is a typed method; there is no raw ioctl escape hatch.
- **`PhysMap`** — guest physical address space manager. Tracks RAM, ROM
  and MMIO regions as an interval map. `PhysMap::new_anon` gives tests a
  real address space with no kernel.
- **`MemCtx`** / **`SubMapping`** — guest memory access, lifetime-bound
  and volatile. Every device read and write of guest memory goes through
  it.
- **`Machine`** — the aggregate: `VmmHdl`, vCPUs, `PhysMap`, and the PIO
  and MMIO buses.
- **`Vcpu`** — per-vCPU register access, run state, and the `vm_run`
  loop.
- **`PioBus`** / **`MmioBus`** — exit dispatch to device handlers.

### vmm-machine

The phase bodies each binary's `run()` calls in its own order, because
that ordering is load-bearing: `build_machine`, `init_chipset`,
`attach_uart`, `setup_fwcfg_and_acpi`, vCPU spawn, the hotplug engine,
the device registry, signal handling and teardown. The crate names no
concrete PCI driver type; the binaries do.

### vmm-devices

- **`Lifecycle`** — start, pause, resume, reset and migration hooks,
  driven by the registry.
- **`PciDevice`** — config space and BAR handling.
- **`QuiesceGate`** — bounded wait for worker threads to stop touching
  guest memory, used by reset, teardown and migration.

### vmm-virtio

- **`VirtioDevice`** — the device-side trait: `u64` feature bits, queue
  notification, config space.
- **`VirtioPciDevice<D>`** — the transitional PCI transport over any
  `VirtioDevice`. BAR0 carries the legacy PIO registers and BAR2 the
  modern MMIO ones.
- **`VirtQueue`** — descriptor chain collection, direct and indirect,
  and used-ring publication.

### vmm-migrate

- **`Message`** / **`MessageType`** (`codec.rs`) — the wire frames.
- **`source`** / **`destination`** — the WebSocket protocol.
- **`sync_source`** / **`sync_destination`** — the stream protocol that
  production uses, which also flushes storage and exports device state.
- **`state`** — kernel state export and import over the vmm-data ioctls.

The two protocols are not equivalent; `docs/security/open-findings.md`
records what the async path leaves out.

## Data flow

### vCPU execution

```
vm_run() -> VmExit -> dispatch:
  PIO exit  -> PioBus  -> device handler
  MMIO exit -> MmioBus -> device handler
  HLT       -> re-enter (the kernel handles it)
  Suspend   -> report to the main thread
```

### Disk I/O (NVMe)

```
guest writes a doorbell -> MMIO exit -> NvmeController
  -> one of eight worker threads
  -> preadv/pwritev straight into guest memory
  -> completion entry posted to the CQ
  -> MSI-X interrupt
```

### Network I/O (viona)

```
guest writes the queue notify register -> PIO or MMIO exit
  -> VNA_IOC_RING_KICK (the kernel moves the packet)
  -> the kernel signals POLLRDBAND
  -> the interrupt poll thread calls VNA_IOC_INTR_POLL
  -> MSI-X, or a legacy PCI interrupt
```

### Serial and metadata (COM2)

```
guest writes ttyS1 -> UART THR -> TX FIFO
  -> the mdata agent thread reads it
  -> parses one V2 protocol frame
  -> looks the key up in the metadata store
  -> writes the reply into the RX FIFO
  -> guest reads RBR
```

## Trust boundary

The guest controls port I/O, MMIO, every virtqueue descriptor, every
device command queue, the serial bytes the mdata agent parses, TPM
commands and config-space writes. The release profile is
`panic = "abort"`, so any panic reachable from guest input is a denial
of service. `SECURITY.md` states the boundaries; `docs/features.md`
records per-device limits.
