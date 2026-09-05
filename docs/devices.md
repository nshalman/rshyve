# Supported Devices

## PCI Devices

`rshyve` accepts every driver below. `firehyve` accepts only the eight
marked "both" and fails at startup on the rest, naming the driver.

| Driver | Binaries | Type | Description |
|--------|----------|------|-------------|
| `hostbridge` | both | Chipset | i440fx host bridge, already attached at 0.0.0 |
| `lpc` | both | Chipset | ISA/LPC bridge, already attached at slot 1 |
| `virtio-blk` | both | Storage | VirtIO block device, multiqueue |
| `virtio-net-viona` | both | Network | VirtIO network via the viona kernel driver |
| `virtio-rnd` | both | Misc | VirtIO entropy source |
| `virtio-console` | both | Serial | VirtIO console on a Unix socket |
| `virtio-fs` | both | Filesystem | FUSE passthrough of a host directory |
| `virtio-blk-pci` | both | Storage | Alias for `virtio-blk` |
| `nvme` | rshyve | Storage | NVMe 1.0 controller with async I/O |
| `ahci-cd` | rshyve | Storage | ATAPI CD-ROM, read only |
| `ahci-hd` | neither | Storage | Not implemented. A hard startup error, not a fallback |
| `fbuf` | rshyve | Display | Framebuffer with an RFB/VNC server on a Unix socket |
| `xhci` | rshyve | USB | xHCI USB 3.0 controller with an absolute tablet |
| `passthru` | rshyve | Passthrough | PCI device passthrough via PPT |

An unrecognized driver name behaves differently per binary: `firehyve`
refuses to start, while `rshyve` logs a warning, skips the device, and
boots without it. A typo in `-s` therefore yields a VM with a missing
disk rather than an error.

See [features.md](features.md) for option keys, defaults and limits.

## LPC Devices

| Device | Description |
|--------|-------------|
| `bootrom` | UEFI firmware ROM |
| `com1` | 16550 UART at 0x3F8, IRQ 4 |
| `com2` | 16550 UART at 0x2F8, IRQ 3 (mdata agent) |

## Kernel-emulated Devices

These are emulated in the bhyve kernel module. The VMM holds handles
for lifecycle management and state export/import during migration.

| Device | Description |
|--------|-------------|
| LAPIC | Local APIC (per-vCPU) |
| IOAPIC | I/O APIC |
| ATPIC | i8259 PIC (master + slave) |
| ATPIT | i8254 PIT (timer) |
| HPET | High Precision Event Timer |
| RTC | MC146818 Real-Time Clock |
| PM Timer | ACPI Power Management Timer |

## Storage Details

### NVMe

- NVMe 1.0 specification compliant
- 8 async worker threads with preadv/pwritev
- Zero-copy I/O directly to/from guest memory
- MSI-X interrupts
- MDTS=9 (2 MiB max transfer)
- Sequential read: ~2.8 GB/s (matches C bhyve)

### virtio-blk

- Transitional VirtIO PCI transport, legacy and modern
- Options: `ro`, `sectorsize=N`, `num-queues=N` (1..=8), `nodelete`
- Multiqueue. The default queue count is `min(vcpus, 4)`, at least 1
- 128 descriptors per queue
- `nodelete` parses and is then never read. DISCARD and WRITE_ZEROES
  advertise their feature bits and return success without touching the
  file, so an in-guest `fstrim` reports success and frees nothing
- Every request bounces through a per-worker host buffer instead of
  reading or writing guest memory directly, which is what lets a device
  reset be synchronous. Measured cost:
  [block-bounce-cost.md](block-bounce-cost.md)

### Disk Options

```
-s 4,virtio-blk,/path/to/disk            # basic
-s 4,virtio-blk,/path/to/disk,nodelete   # disable TRIM
-s 4,virtio-blk,/path/to/disk,sectorsize=4096  # 4K sectors
-s 4,virtio-blk,/path/to/disk,ro         # read-only
-s 4,nvme,/path/to/disk                  # NVMe (recommended)
```

## Serial Details

### virtio-console

A single-port VirtIO console. The host end is a Unix domain socket
created with 0600 permissions inside a 0700 directory; the guest sees
one port at `/dev/hvc0`.

```
-s 8,virtio-console,/var/run/rshyve/<vm>/console.sock
```

The config takes a socket path and nothing else. Any comma-separated
option after the path is rejected.

Attach from the host with:

```
socat - UNIX-CONNECT:/var/run/rshyve/<vm>/console.sock
```

Only one client may be attached at a time. Guest output written while no
client is attached is discarded. Host input that arrives while the guest
has posted no receive buffer is held, up to 64 KiB, and delivered when
the guest posts one.

