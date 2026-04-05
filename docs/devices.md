# Supported Devices

## PCI Devices

| Driver | Type | Description |
|--------|------|-------------|
| `hostbridge` | Chipset | i440fx host bridge (slot 0) |
| `lpc` | Chipset | ISA/LPC bridge (slot 31) |
| `virtio-blk` | Storage | VirtIO legacy block device |
| `nvme` | Storage | NVMe 1.0 controller with async I/O |
| `ahci-cd` | Storage | CD-ROM (falls back to virtio-blk) |
| `ahci-hd` | Storage | AHCI disk (falls back to virtio-blk) |
| `virtio-net-viona` | Network | VirtIO network via viona kernel driver |
| `virtio-rnd` | Misc | VirtIO entropy source (/dev/random) |
| `fbuf` | Display | Framebuffer with RFB/VNC server |
| `xhci` | USB | xHCI USB 3.0 controller (tablet input) |
| `passthru` | Passthrough | PCI device passthrough via PPT |

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

- VirtIO legacy PCI transport
- Supports `nodelete` (disable TRIM) and `sectorsize=N` options
- Single queue, 128 entries

### Disk Options

```
-s 4,virtio-blk,/path/to/disk            # basic
-s 4,virtio-blk,/path/to/disk,nodelete   # disable TRIM
-s 4,virtio-blk,/path/to/disk,sectorsize=4096  # 4K sectors
-s 4,virtio-blk,/path/to/disk,ro         # read-only
-s 4,nvme,/path/to/disk                  # NVMe (recommended)
```

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

## PCI Passthrough

Requires PPT (PCI PassThrough) kernel driver. Works on both Intel
(VT-d) and AMD (IOMMU) platforms.

```
-s 5,passthru,/dev/ppt0
```

Automatically adds `-S` (wire memory) and `-A` (ACPI) when passthrough
devices are present.
