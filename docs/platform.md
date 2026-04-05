# Platform Image Integration

## Drop-in Replacement Strategy

The `vmm` binary is a drop-in replacement for `/usr/sbin/bhyve`. It
accepts the same CLI flags that `boot.c` constructs.

### Integration Path

1. Install `vmm` at `/usr/sbin/vmm` in the platform image
2. Modify `boot.c` to exec `vmm` instead of `bhyve`
3. No changes needed to vmadm, VMAPI, or zone brand

### boot.c Interface

`boot.c` reads zone configuration from environment variables and
constructs a bhyve command line. The following flags are used:

```
vmm -H
  -U <uuid>
  -B "1,manufacturer=Joyent,product=SmartDC HVM,version=7.<ver>,serial=<zone>,sku=001,family=Virtual Machine"
  -c <vcpus> -m <ram>
  -s 0,hostbridge,model=i440fx
  -s <slot>,virtio-blk,<path>[,nodelete][,sectorsize=N]
  -s <slot>,nvme,<path>
  -s <slot>,ahci-cd,<path>
  -s <slot>,virtio-net-viona,<vnic>[,promiscphys]
  -s <slot>,passthru,<path>
  -s 30:0,fbuf,vga=off,unix=/tmp/vm.vnc[,password=<b64>]
  -s 30:1,xhci,tablet
  -s 31,lpc
  -l bootrom,/usr/share/bhyve/uefi-rom.bin
  -l com1,/dev/zconsole
  SYSbhyve-<did>
```

All of these flags are supported by `vmm`.

### PCI Slot Assignments (boot.c convention)

| Slot | Device |
|------|--------|
| 0 | Hostbridge (i440fx) |
| 3-5 | CD-ROM devices |
| 4 | Boot disk |
| 6+ | Additional disks |
| 29 | Network (function 0 = primary, 1+ = secondary) |
| 30:0 | Framebuffer |
| 30:1 | xHCI tablet |
| 31 | LPC bridge |

### Signal Handling

`vmm` handles SIGTERM by triggering an ACPI poweroff, allowing the
guest to shut down gracefully. This integrates with zoneadmd's zone
lifecycle management.

## Metadata Delivery

In a Triton zone, metadata is normally served by a node.js agent in
the global zone, connected to the zone's COM2 via a Unix socket path.
Two options for `vmm`:

### Production: External Agent

`boot.c` passes `-l com2,/dev/zconsole` (or the zone metadata socket
path), connecting COM2 to the existing global zone metadata agent. No
changes needed — the external agent handles all metadata requests.

### Testing: Built-in Agent

When no `-l com2,<path>` is specified, `vmm` runs a built-in mdata
agent for standalone testing. Network config and SSH keys are passed
via `--mdata-*` flags. This is not used in production.

## Build Integration

### Cargo Release Build

```bash
cargo build --release
# Binary: target/release/vmm
```

### Platform Image Inclusion

Add to the SmartOS platform build:

1. Build `vmm` as part of `smartos-live` (or ship pre-built)
2. Install to `/usr/sbin/vmm`
3. Link libdladm (required for VNIC lookup)

### Dependencies

- `libdladm.so` - VNIC name resolution (dladm_name2info)
- Kernel: bhyve VMM module (API v5+, v18 recommended)
- Kernel: viona module (interface v6)

## Feature Comparison

| Feature | C bhyve | Rust vmm |
|---------|---------|----------|
| virtio-blk | Yes | Yes |
| NVMe | Yes | Yes (async, zero-copy) |
| virtio-net (viona) | Yes | Yes |
| AHCI | Yes | Fallback to virtio-blk |
| Framebuffer/VNC | Yes | Yes (Unix socket) |
| xHCI tablet | Yes | Yes |
| PCI passthrough | Yes | Yes (Intel + AMD) |
| Live migration | No | Yes |
| CPUID masking | No | Yes (--cpu-baseline) |
| Built-in mdata | No | Yes |
| Control socket | No | Yes (JSON/Unix) |
| SIGTERM handling | No | Yes (ACPI poweroff) |
