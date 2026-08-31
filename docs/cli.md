# CLI Reference

## Synopsis

```
rshyve [OPTIONS] <VM_NAME>
```

## bhyve-compatible Flags

These flags match the existing C bhyve interface used by `boot.c`:

| Flag | Description |
|------|-------------|
| `-c <N>` | CPU count (or `cpus=N,sockets=M,cores=C,threads=T`) |
| `-m <SIZE>` | Memory size. Bare number = MiB. Suffixes: K, M, G, T |
| `-s <slot,driver[,config]>` | PCI device (repeatable). See [Devices](devices.md) |
| `-l <device,config>` | LPC device (repeatable): `bootrom`, `com1`, `com2` |
| `-U <UUID>` | VM UUID (SMBIOS Type 1 + mdata `sdc:uuid`) |
| `-B <smbios>` | SMBIOS string: `1,manufacturer=X,product=Y,...` |
| `-H` | Exit on HLT instruction |
| `-D` | Destroy VM on power-off |
| `-S` | Wire guest memory (no swap) |
| `-A` | Generate ACPI tables |

## New Flags

### Migration

| Flag | Description |
|------|-------------|
| `--track-dirty` | Enable dirty page tracking (required for migration) |
| `--migrate-listen <ADDR:PORT>` | Destination mode: wait for incoming migration |
| `--control-socket <PATH>` | Unix socket for management commands |
| `--cpu-baseline <PROFILE>` | CPU feature mask: `host`, `avx2`, `sse42` |

### Metadata Agent

When no `-l com2,<path>` is specified, a built-in SmartOS metadata agent
runs on COM2 using these values:

| Flag | Description |
|------|-------------|
| `--mdata-nics <JSON>` | Network config (sdc:nics format) |
| `--mdata-resolvers <JSON>` | DNS resolvers: `'["8.8.8.8"]'` |
| `--mdata-ssh-keys <KEYS>` | SSH authorized keys for root |
| `--mdata-root-pw <PASS>` | Root password |

### Direct Boot

| Flag | Description |
|------|-------------|
| `--kernel <PATH>` | Linux bzImage or PVH ELF, classified from the file (bypasses UEFI) |
| `--initrd <PATH>` | initrd/initramfs |
| `--cmdline <STRING>` | Kernel command line |

## PCI Slot Syntax

Format: `[bus:]dev[:func],driver[,config...]`

Examples:
```
-s 4,nvme,/dev/zvol/dsk/zones/disk0
-s 6,virtio-net-viona,net0,promiscphys
-s 0:5:0,passthru,/dev/ppt0
-s 30:0,fbuf,vga=off,unix=/tmp/vm.vnc
-s 8,virtio-console,/var/run/rshyve/myvm/console.sock
-s 12,virtio-fs,/export/shared,tag=shared,ro
```

## LPC Device Syntax

```
-l bootrom,/usr/share/bhyve/uefi-rom.bin
-l bootrom,uefi                          # alias for the above
-l bootrom,bios                          # UEFI-CSM ROM
-l com1,stdio                            # serial to stdout/stdin
-l com1,/dev/zconsole                    # serial to device path
-l com2,/path/to/socket                  # override mdata agent
```

## Control Socket Commands

When `--control-socket` is specified, send newline-delimited JSON:

```bash
# VM status
echo '{"command":"status"}' | socat - UNIX-CONNECT:/path/to/sock

# Pause/resume
echo '{"command":"pause"}' | socat - ...
echo '{"command":"resume"}' | socat - ...

# Live migration (source side)
echo '{"command":"migrate-source","target_addr":"10.0.0.2:4567"}' | socat - ...

# Migration progress
echo '{"command":"migrate-status"}' | socat - ...
```

## firehyve

`firehyve` is the second binary in this workspace: a microVM over the
same crates, with a much smaller device surface. It shares this flag
grammar, so a `-c/-m/-s/-l` command line reads the same, but it rejects
at startup every flag it cannot honor rather than ignoring it.

```
firehyve [OPTIONS] --kernel <PATH> <VM_NAME>
```

| Difference | Detail |
|------------|--------|
| `--kernel` | Required. There is no UEFI path, so `-l bootrom,...` is rejected |
| `-l com1` | `stdio` only. `/dev/zconsole` and `socket,<path>` are rejected |
| `-l com2` | Not attached. There is no metadata agent, so `--mdata-*` is rejected |
| `-s <slot>,<driver>` | `virtio-blk` (alias `virtio-blk-pci`), `virtio-net-viona`, `virtio-rnd`, `virtio-console`, `virtio-fs`, `hostbridge` and `lpc` only. `nvme`, `ahci-*`, `fbuf`, `xhci` and `passthru` are rejected, not skipped |
| `-s ...,bootindex=N` | Rejected. A directly booted kernel has no firmware boot order |
| Migration | Not supported: `--migrate-listen` and `--control-socket` are rejected |
| Other | `--vtpm`, `--hyperv` and `--json-config` are rejected |

Example:

```bash
firehyve -c 2 -m 512M \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/$UUID/disk0 \
  -l com1,stdio \
  --kernel /var/tmp/vmlinux --cmdline "console=ttyS0 root=/dev/vda" \
  guest
```

## Example: boot.c Equivalent

The following matches what `boot.c` constructs for a typical Triton VM:

```bash
rshyve -H \
  -U $UUID \
  -B "1,manufacturer=Joyent,product=SmartDC HVM,serial=$ZONENAME" \
  -c $VCPUS -m ${RAM}M \
  -s 0,hostbridge \
  -s 4:0,virtio-blk,/dev/zvol/rdsk/zones/$UUID/disk0 \
  -s 29:0,virtio-net-viona,net0 \
  -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,/dev/zconsole \
  SYSbhyve-$ZONENAME
```
