# Platform Image Integration

## Replacing the platform bhyve

The `rshyve` binary takes the command line `boot.c` constructs, so it
can stand in for `/usr/sbin/bhyve`. It is not a complete bhyve:
`ahci-hd` is refused at startup, `-D`, `-k` and `--json-config` are
refused rather than ignored, an unknown `-s` driver name is logged and
skipped, and several device options parse and then do nothing
([features.md](features.md#accepted-by-the-cli-and-then-ignored)).

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

`rshyve` parses every one of these and boots from them. Some of the
trailing options are then ignored: `nodelete` and `sectorsize` on a
disk, `vga=` on the framebuffer, and `tablet` on xHCI. `password=` is
used as given and is not base64-decoded.

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
- Kernel: bhyve VMM module, API v18. Use v18: it is what the `vnext`
  platform ships and the only version this tree has run on. Nothing
  enforces a floor, so an older kernel starts and then fails at an
  ioctl or gives less without saying so. What the code needs, if a node
  runs something older: v5 for the CPUID control every VM uses, v8 for
  the `VCF_TRACK_DIRTY` create flag rshyve asks for unless
  `--no-track-dirty` is given, and v13 for live migration, which writes
  state through vmm-data (v13), imports guest time data (v11) and
  exports the RTC at data version 2 (v12). From v16 a vCPU exit also
  reports which vCPU took a triple fault. No version below 18 has been
  tried.
- Kernel: viona module, interface v6. Nothing checks this one either, so
  a mismatched module surfaces as a raw ioctl errno.

Every one of these ships in a stock platform image. `rshyve` must not
depend on anything else, because a platform image has no pkgsrc: a
binary that records `NEEDED libcrypto.so.3` links on the build host
and then fails to exec on a node.

### Host tunables

`strmsgsz` must be 8192 or above, or 0. Read the live value with
`echo 'strmsgsz/E' | mdb -k`, and look for a `set strmsgsz=` line in
`/etc/system` on a node that was tuned.

This is measured, not assumed. A lab node reads 65536 against the
VMM's 8 KiB write chunk, an 8x margin, and nothing in a stock platform
image changes it ([hardware-run.md](hardware-run.md#the-socket-bound-is-measured-not-assumed)).

The VMM writes to its Unix sockets under a lock or on a vCPU thread, so
each write needs an end. illumos has no send timeout that works on an
AF_UNIX stream, so the bound is the size of the send: 8 KiB after
`POLLOUT`, which the stream head passes as one message. The stream head
caps a message at `strmsgsz`. Below 8192 the send splits, and the wait
between two messages has no timeout. The sockets this covers are the
virtio-console client, the vsock host socket, and the firehyve
`CONTROL` reply. [docs/features.md](features.md#known-caveats) has the
full chain.

Check this on a new host rather than trusting a test pass from a
development machine. macOS bounds an AF_UNIX send with `SO_SNDTIMEO`
and illumos accepts the option and ignores it, so the portable test
suite never reaches the code that needs `strmsgsz`.

### Interrupt injection and the VM lock

A device reset ends by waiting out the interrupt injections already in
flight, and both injection ioctls take the bhyve VM read lock. illumos
grants that lock to a waiting writer first, so any operation holding
the VM write lock delays every reset on that VM. Memory hot-add, pause,
resume and VM-wide device-state export all hold it.

The delay is unbounded and it scales with the size of a hot-add: 233 to
270 ms per GiB measured. Plan a node's memory hot-add policy around
that, and read
[hardware-run.md](hardware-run.md#why-a-device-reset-has-no-upper-bound)
before assuming a reset completes in a fixed time.

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
| AHCI CD-ROM (`ahci-cd`) | Yes | Yes (ATAPI, zero-copy reads) |
| AHCI disk (`ahci-hd`) | Yes | No, refused at startup |
| Framebuffer/VNC | Yes | Unix socket only: no TCP, no `wait`, no VGA, Raw and ZRLE encodings, one client |
| xHCI tablet | Yes | One slot, absolute pointer with a scroll wheel |
| PCI passthrough | Yes | Intel VT-d only. There is no AMD IOMMU backend, MSI-X devices are refused, and no assigned device has ever been run (OSF-11) |
| Live migration | No | Yes |
| CPUID masking | No | Yes (--cpu-baseline) |
| Built-in mdata | No | Yes |
| Control socket | No | Yes (JSON/Unix) |
| SIGTERM handling | No | Yes (ACPI poweroff) |
