# Platform Image Integration

## Drop-in Replacement Strategy

The `rshyve` binary is a drop-in replacement for `/usr/sbin/bhyve`. It
accepts the same CLI flags that `boot.c` constructs.

### Integration Path

1. Install `rshyve` at `/usr/sbin/rshyve` in the platform image
2. Modify `boot.c` to exec `rshyve` instead of `bhyve`
3. No changes needed to vmadm, VMAPI, or zone brand

### boot.c Interface

`boot.c` reads zone configuration from environment variables and
constructs a bhyve command line. The following flags are used:

```
rshyve -H
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

All of these flags are supported by `rshyve`.

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

`rshyve` handles SIGTERM by triggering an ACPI poweroff, allowing the
guest to shut down gracefully. This integrates with zoneadmd's zone
lifecycle management.

## Metadata Delivery

In a Triton zone, metadata is normally served by a node.js agent in
the global zone, connected to the zone's COM2 via a Unix socket path.
Two options for `rshyve`:

### Production: External Agent

`boot.c` passes `-l com2,/dev/zconsole` (or the zone metadata socket
path), connecting COM2 to the existing global zone metadata agent. No
changes needed — the external agent handles all metadata requests.

### Testing: Built-in Agent

When no `-l com2,<path>` is specified, `rshyve` runs a built-in mdata
agent for standalone testing. Network config and SSH keys are passed
via `--mdata-*` flags. This is not used in production.

## Build Integration

### Cargo Release Build

```bash
cargo build --release
# Binary: target/release/rshyve
```

### Platform Image Inclusion

Add to the SmartOS platform build:

1. Build `rshyve` as part of `smartos-live` (or ship pre-built)
2. Install to `/usr/sbin/rshyve`
3. Link libdladm (required for VNIC lookup)

### Dependencies

- `libdladm.so` - VNIC name resolution (dladm_name2info)
- `libcrypto-smartos.so.3` - the platform OpenSSL, used by the vTPM
- Kernel: bhyve VMM module (API v5+, v18 recommended)
- Kernel: viona module (interface v6)

Every one of these ships in a stock platform image. `rshyve` must not
depend on anything else, because a platform image has no pkgsrc: a
binary that records `NEEDED libcrypto.so.3` links on the build host
and then fails to exec on a node.

### OpenSSL and the vTPM

The vTPM backend is the vendored libtpms, which needs OpenSSL. A
platform image ships `libcrypto-smartos.so.3`, whose exports are
almost all renamed with a `sunw_` prefix so a zone's pkgsrc OpenSSL
and the platform's cannot collide in one process. So `-lcrypto` alone
cannot use it.

`crates/vmm-tpm-sys/build.rs` handles that. It reads the export list
out of the platform library, writes a `#pragma redefine_extname`
header from it, and force-includes that header when it builds
libtpms. A link farm makes `-lcrypto` resolve to the platform
library, so `ld` records its SONAME. Off SmartOS, pkg-config decides
as before. The build script compiles and runs a known-answer test
against the platform library before it builds anything, so a
mismatch fails the build with a message instead of producing a
binary that misbehaves.

The OpenSSL headers come from pkgsrc and the library from the
platform image, so the two track different OpenSSL releases. Within
the OpenSSL 3 series that is supported. Build on a platform image no
newer than the oldest node you deploy to: the linker records the
version of every symbol it binds, and `ld.so.1` refuses to start a
binary whose recorded version the library on the node does not
define. The reverse is safe.

### Checking a binary before it ships

```bash
tools/check-needed.sh target/release/rshyve
```

The script fails if any `NEEDED` entry names a library outside the
stock platform image, or if the recorded platform OpenSSL version is
newer than the oldest image we support. `tools/remote-build.sh build`
runs it on every build.

## Feature Comparison

| Feature | C bhyve | Rust rshyve |
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