`VIRTIO_CONSOLE_F_MULTIPORT` and `VIRTIO_CONSOLE_F_SIZE` are not
offered, so there are no control queues and no terminal-size reporting.
Console state is not carried across a live migration.

## Network Details

### virtio-net-viona

The network stack is handled entirely in-kernel by the viona driver.
Userspace is responsible for:

1. Opening `/dev/viona` and binding to a VNIC
2. Programming virtqueue ring addresses
3. Forwarding queue notifications (kicks)
4. Polling for completion interrupts (POLLRDBAND)

Options:
```
-s 6,virtio-net-viona,net0               # basic
-s 6,virtio-net-viona,net0,promiscphys   # promiscuous mode
```

## Display

### Framebuffer (fbuf)

VNC server listening on a Unix socket. Used with `xhci,tablet` for
pointer input.

```
-s 30:0,fbuf,vga=off,unix=/tmp/vm.vnc
-s 30:0,fbuf,vga=off,unix=/tmp/vm.vnc,password=secret
-s 30:0,fbuf,vga=off,unix=/tmp/vm.vnc,w=1920,h=1080
-s 30:1,xhci,tablet
```

`vga=` is accepted for bhyve compatibility and then discarded. There is
no VGA emulation to enable, so every value behaves the same.

## PCI Passthrough

Needs the PPT (PCI PassThrough) kernel driver, and **Intel VT-d only**.

```
-s 5,passthru,/dev/ppt0
```

`-S` (wire memory) must be passed explicitly when using passthrough.
Nothing adds it automatically, and `-A` is accepted but never read.

### Limits

**AMD hosts have no IOMMU backend.** `iommu_init()` asks for
`misc/vmm_vtd` on an Intel host and `misc/vmm_amdvi` on an AMD one, but
no `vmm_amdvi` module is built: the illumos tree has a `vmm_vtd` module
directory and `VMM_VTD_OBJS` in `Makefile.files`, and neither has an AMD
counterpart. The source carries the comment "Use the expected name for
if/when this is ported". On an AMD host `ddi_modopen` fails and
passthrough is unavailable.

**A device that advertises MSI-X is refused at attach.** `PciPassthru`
maps a whole BAR into the guest with EPT, and on an MSI-X device that
BAR holds the physical MSI-X table. illumos VT-d remaps DMA but not
interrupts, so a guest that writes that table makes the device raise a
vector of its choice on the host APIC. Linux gates the same hole behind
`allow_unsafe_interrupts`. This VMM refuses the device instead, with
`device advertises MSI-X, which this VMM cannot isolate`. Most modern
PCIe devices advertise MSI-X, so in practice passthrough is limited to
devices with MSI or INTx.

**Config-space writes are deny by default.** Only the command register,
the six BARs, the interrupt line byte and the MSI capability have a
handler. Every other write is logged and dropped, because the illumos
PPT driver forwards `PPT_CFG_WRITE` to `pci_config_put32` with no filter
of its own. A guest cannot drive PCIe DEVCTL Initiate FLR, move the
device to D3 through PMCSR, or point the Expansion ROM BAR at host
memory.

**An I/O BAR decodes nothing.** The device registers no PIO handler, so
guest port accesses to an I/O BAR reach neither hardware nor emulation.

## Filesystem

### virtio-fs

Exports a host directory to the guest as a FUSE filesystem over a
virtqueue. No DAX and no shared-memory window: every reply travels
through the request queue.

```
-s 12,virtio-fs,/export/shared                       # tag "shared", rw
-s 12,virtio-fs,/export/data,tag=data,ro             # read-only
-s 12,virtio-fs,/export/data,queue-size=256          # deeper queue
```

Options are `tag=<name>` (defaults to the last path component, at most 36
bytes, no NUL), `ro`, and `queue-size=<n>` (8..=1024, rounded up to a
power of two). An unrecognized option is an error.

Mount it in a Linux guest with:

```
mount -t virtiofs <tag> /mnt
```

Limitations:

- The device advertises EVENT_IDX and INDIRECT_DESC, which the PCI
  transport strips on the legacy path. A legacy-only guest therefore
  negotiates a smaller feature set than a VERSION_1 guest.
- One worker serves both queues, so the hiprio queue gives no priority:
  a `FUSE_INTERRUPT` or `FUSE_FORGET` waits behind the request already
  in flight.
- `FUSE_ACCESS` is granted whenever the node can be stat'd, so the
  `mask` argument is not enforced on a read-write export.
- Hardlink creation inside the export uses the `/proc/self/fd` idiom and
  is not guaranteed on illumos.
- No migration state is exported, so in-flight requests do not survive a
  migration.
