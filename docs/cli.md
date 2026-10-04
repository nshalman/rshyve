# CLI Reference

## Synopsis

```
rshyve [OPTIONS] <VM_NAME>
```

## bhyve-compatible Flags

These flags match the existing C bhyve interface used by `boot.c`:

| Flag | Description |
|------|-------------|
| `-c <N>` | CPU count, or `cpus=N`. Add `maxcpus=M` for hotplug CPU slots, see [Hotplug](#hotplug). `sockets=`, `cores=` and `threads=` are refused: this VMM programs one socket with one thread per core |
| `-m <SIZE>` | Memory size. Bare number = MiB. Suffixes: K, M, G, T |
| `-s <slot,driver[,config]>` | PCI device (repeatable). See [Devices](devices.md) |
| `-l <device,config>` | LPC device (repeatable): `bootrom`, `com1`, `com2` |
| `-U <UUID>` | VM UUID (SMBIOS Type 1 + mdata `sdc:uuid`) |
| `-B <smbios>` | SMBIOS string: `1,manufacturer=X,product=Y,...` |
| `-H` | Exit on HLT instruction |
| `-S` | Draw guest memory from the VMM reservoir. C bhyve's `-S` means `memory.wired`, which is a different thing this VMM does not implement |
| `-A` | Accepted because `boot.c` passes it; ACPI tables are always generated |
| `-o <key=value>` | Config option (repeatable). Only `hotplug.maxmem` and `hotplug.memslot` are read; see [Hotplug](#hotplug). Any other key is passed over, but a misspelled `hotplug.` key is a startup error |

## New Flags

### Migration

| Flag | Description |
|------|-------------|
| `--no-track-dirty` | Turn off dirty page tracking. It is on otherwise |
| `--migrate-listen <ADDR:PORT>` | Destination mode: wait for incoming migration |
| `--control-socket <PATH>` | Unix socket for management commands |
| `--cpu-baseline <PROFILE>` | CPU feature mask: `host`, `avx2`, `sse42` |

### Hotplug

| Flag | Description |
|------|-------------|
| `--hotplug` | Publish the ACPI hotplug interface: the GPE0 block, the PCI, CPU and memory register files, and the matching AML. Off by default |
| `-c cpus=N,maxcpus=M` | `M` CPU slots, of which `N` boot. CPU hot-add needs `M` above `N`. `M` is at most 64 |
| `-o hotplug.maxmem=SIZE` | The guest address window hot-added memory goes in. Needs `--hotplug` and `-S` |
| `-o hotplug.memslot=SIZE` | Size of one hot-add memory slot. Default `128M`. It must divide `hotplug.maxmem`, and the window may need at most 8 slots |

Both binaries accept all four. Sizes use the `-m` grammar: a bare number
is MiB, and K, M, G and T suffixes work.

`-S` is required with `hotplug.maxmem` because a hot-add runs
`VM_ALLOC_MEMSEG` on a live VM, and that ioctl freezes every vCPU until
it returns. With reservoir memory the allocation costs microseconds at
any size. Without it the same call zeroes every page inside the lock,
about 400 ms per GiB, which the guest sees as a stall. Size the host
reservoir first, for example
`/usr/lib/rsrvrctl -c 512 -a <MiB>`.

Hot-REMOVE of a CPU or of memory is impossible on illumos. There is no
`vm_deactivate_cpu` and no `VM_FREE_MEMSEG`. `cpu-remove` and
`mem-remove` are accepted and always answered with that reason. Device
removal is advisory: the guest has to run `_EJ0`, and a guest that never
does leaves the slot in `remove-pending` with no host state changed.

A VM that has hot-added anything cannot be a migration source. For a
CPU or memory add, the destination is built from the same command line
and would not have the addition. For a PCI add, device state on the
migration wire is matched by position, so the destination would restore
another device's state. Both are refused by `migrate-source`, naming the
reason.

Example, a VM that can grow from 2 to 8 CPUs and add up to 4 GiB:

```bash
rshyve -H -S --hotplug \
  -c cpus=2,maxcpus=8 -m 2G \
  -o hotplug.maxmem=4G -o hotplug.memslot=512M \
  -s 0,hostbridge \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/$UUID/disk0 \
  -s 29:0,virtio-net-viona,net0 \
  -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,stdio \
  --control-socket /var/run/rshyve/growable.sock \
  growable
```

That VM claims PIO `0xAFE0` (GPE0), `0xAE00` (PCI slots), `0xAF00` (CPU
slots) and `0x0A00` (memory slots). Without `--hotplug` none of those
ports are claimed and the ACPI tables are unchanged.

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
-s 5,nvme,mantablock=/var/run/mantablock/<uuid>.sock,volume=<uuid>
-s 6,virtio-net-viona,net0,promiscphys
-s 0:5:0,passthru,/dev/ppt0
-s 30:0,fbuf,vga=off,unix=/tmp/vm.vnc
-s 8,virtio-console,/var/run/rshyve/myvm/console.sock
-s 12,virtio-fs,/export/shared,tag=shared,ro
```

### NVMe on a MantaBlock volume

```
-s <slot>,nvme,mantablock=<socket>,volume=<uuid>[,allow-volatile]
```

When the first field after `nvme` is `mantablock=`, the namespace is a
MantaBlock volume and not a file. rshyve connects to the engine's Unix
socket at `<socket>` (an absolute path) and attaches `<uuid>`. A spec
whose first field is a path is the file disk it always was.

| Option | Meaning |
|--------|---------|
| `mantablock=<socket>` | Required, first. The engine's socket for the volume |
| `volume=<uuid>` | Required. The volume to attach |
| `allow-volatile` | Take an engine whose writes are not durable when they complete. For tests and measurement |

The namespace has 4 KiB logical blocks. It reports no volatile write
cache when the engine says every write is durable when it completes.
An engine that does not say so is refused at startup, unless
`allow-volatile` is given: the namespace then reports a volatile write
cache, and the guest sends flushes.

The grammar is strict: an unknown option, a repeated option or a
relative socket path is a startup error. A ring option after a file
path (`nvme,/disk,volume=...`) is an error too, so a misplaced option
cannot boot the guest on the file. If the engine cannot be reached or
refuses the volume, rshyve does not start.

A VM with such a disk cannot be a migration destination yet.

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

### Hotplug commands

`device-add` takes the same `slot,driver[,config]` spec `-s` takes and
answers with the id `device-remove` needs. `device-remove` only asks the
guest; the device leaves when the guest runs `_EJ0`, and `device-list`
reports `remove-pending` until it does.

```bash
echo '{"command":"device-list"}' | socat - ...
echo '{"command":"device-add","spec":"5,virtio-blk,/dev/zvol/rdsk/zones/d1"}' | socat - ...
echo '{"command":"device-remove","id":"virtio-blk@5"}' | socat - ...

echo '{"command":"cpu-list"}' | socat - ...
echo '{"command":"cpu-add","id":2}' | socat - ...

# bytes, rounded up to a whole hotplug.memslot
echo '{"command":"mem-list"}' | socat - ...
echo '{"command":"mem-add","bytes":536870912}' | socat - ...
```

`cpu-remove` and `mem-remove` are recognised and always refused, so the
answer says why the platform cannot do it. A request for an engine the
VM was not started with is refused too, naming the flag that is missing.
Every hot-add needs the VM to be running: a paused or migrating VM
answers with its state instead.

## firehyve

`firehyve` is the second binary in this workspace: a microVM over the
same crates, with a much smaller device surface. It shares this flag
grammar, so a `-c/-m/-s/-l` command line reads the same. A flag that
would change guest behaviour is rejected at startup rather than
ignored. The exception is what the bhyve zone brand puts on every zone
and offers no way to leave out: those are accepted, left unused and
logged at `warn`, because refusing them would mean an unmodified brand
could never start the binary.

```
firehyve [OPTIONS] --kernel <PATH> <VM_NAME>
```

| Difference | Detail |
|------------|--------|
| `--kernel` | Required. There is no UEFI path. `-l bootrom,<rom>`, which `boot.c` appends to every zone, is accepted and logged; nothing is loaded from the ROM |
| `-l com1` | `stdio`, or an absolute device path such as the `/dev/zconsole` the brand names, which is served in both directions so `zlogin -C` works. Any other backend (`socket,<path>`, a relative path) falls back to stdio and is logged |
| `-l com2` | Always attached, transmit only, mirrored to stdout as a marker channel for `fhrun-init`. A `com2,<backend>` from the argv is accepted and logged; there is no metadata agent, so `--mdata-*` is rejected |
| `-s <slot>,<driver>` | `virtio-blk` (alias `virtio-blk-pci`), `virtio-net-viona`, `virtio-rnd`, `virtio-console`, `virtio-fs` and `virtio-vsock` are attached; `hostbridge` and `lpc` name what the chipset already made. `fbuf` and `xhci`, which the brand adds unless VNC is off, are accepted in the startup argv, left unattached and logged; a hot-add of either is refused. `nvme`, `ahci-*` and `passthru` are rejected, not skipped |
| `-s ...,bootindex=N` | Rejected. A directly booted kernel has no firmware boot order |
| Migration | Not supported: `--migrate-listen` and `--control-socket` are rejected |
| `--hotplug` | Accepted, with the same `-c maxcpus=` and `-o hotplug.*` options as rshyve. The commands arrive on the vsock `CONTROL` verb, so the VM also needs a `-s <slot>,virtio-vsock,<path>,cid=<n>` device or nothing can reach them |
| Other | `--vtpm`, `--hyperv` and `--json-config` are rejected |

Example:

```bash
firehyve -c 2 -m 512M \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/$UUID/disk0 \
  -l com1,stdio \
  --kernel /var/tmp/vmlinux --cmdline "console=ttyS0 root=/dev/vda" \
  guest
```

### firehyve CONTROL verb

firehyve has no control socket, so the same commands are served on the
host socket of a `virtio-vsock` device. A peer that sends `CONNECT
<port>` is joined to a guest port; a peer that sends `CONTROL` is
answered by the VMM itself and reaches no guest. One request per line,
one response line back, `OK ...` or `ERR <reason>`.

```
CONTROL
device-list                     -> OK 2 virtio-blk@4,0.4.0,present ...
device-add 5,virtio-blk,/path   -> OK virtio-blk@5
device-remove virtio-blk@5      -> OK virtio-blk@5,remove-pending
cpu-list                        -> OK 4 0,present,boot 2,absent,hotplug ...
cpu-add 2                       -> OK 2,present
mem-list                        -> OK 1 boot,1073741824,present
mem-add 134217728               -> OK slot0,present
```

Every answer is one line. A list answer is `OK <count>` followed by one
field per record, and a record is its values joined by commas, so a
client splits on whitespace and then on commas. No value carries a space
or a comma. `mem-add` takes BYTES on both transports, not the `-m` size
grammar.

The socket is the path in the device spec, for example
`-s 9,virtio-vsock,/var/run/firehyve/guest.sock,cid=3`. It is 0600 in a
0700 directory, so the peer is already the VMM's own user:

```bash
printf 'CONTROL\ncpu-add 2\n' | socat - UNIX-CONNECT:/var/run/firehyve/guest.sock
```

`virtio-vsock` itself cannot be hot-added: its CONTROL slot is armed
once, at startup.

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

## Flags this VMM refuses

`-D`, `-k` and `--json-config` are C bhyve flags this VMM does not
implement, so it refuses them instead of ignoring them. `-D` (destroy on
power-off) would change the lifecycle the zone brand relies on, and `-k`
and `--json-config` name configuration files nothing here reads, so a VM
would boot with defaults and no word about it.
