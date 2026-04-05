# CLI Reference

## Synopsis

```
vmm [OPTIONS] <VM_NAME>
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
| `--kernel <PATH>` | Linux bzImage (bypasses UEFI) |
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

## Example: boot.c Equivalent

The following matches what `boot.c` constructs for a typical Triton VM:

```bash
vmm -H \
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
