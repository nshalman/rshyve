# Feature reference

`rshyve` and `firehyve` are two binaries built from one workspace. `rshyve` stands in for the illumos `bhyve(8)` command on the command line the zone brand builds: UEFI firmware, NVMe, AHCI CD-ROM, a VNC framebuffer, PCI passthrough, a control socket, and live migration. It is not all of bhyve, and the tables below separate what it implements from what it accepts and ignores and what it refuses. `firehyve` is a microVM that boots a Linux kernel directly and links a deliberately smaller crate set; it attaches six virtio drivers, accepts the two chipset names and the two graphical-console names the bhyve zone brand always emits, and rejects everything else at startup instead of skipping it. Choose `firehyve` when the guest is a Linux kernel you supply and the device list is virtio only; choose `rshyve` for anything that needs firmware, a graphical console, or migration.

Everything below is traced to code. Where a shipped document contradicts the code, the code wins and the contradiction is listed at the end.

## At a glance

Markers: **yes** works, **caveat** works with a condition named in the matching section, **exp** present but experimental, **no** absent, **reject** a startup error naming the unsupported item (not a silent skip).

| Capability | rshyve | firehyve | Implemented at |
|---|---|---|---|
| **Boot** | | | |
| Direct Linux bzImage (`--kernel`) | yes | yes (required) | `crates/vmm-boot/src/direct.rs` `KernelImage::from_bytes` |
| PVH ELF kernel (`--kernel`) | yes | yes | `crates/vmm-boot/src/elf.rs` `PvhKernel::from_bytes` |
| `--initrd` / `--cmdline` | yes | yes | `crates/vmm-boot/src/initrd.rs`, `direct.rs:326`, `pvh.rs:209` |
| UEFI bootrom (`-l bootrom,...`) | yes | reject | `bin/rshyve/src/bootrom.rs:196`; `bin/firehyve/src/main.rs:68` |
| Persistent UEFI variable store | caveat | reject | `bin/rshyve/src/varstore.rs:118` |
| `bootindex=N` fw_cfg boot order | yes | reject | `bin/rshyve/src/bootorder.rs:53`; `bin/firehyve/src/main.rs:75` |
| In-place guest reboot | yes (re-exec) | no (exits 0) | `bin/rshyve/src/reboot.rs`; `bin/firehyve/src/main.rs:263` |
| **Storage** | | | |
| `virtio-blk` / `virtio-blk-pci` | yes | yes | `crates/vmm-virtio/src/block.rs`, `attach.rs:91` |
| `nvme` | caveat | reject | `crates/vmm-storage/src/nvme/mod.rs:185`; `bin/rshyve/src/main.rs:1064` |
| `ahci-cd` (ATAPI CD-ROM, read only) | yes | reject | `crates/vmm-storage/src/ahci/`; `bin/rshyve/src/main.rs:1231` |
| `ahci-hd` | no (hard error) | reject | `bin/rshyve/src/main.rs:974` `reject_ahci_hd` |
| qcow2 or any non-raw image, block I/O throttling | no | no | raw offset arithmetic in every backend; no limiter anywhere |
| **Network** | | | |
| `virtio-net-viona` | yes | yes | `crates/vmm-virtio/src/viona.rs`, `attach.rs:170` |
| Any other NIC model (e1000, tap, slirp) | no | reject | no match arm exists |
| **Serial and console** | | | |
| `-l com1,stdio` | yes | yes | `bin/rshyve/src/main.rs:1279`; `bin/firehyve/src/console.rs:60` |
| `-l com1,socket,<path>` | yes | reject | `bin/rshyve/src/main.rs:1459`; `console.rs:46` |
| `-l com1,<device path>` (`/dev/zconsole`) | yes | reject | `bin/rshyve/src/main.rs:1279`; `console.rs:46` |
| COM2 as a general backend | yes | no (fixed) | firehyve COM2 is transmit only to stdout, `console.rs:111` |
| `virtio-console` (Unix socket) | yes | yes | `crates/vmm-virtio/src/console.rs` |
| **Filesystem** | | | |
| `virtio-fs` host passthrough | yes | yes | `crates/vmm-virtio/src/fs/` |
| virtio-fs DAX, xattrs, ACLs, locking | no | no | `fs/server.rs:112` dispatch |
| **Display** | | | |
| `fbuf` framebuffer plus VNC on a Unix socket | caveat | reject | `crates/vmm-hid/src/fbuf/mod.rs:163` |
| VNC over TCP, bhyve `wait`, VGA emulation | no | no | no `TcpListener` in `crates/vmm-hid`; no 0x3C0/0x3D4 handlers |
| **Input** | | | |
| PS/2 keyboard (always present, no flag) | yes | no | `bin/rshyve/src/input.rs` `setup_ps2` |
| `xhci` USB 3.0 with absolute tablet | caveat | reject | `crates/vmm-hid/src/xhci/mod.rs:198` |
| USB keyboard | no | no | absent from the tree |
| PS/2 mouse motion | no | no | `ps2/mouse.rs` `movement()` returns zeros |
| **Migration and control** | | | |
| Live migration over a Unix socket | exp | reject | `crates/vmm-migrate/src/source.rs`, `destination.rs` |
| Live migration over TCP | exp | reject | the same driver over `std::net::TcpStream` (`crates/vmm-migrate/src/wire.rs`) |
| Control socket (`--control-socket`) | yes | reject | `bin/rshyve/src/control.rs`; `bin/firehyve/src/main.rs:46` |
| Dirty page tracking | on by default | never | `bin/rshyve/src/main.rs:175`; `bin/firehyve/src/main.rs:116` |
| **Hotplug (`--hotplug`, off by default)** | | | |
| ACPI hotplug interface (GPE0 plus three register files) | yes | yes | `crates/vmm-machine/src/pm.rs:139,166`; `crates/vmm-devices/src/acpi/hotplug/mod.rs:20` |
| PCI hot-add, any driver the binary's own `-s` catalog builds | caveat | caveat | `crates/vmm-machine/src/hotplug.rs:536` |
| PCI hot-remove, guest runs `_EJ0` | caveat | caveat | advisory, `crates/vmm-machine/src/hotplug.rs:546,292` |
| CPU hot-add | caveat | caveat | needs `-c cpus=N,maxcpus=M`, `crates/vmm-machine/src/hotplug/cpu.rs:287` |
| CPU hot-remove | no (kernel) | no (kernel) | no `vm_deactivate_cpu`, refused at `hotplug/cpu.rs:227` |
| Memory hot-add | caveat | caveat | needs `-S` and `-o hotplug.maxmem`, `crates/vmm-machine/src/hotplug/mem.rs:389` |
| Memory hot-remove | no (kernel) | no (kernel) | no `VM_FREE_MEMSEG`, refused at `hotplug/mem.rs:464` |
| Hotplug commands | control socket | vsock `CONTROL` verb, needs a `virtio-vsock` device | `bin/rshyve/src/control/dispatch.rs:202-252`; `bin/firehyve/src/control.rs:287-297` |
| Migration after any hot-add, PCI included | reject | n/a | `bin/rshyve/src/control/migrate.rs:84,64` |
| **Platform** | | | |
| vTPM 2.0 (`--vtpm`) | caveat | reject | `crates/vmm-tpm/src/crb.rs`; `bin/firehyve/src/main.rs` `validate_cli` |
| Hyper-V enlightenments (`--hyperv`) | exp | reject | `crates/vmm-hyperv/src/lib.rs`; `validate_cli` |
| `--cpu-baseline` CPUID masking | yes | yes | `crates/vmm-core/src/cpuid.rs:32` |
| PCI passthrough (`passthru,/dev/pptN`) | caveat | reject | Intel only, no MSI-X device; `crates/vmm-passthru/`; `bin/rshyve/src/main.rs:1412` |
| `virtio-rnd` | yes | yes | `crates/vmm-virtio/src/attach.rs:341` |
| SmartOS metadata agent on COM2 | yes | no (rejects only `--mdata-*`) | `bin/rshyve/src/mdata.rs` |
| `--json-config`, `-k` | accepted, no reader | reject (`--json-config`) | `crates/vmm-config/src/lib.rs` |
| `-o hotplug.maxmem=`, `-o hotplug.memslot=` | yes | yes | `crates/vmm-config/src/lib.rs:374,398`; every other `-o` key is passed over |

### What firehyve rejects at startup

`validate_cli` (`bin/firehyve/src/main.rs`) errors on: a missing `--kernel`, `--migrate-listen`, `--control-socket`, `--json-config`, every `--mdata-*` flag, `--vtpm`, `--vtpm-state-dir`, `--hyperv`, `--tsc-freq-hz`, and any `-s` spec containing a `bootindex=` token; `hardening_from_cli` errors on `--priv-drop=false` and on a `virtio-fs` spec. `split_spec` (`bin/firehyve/src/devices.rs`) then rejects any PCI driver outside its `SUPPORTED` list.

What the bhyve zone brand forces onto every zone is accepted instead, because refusing it would mean an unmodified brand could never start the binary. Each is logged at `warn` and changes no guest behaviour: `-l bootrom,<rom>` (nothing is loaded from it), a `-l com1`/`com2` backend firehyve cannot serve (the port falls back to inherited stdio, which zoneadmd copies into the zone log rather than the zone console), and the `fbuf` and `xhci` specs `add_fbuf` emits (the slots stay empty). COM1 does serve an absolute device path, so the brand's `/dev/zconsole` is a working console. The startup argv is the only place these are tolerated: a hot-add of `fbuf` or `xhci` over the `CONTROL` verb is refused.

Because both binaries share one clap derive (`crates/vmm-config/src/lib.rs`), `firehyve --help` still lists the rshyve-only flags. The refusal happens at runtime, not in the parser.

## Device specs

Both binaries parse `-s <addr>,<driver>[,<config>]` with the same helper (`crates/vmm-machine/src/parse.rs:10` `parse_bdf`, `splitn(3, ',')` in each binary). Address forms: `dev`, `dev:func`, or `bus:dev:func`. Decimal only, no `0x`. `dev` is 0..=31 and `func` is 0..=7 (`crates/vmm-devices/src/pci/mod.rs:45`).

Slots 0 and 1 are already taken by the chipset (host bridge at 0.0.0, LPC bridge at 0.1.0, `crates/vmm-devices/src/chipset/i440fx.rs:87,96`). `-s 0,hostbridge` and `-s 31,lpc` are accepted and attach nothing, but `-s 0,<real driver>` or `-s 1,<real driver>` panics on the duplicate-BDF assert at `crates/vmm-devices/src/pci/bus.rs:67`. The LPC bridge is at slot **1**, not slot 31, whatever the spec text says.

Option strictness is not uniform, and this matters:

| Driver | Unknown option |
|---|---|
| `virtio-blk`, `virtio-blk-pci`, `nvme` | silently ignored (`attach.rs:78`) |
| `virtio-net-viona` | silently ignored (`attach.rs:157`) |
| `fbuf`, `passthru`, `xhci`, `virtio-rnd`, `hostbridge`, `lpc` | silently ignored or never read |
| `virtio-console` | hard error (`console.rs:346`) |
| `virtio-fs` | hard error (`fs/mod.rs:117`) |
| `ahci-cd` | hard error (`bin/rshyve/src/main.rs:966`) |

An unknown **driver name** is a hard error in firehyve and a `warn`-level log line in rshyve, which then boots the VM without that device (`bin/rshyve/src/main.rs:1263`). Only `ahci-hd` has a dedicated error arm ahead of that catch-all.

Attach order is argv order. The first device-creation failure aborts the whole pass, so no later spec is built (`crates/vmm-machine/src/pci.rs:103-132`).

## Boot

There is no `--boot` or `--protocol` flag. `crates/vmm-boot/src/image.rs:42` classifies the file: `\x7fELF` means PVH, otherwise `HdrS` at offset 0x202 means bzImage, otherwise startup fails with "unrecognized kernel image". ELF magic wins first, so an ELF without a Xen `PHYS32_ENTRY` note is not retried as a bzImage.

**bzImage requirements**: file longer than 0x290 bytes, protocol version at 0x206 >= 0x020C, `XLF_KERNEL_64` set at 0x236, `setup_sects` consistent with the file length. The image is written verbatim at its own `pref_address` with entry `pref_address + 0x200`; there is no relocation and `relocatable_kernel` / `kernel_alignment` are never read.

**PVH requirements**: ELF64, little endian, `EM_X86_64`, at least one `PT_LOAD` with a non-zero `p_filesz`, and a `PT_NOTE` carrying `XEN_ELFNOTE_PHYS32_ENTRY` (`CONFIG_PVH=y`). Only `p_filesz` bytes are copied; BSS relies on guest RAM being zero.

`--cmdline` defaults to `console=ttyS0 earlyprintk=serial` in both binaries and is written to GPA 0x20000. `--initrd` is placed at the highest page-aligned address below `min(mem_size, 0xC000_0000)`.

In rshyve, `--kernel` suppresses all UEFI setup and any co-supplied `-l bootrom,...` is ignored with no warning (`bin/rshyve/src/main.rs:132,152`). `--initrd` is opened and validated even with no `--kernel`, then never used.

### UEFI (rshyve only)

```
-l bootrom,<rom>[,<varfile>]
-l bootrom,uefi        # alias for /usr/share/bhyve/uefi-rom.bin
-l bootrom,bios        # alias for /usr/share/bhyve/uefi-csm-rom.bin
```

With no `--kernel` and no bootrom argument at all, `/usr/share/bhyve/uefi-rom.bin` is used (`bootrom.rs:309,330`). The device token match is case insensitive. A third field containing `=` is treated as a legacy option, debug-logged, and dropped; the symbolic names are not expanded in the varfile position. If several bootrom arguments are given, the first wins and the rest are warned about.

The variable store is emulated CFI flash (commands 0x10, 0x50/0x70, 0xFF) mapped immediately below the ROM. The file is `MAP_SHARED`, holds an `fcntl` write lock so two VMMs cannot share it, and is validated for the `EFI_SYSTEM_NV_DATA_FV` GUID and `_FVH` signature unless it is all 0xFF (blank). Persistence is `msync` on teardown and on the control socket's durable flush.

### Worked command lines

rshyve, UEFI guest with a VNC console:

```
rshyve -c 4 -m 8G \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin,/zones/vm0/vars.fd \
  -l com1,socket,/var/run/rshyve/vm0/console.sock \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/vm0/disk0,num-queues=4 \
  -s 5,ahci-cd,/zones/vm0/install.iso,bootindex=1 \
  -s 6,virtio-net-viona,vm0_net0 \
  -s 30:0,fbuf,unix=/var/run/rshyve/vm0/vnc.sock,w=1280,h=1024 \
  -s 30:1,xhci \
  --control-socket /var/run/rshyve/vm0/control.sock \
  vm0
```

firehyve, direct-boot microVM:

```
firehyve -c 2 -m 512M \
  --kernel /var/lib/firehyve/vmlinux \
  --initrd /var/lib/firehyve/initramfs.cpio \
  --cmdline "console=ttyS0 root=/dev/ram0 panic=-1" \
  -l com1,stdio \
  -s 4,virtio-blk,/var/lib/firehyve/rootfs.img,ro \
  -s 7,virtio-net-viona,fh0 \
  -s 12,virtio-fs,/export/share,tag=share,ro \
  -s 15,virtio-console,/var/run/fh/console0.sock \
  fh0
```

## Storage

### virtio-blk (both binaries)

```
-s <slot>,virtio-blk,<path>[,ro][,nodelete][,sectorsize=N][,num-queues=N]
```

`parse_blk_config` (`crates/vmm-virtio/src/attach.rs:55`) parses exactly those four keys. Defaults: writable, `sectorsize` 512, `num-queues` = `min(num_vcpus, 4)` floored at 1. `sectorsize=N` is applied only if N is a power of two in 512..=65536; `num-queues=N` only if N is in 1..=8. Out-of-range values are discarded with no diagnostic and the default stands.

The backing store is opened as a plain file: a raw image, a zvol raw node (`/dev/zvol/rdsk/...`), or an ISO all work; a writable block node is refused (see [Disk flush and write cache](#disk-flush-and-write-cache)). Capacity comes from `fstat` alone; there is no `DKIOCGMEDIAINFO` fallback anywhere in the tree, so a backing store whose stat size is 0 presents as a zero-capacity disk. Transport is transitional PCI (device id 0x1001 with modern capability structures), ring size 128, MSI-X vectors `num_queues + 1`.

`ro` is enforced three times: the file is opened without write, `VIRTIO_BLK_F_RO` is set, and `T_OUT` returns `VIRTIO_BLK_S_IOERR` (`block.rs:315,272,646`). It also suppresses DISCARD advertisement and skips the zvol write-cache ioctl.

Two traps in this parser:

- `nodelete` is parsed, stored, and never read. The field carries `#[allow(dead_code)]` at `block.rs:113`.
- `sectorsize=4096` reports a disk one eighth of its true size. `block.rs:147` divides the file length by the configured sector size, but the virtio config `capacity` field is defined in 512-byte units. The range check uses the same shrunken value, so the guest is confined to part of the device rather than corrupting anything, but the geometry is wrong.

The guest-visible serial from `VIRTIO_BLK_T_GET_ID` is the constant string `vmm-virtio-blk` for every disk in the VM (`block.rs:206`), so a guest cannot tell two virtio-blk disks apart by serial.

### NVMe (rshyve only)

```
-s <slot>,nvme,<path>[,ro]
-s <slot>,nvme,mantablock=<socket>,volume=<uuid>[,allow-volatile][,spin-us=<n>]
```

The second form puts the namespace on a MantaBlock volume, with 4 KiB blocks and no write cache when the engine's writes are durable. It is parsed strictly; see [cli.md](cli.md#nvme-on-a-mantablock-volume). The rest of this section is about the first form.

The arm calls the shared `parse_blk_config`, so `nodelete`, `sectorsize=` and `num-queues=` all **parse and are then discarded**. Only `read_only` reaches `NvmeController::new` (`bin/rshyve/src/main.rs:1077`). The arm builds the default `NvmeConfig`, 512-byte LBA and a volatile write cache. `NvmeConfig` can choose 4 KiB and no cache for a backend whose writes are durable, but no option reaches it from the command line, so `sectorsize=4096` silently yields a 512-byte-LBA namespace.

One namespace, NSID must be 1. Zero-copy `preadv`/`pwritev` straight into guest pages, 8 worker threads, MDTS 9 (2 MiB), up to 15 I/O queues of up to 4096 entries. Admin commands implemented: Identify (CNS 0 and 1 only), Create/Delete IO SQ and CQ, Get/Set Features, Abort (always reports not found), Get Log Page (a stub that writes zeros). I/O opcodes: READ, WRITE, FLUSH. Dataset Management, Write Zeroes and Compare are absent and `ONCS` is correctly left at zero.

`-s N,nvme,<iso>,ro` is the tested path for presenting installation media to a UEFI guest (`FEATURES_TODO.md:45`). A write to a read-only namespace returns `SC_DATA_XFER_ERROR`, not the spec-correct 0x82 "Attempted Write to Read Only Range", so guest logs show a transfer error rather than a permission error.

Serial is `TRITON-NVME-{:04}` derived from the PCI **device number only** (`main.rs:1080`), so two NVMe controllers at different functions of one slot get identical serials.

### AHCI CD-ROM (rshyve only)

```
-s <slot>,ahci-cd,<iso-path>
```

No options. Any non-empty field after the path is a hard error. `bootindex=N` is the one token that coexists, because it is stripped before this parse.

Real ATAPI over an Intel ICH8 AHCI controller (vendor 0x8086, device 0x2821, BAR5 ABAR). Read-only by construction: `IsoMedia` opens with `read(true)` and refuses a file under 2048 bytes. One port, one fixed ISO, no media change or tray simulation beyond an eject `CHECK CONDITION`. INTx only, no MSI-X. Reads are synchronous `preadv` on the vCPU thread with no worker pool, so a slow ISO read stalls a vCPU. Max transfer 1 MiB, PRDTL 512 entries.

SCSI/MMC commands implemented: TEST UNIT READY, REQUEST SENSE, INQUIRY, START/STOP UNIT, PREVENT/ALLOW MEDIUM REMOVAL, READ CAPACITY, READ(10), READ TOC (formats 0 and 1; formats 2 and 3 return ILLEGAL REQUEST), GET EVENT STATUS NOTIFICATION, MODE SENSE(10) pages 0x01 and 0x2A, REPORT LUNS, READ(12). Everything else is an ILLEGAL REQUEST.

### Disk flush and write cache

A writable virtio-blk or NVMe backing fd is handled by type (`crates/vmm-devices/src/blkdev.rs`). Skipped entirely under `ro`. `ahci-cd` does not use it.

- **Regular file**: guest flushes call fsync.
- **Block device** (`/dev/zvol/dsk/...`, `/dev/dsk/...`, `/dev/lofi/...`): refused when writable. For a `dsk` path under `/dev` the error names the raw node to use; for `/dev/lofi` it says to use a zvol, since raw lofi is refused too; for any other path it says to use the raw (character) node. Specfs answers ENOTTY to every disk ioctl on a block node, and writes wait in its page cache (full MAXBSIZE blocks also start async writeback). fsync drains that cache and tries the device flush but hides both errors, and after one ENOTSUP or ENOTTY it never flushes the node again. A read-only block node is allowed.
- **zvol character device** (`/dev/zvol/rdsk/...`), found by the driver's `DKIOCINFO` drive name `zvol`, not by path: the write cache is turned on (`DKIOCSETWCE=1`, best effort) for speed. Every guest flush sends `DKIOCFLUSHWRITECACHE`, which on a zvol always commits the ZIL, and every error goes back to the guest. On drop, the cache is turned off if it was off when this open checked (best effort). Two opens can both see it off, both turn it on and both turn it off; that is harmless because every zvol flush commits the ZIL (with `sync=disabled` nothing commits and the cache state changes nothing either). A failed cache change is logged as a warning. If the process dies without running drop, the cache stays on until the zvol minor is recreated, which is safe for the same reason.
- **Any other character device** (`/dev/rdsk/...`): the write cache is never changed. `DKIOCGETWCE` answering anything but on, off, ENOTSUP or ENOTTY refuses the disk. If it reads on, `DKIOCSETWCE=1` is sent once to refresh sd's own copy of the cache state (sd answers a flush with success and sends nothing while that copy says off), and the disk is refused if that call fails (sd answers EINVAL when the cache mode is not changeable or `sd.conf` disables the cache). Then `DKIOCFLUSHWRITECACHE` is probed:
  - The probe works: every guest flush sends the ioctl and every error goes back to the guest.
  - The probe answers ENOTSUP (the driver says it has no cache, for example an NVMe namespace without a volatile write cache): accepted, with no-op flushes, only if `DKIOCGETWCE` read off or answered ENOTSUP or ENOTTY. If it read on, the disk has a volatile cache it cannot flush and is refused. This trusts a driver that answers ENOTSUP to have no volatile cache (for example vioblk without the FLUSH feature in a nested guest).
  - The probe answers ENOTTY (the driver does not know the flush ioctl): refused. Raw lofi (`/dev/rlofi/...`) is one: it can wrap an sd or NVMe device, and its fsync of that device goes through specfs, which drops the device flush result. Attach it read-only or use a zvol.
  - Any other errno from the probe refuses the disk.

  Every refusal says what to do next: attach the disk read-only, turn its write cache off, or use a zvol.

The probe result is decided once, at open. If the cache is turned on out of band after that (a uscsi MODE SELECT, a drive reset that restores WCE=1, an NVMe passthrough SET FEATURES), the driver's own flag can go stale and later flushes can be no-ops. Userland cannot detect this, and every user of the flush ioctl has the same exposure. sd's `suppress_cache_flush` setting also makes flushes no-ops by design.

## Network

```
-s <slot>,virtio-net-viona,<vnic>[,promiscphys]
```

The only network driver either binary matches. `parse_viona_config` (`attach.rs:154`) recognizes the VNIC name and the literal `promiscphys` or `promiscphys=true`. `promiscphys=false` does not match and therefore leaves promiscuous mode off. Anything else, including `mtu=9000`, is ignored.

Host requirements, all hard: illumos, an openable `/dev/viona`, `libdladm` linked in, and a VNIC that already exists. Neither binary creates or configures a VNIC.

Two rx/tx queues of 256 entries, 3 MSI-X vectors (rx, tx, config change), one `viona-intr-poll` thread per device. `EVENT_IDX` is deliberately stripped because viona manages notification suppression in the kernel. Only the low 32 bits of the kernel feature word are used, so any kernel-offered bit at 32 or above is dropped; `VERSION_1` is re-added by the PCI transport.

Two facts an operator should know. Link status is hardcoded `VIRTIO_NET_S_LINK_UP` (`viona.rs:50`); there is no link-down reporting. And if `dladm_vnic_info` fails while `dladm_name2info` succeeded, the MAC is left as all zeros with no error and no log line (`viona.rs:710`), so the guest reads `00:00:00:00:00:00` out of device config.

There is no multiqueue, no MTU setting, and no `VIRTIO_NET_F_CTRL_VQ`. The `VNA_IOC_GET_PAIRS`/`SET_MTU` ioctls exist in `crates/viona-api` and nothing outside that crate calls them. The viona interface version is likewise never checked at startup, so a mismatched kernel module surfaces as a raw ioctl errno.

Per-binary difference in behavior, not acceptance: rshyve passes `defer_poll = cli.migrate_listen.is_some()` so the kernel poll thread waits for ring restore on a migration destination; firehyve hardcodes `defer_poll = false` because it is never a destination.

## Serial and console

### LPC UARTs

COM1 is 16550 at 0x3F8 IRQ 4; COM2 at 0x2F8 IRQ 3 (`crates/vmm-devices/src/uart/lpc.rs`).

rshyve backends (`bin/rshyve/src/main.rs:1279` `setup_serial_io`):

| Form | Behavior |
|---|---|
| absent or `-l com1,stdio` | stdout for TX, a thread reading stdin |
| `-l com1,socket,<path>` | Unix listener, socket 0600 in a 0700 directory |
| `-l com1,<anything else>` | treated as a device path, opened `O_RDWR|O_NONBLOCK|O_CLOEXEC`, `cfmakeraw` plus `CLOCAL` on illumos (this is the `/dev/zconsole` path) |

The `-l` grammar as a whole is not validated. Only the `bootrom`, `com1` and `com2` prefixes are ever looked up (`crates/vmm-machine/src/parse.rs:33`), so `-l com3,stdio` or a misspelling is silently ignored.

The socket backend serializes clients: the accept loop bridges one connection fully and a second connect waits in the backlog. TX is a blocking `write_all` on the vCPU thread with no write timeout, so a connected peer that stops reading can stall a guest vCPU. Guest output with no client attached is dropped.

If no `-l com2,<backend>` is given, rshyve starts the built-in metadata agent on COM2 instead. Any `-l com2` value routes COM2 to that backend and the agent never runs.

firehyve accepts only an absent backend or `stdio` on either port (`bin/firehyve/src/console.rs:46` `require_stdio_backend`). COM2 is **always attached**, transmit only, mirrored to the process stdout, with no stdin reader; it exists so `fhrun-init` can write boot markers to `/dev/ttyS1` that never pass through the kernel console.

### virtio-console

```
-s <slot>,virtio-console,<unix-socket-path>
```

No options, and this is enforced: any comma-separated field after the path is a hard error, as is an empty config. A socket path containing a comma cannot be expressed.

Single port. No `VIRTIO_CONSOLE_F_MULTIPORT`, no `F_SIZE`, no control queues, so there is no terminal-size reporting and no way to ask for a second port. Two queues of 128, 3 MSI-X vectors. The 12-byte config region reads all zeros. Guest device is `/dev/hvc0`.

One client at a time, and a new connection **evicts** the incumbent rather than queueing (`console.rs` `replace_client`). Guest output with no client attached is discarded. Host input with no posted guest RX chain is buffered up to 64 KiB then dropped with one warning per burst. Writes to the client have a 250 ms timeout; a stalled client loses its console.

## Shared filesystem

```
-s <slot>,virtio-fs,<host-dir>[,tag=NAME][,ro][,queue-size=N]
```

Exactly three options (`crates/vmm-virtio/src/fs/mod.rs:85`). `tag` defaults to the last path component of the host directory, is capped at 36 bytes, and must contain no NUL. `queue-size` defaults to 128, must be in 8..=1024 or startup fails, and is then rounded **up** to a power of two, so `queue-size=100` yields 128. An unknown option is a hard error.

Mount in Linux with `mount -t virtiofs <tag> /mnt`.

Two queues (hiprio and one request queue) served by a single worker thread, so the hiprio queue confers no real priority: a `FUSE_INTERRUPT` waits behind whatever is in flight. No DAX and no shared-memory window; every reply travels through the request queue.

Opcodes implemented: INIT, DESTROY, LOOKUP, FORGET, BATCH_FORGET, GETATTR, STATFS, ACCESS, READLINK, OPEN, RELEASE, READ, OPENDIR, RELEASEDIR, READDIR, READDIRPLUS, FLUSH, FSYNC, FSYNCDIR, WRITE, CREATE, MKDIR, MKNOD, UNLINK, RMDIR, RENAME, RENAME2, SETATTR, SYMLINK, LINK. GETXATTR and LISTXATTR return ENOTSUP; everything else returns ENOSYS. FLUSH and DESTROY are accepted and do nothing (`fs/server.rs:114,138`), which is normal for a passthrough but means neither flushes host state. Negotiated limits: max_write 512 KiB, max_background 64, and a hard 1 MiB cap on any single request body.

Path-escape defense is real and tested. `validate_name` (`fs/passthrough.rs:1318`) rejects empty, `.`, `..`, any embedded `/`, and any NUL as EINVAL, and is called from every op that takes a name. The share root is pinned by an fd opened `O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` and every access is an `*at()` syscall with `O_NOFOLLOW`, so host symlinks are never traversed. LOOKUP admits only regular files, directories and symlinks; device nodes, FIFOs and sockets are refused with ENOTSUP.

`ro` returns EROFS from CREATE, MKNOD, MKDIR, UNLINK, RMDIR, RENAME, RENAME2, SYMLINK, LINK, SETATTR, WRITE, and any OPEN whose access mode is not `O_RDONLY`. OPEN also refuses `O_TRUNC` even alongside `O_RDONLY`, because POSIX leaves that combination undefined and illumos truncates. That is checked twice and pinned by a test that asserts the host file is unchanged.

**A writable share grants the guest the VMM's own credentials.** Every file operation runs as the VMM's euid, which is root in a bhyve zone, and there is no uid/gid squashing or idmapping anywhere in the module. `FUSE_SETATTR` passes guest-supplied values straight through: `fchown(fd, uid, gid)` with any uid and gid, and `fchmod(fd, mode & 0o7777)`, where the mask keeps the setuid, setgid and sticky bits (`fs/passthrough.rs:853,871`). A guest can therefore create a setuid-root file inside the shared directory, and any host user who can reach that directory can run it. Export read-write only to a guest you would trust with root on that subtree, and prefer `ro` otherwise.

`FUSE_ACCESS` ignores its `mask` argument and grants access whenever the node can be stat'd (`passthrough.rs:421`), so on a read-write export the guest's `access()` answers are not authoritative.

## Display and input (rshyve only)

### fbuf and VNC

```
-s <slot>[:<func>],fbuf[,unix=<path>][,password=<pw>][,w=<n>][,h=<n>]
```

Those four keys are the whole parser (`bin/rshyve/src/main.rs:1163`). Defaults: socket `/var/run/rshyve/<vm_name>/vnc.sock`, no password, 1024x768. `w=abc` silently gives 1024. Every other key is ignored, **including `vga=`**: there is no VGA emulation to turn on, and `vga=on` does nothing. bhyve's `tcp=`, `rfb=` and `wait` are also ignored rather than rejected, so `-s 30:0,fbuf,tcp=0.0.0.0:5900` starts a VNC server on the default Unix socket with no diagnostic.

PCI identity 0xFB5D:0x40FB matches C bhyve's `pci_fbuf`, so the OVMF bhyve GOP driver binds. BAR1 is a fixed 16 MiB XRGB8888 framebuffer backed by a devmem segment mapped into the guest, not a trapping MMIO region. Each fbuf consumes one VM memory segment; exhausting them fails attach.

The RFB server always speaks 3.8 framing, always sends XRGB8888, and always sends a single full-screen Raw rectangle. `SetPixelFormat` and `SetEncodings` are parsed, warned about, and ignored; the `incremental` flag is discarded. The frame loop sleeps 33 ms and parses at most one client message per pass, so input is capped near 30 events per second. One client at a time, strictly sequential.

Guest writes to BAR0 can change resolution (2-byte writes at 0x04 and 0x06, or one 4-byte write at 0x04) and are validated against the 16 MiB BAR, giving a 2048x2048 ceiling. The CLI `w=`/`h=` path skips that check; an oversized CLI resolution produces a vertically truncated picture rather than an error. A resolution change is never pushed to a connected client; there is no DesktopSize pseudo-encoding.

Authentication exists but is legacy RFB VncAuth. With no password the server offers only security type None and refuses a client that selects VncAuth; with a password it offers only VncAuth and refuses None, so there is no downgrade path. The password value is used verbatim (no base64) and the RFB DES scheme truncates it to 8 bytes. There is no transport encryption and no attempt rate limiting. The socket is 0600 in a 0700 directory with no peer-credential check, unlike the control socket.

Clipboard text is read off the wire and thrown away. `Framebuffer::shutdown()` exists and nothing calls it, so the VNC thread lives until process exit and the socket file is left on disk until the next bind unlinks it.

### xHCI tablet

```
-s <slot>[:<func>],xhci
```

**No options are parsed.** The config field is never read, so `,tablet`, `,keyboard` and nothing at all all produce the same single absolute-pointer tablet. The `,tablet` in the docs and in bhyve-shaped command lines is decorative.

Intel 0x8086:0x1E31, BAR0 8 KiB MMIO, INTx only (no MSI or MSI-X). `HCSPARAMS1` advertises MaxSlots 64, but only one device slot is implemented: a second Enable Slot returns `TRB_CC_NO_SLOTS_AVAILABLE`. Two ports (USB2 disconnected, USB3 with the tablet), one interrupter, EP0 control and EP1 IN only.

The tablet reports 3 buttons and 16-bit absolute X/Y. `bInterfaceSubClass` is 0, so there is no boot-protocol fallback for firmware that speaks only boot mouse. The HID descriptor declares a wheel field but `get_report` always emits 0, so VNC scroll does nothing.

### PS/2

A PS/2 controller with keyboard is created for **every** rshyve VM. There is no `-s` spec and no way to disable it (`bin/rshyve/src/input.rs` `setup_ps2`). Ports 0x60/0x64, IRQ 1 and 12. VNC key events reach the guest only through this path; there is no USB keyboard anywhere in the tree. Keysyms with no scan-code mapping are dropped with a USDT probe and no log line. Scan code set 3 is not implemented.

The PS/2 aux device parses and ACKs the standard mouse command set, so a guest driver binds successfully, but `PS2Mouse::movement()` is hardcoded to report zero motion and nothing ever injects host movement. A guest with PS/2 and no xhci has a mouse that never moves.

## vTPM, CPUID, Hyper-V

### vTPM (rshyve only)

```
rshyve --vtpm [--vtpm-state-dir <PATH>] <vm_name>
```

TPM 2.0 only, backed by vendored libtpms compiled into the process. Guest interface is a TCG PTP 2.0 CRB at 0xFED40000, **locality 0 only**, with a 3968-byte command buffer; a longer command is truncated. No interrupts (INT_ENABLED and INT_STS read 0), so the guest polls CTRL_START. `TPM2_Cancel` is recorded and ignored. `TPMLIB_Process` runs synchronously inside the vCPU's MMIO write handler, so an RSA keygen blocks that vCPU.

Discovery is complete: a TPM2 ACPI table (Start Method 7, CRB) plus a DSDT `Device(TPM)` with `_HID "MSFT0101"`, `_STA 0x0F` and a `_CRS` covering the control area (`crates/vmm-devices/src/acpi.rs:366`). The TPM2 table declares no event log.

NV state defaults to `/var/db/rshyve/<vm_name>/tpm` and survives a VMM restart. Writes are temp-file plus fsync plus rename. Two things to know: the state directory is created under the process umask with no explicit mode, unlike the control socket; and pointing `--vtpm-state-dir` at a wrong or empty directory is **not an error**, because a missing NV file makes libtpms manufacture a fresh TPM. A guest that used BitLocker will not find its keys and will not be told why.

One VM per process: libtpms is single-instance and a second `Tpm::new` fails.

### `--cpu-baseline` (both binaries)

Accepted values, case insensitive (`crates/vmm-core/src/cpuid.rs:33`): `host`/`native`, `avx2`/`no-avx512`, `sse42`/`sse4.2`/`westmere`. Default `host`, which is a true no-op that leaves native CPUID in place. An unknown value is a startup error in both binaries. There is no per-leaf or per-bit override and no CPU-model impersonation.

`avx2` clears the AVX-512 and AMX sets in leaf 7 and the matching leaf 0xD components. `sse42` additionally clears AVX, FMA, F16C, RDRAND, BMI1, AVX2 and BMI2. Both profiles clear AVX-VNNI, AVX-512_BF16 and AVX-IFMA in leaf 7 subleaf 1, so AVX-VNNI disappears even at the `avx2` baseline. `sse42` does **not** clear the leaf 1 SSE4.2 or POPCNT bits and does not touch leaf 0x80000001.

### Hyper-V enlightenments (rshyve only)

`--hyperv [--tsc-freq-hz <HZ>]`. Tier 1 only and all-or-nothing: `reset` and `frequencies` are hardcoded on and `reference_tsc` follows from a non-zero TSC frequency. There is no per-feature flag.

CPUID leaves 0x40000000-0x40000006 are published with vendor `Microsoft Hv`. Leaf 0x40000006 is advertised and returns all zeros. MSRs handled: GUEST_OS_ID, HYPERCALL, VP_INDEX, RESET, TIME_REF_COUNT, REFERENCE_TSC, CRASH_P0-P4 and CRASH_CTL (writing NOTIFY logs the bugcheck parameters through slog).

The hypercall page is a deliberate stub: `mov rax, 2; ret`, which is `HV_STATUS_INVALID_HYPERCALL_CODE`. It satisfies the Windows boot probe and performs no hypercall. SynIC, synthetic timers, virtual APIC, TLB-flush hypercalls and IPI hypercalls are absent.

If the host TSC frequency probe fails and `--tsc-freq-hz` is not given, rshyve warns and disables reference-TSC entirely, at which point both timing MSRs inject #GP.

## PCI passthrough (rshyve only)

`-s <slot>,passthru,/dev/pptN` binds a real PCI function to the VM
(`crates/vmm-passthru/src/lib.rs`, attached at `bin/rshyve/src/main.rs:1412`).
The device needs the illumos PPT driver. Pass `-S` yourself: nothing
inspects the `-s` specs and adds it for you.

**Intel VT-d only. There is no AMD IOMMU backend.** `iommu_init()` opens
`misc/vmm_vtd` on an Intel host and `misc/vmm_amdvi` on an AMD one, and
only the first exists: the illumos tree ships a `vmm_vtd` module
directory and `VMM_VTD_OBJS = vtd.o` in `Makefile.files`, with no AMD
counterpart in either place. `amdvi_hw.c` sits in the source tree
unbuilt, next to the comment "Use the expected name for if/when this is
ported". On an AMD host `ddi_modopen` fails, `iommu_init` returns that
error, and passthrough does not work. Both `docs/devices.md` and
`docs/status.md` claimed AMD support until this was checked.

**A device that advertises MSI-X is refused at attach**, with
`ErrorKind::Unsupported` and the text "device advertises MSI-X, which
this VMM cannot isolate" (`check_passthrough_supported`). The reason is
not tidiness. `update_bar_mappings` maps a whole BAR into the guest with
`VM_MAP_PPTDEV_MMIO`, and on an MSI-X device that BAR holds the physical
MSI-X table; the illumos kernel checks only that the range is inside a
BAR (`ppt_map_mmio`), and carves nothing out. illumos VT-d remaps DMA
and not interrupts: there is no IRTA or IRTE code anywhere in `vtd.c`.
So a guest that writes that table makes the device post a message to a
host physical address of its choice, which is an interrupt at a vector
of its choice on the host APIC. Linux gates exactly this behind
`allow_unsafe_interrupts`; cloud-hypervisor carves the table and PBA out
of the mapped region. This VMM refuses the device. Most modern PCIe
devices advertise MSI-X, so this rules most of them out. MSI and INTx
devices still attach.

The userspace MSI-X table emulation that used to sit in
`crates/vmm-passthru/` has been deleted, not left in place. It could
never run: nothing
registered the device on the MMIO bus, so `bar_rw` was never called,
`VM_PPTDEV_MSIX` was never issued, and the kernel never installed a host
interrupt handler. Finishing it needs the carve-out, an MMIO-bus
registration, and a host with an assigned device to test on.

**Config-space writes are deny by default.** `classify_cfg_write` admits
the command register, the six BARs, the interrupt line byte and the MSI
capability, and drops everything else with a `warn` line. The default
matters because the illumos PPT driver adds no filter: `PPT_CFG_WRITE`
is a bare `pci_config_put32(cfg, cio.pci_off, cio.pci_data)`. Before
this, the catch-all arm forwarded any offset verbatim, so a guest could
set PCIe DEVCTL Initiate FLR and reset a device the host still owned,
write PMCSR and move it to D3, or point the Expansion ROM BAR at host
physical memory. The MSI capability window is now sized from its own
Message Control rather than a fixed 24 bytes, so it can no longer be
used to write the capability that follows it. Guest MSI Enable is
stripped from the write-through, because `ppt_setup_msi` has the DDI
framework put a host address and a host vector in those registers; the
guest's address and data are used for guest-side injection only.
Multiple Message Enable is clamped to the kernel's `msi_limit`.

**An I/O BAR decodes nothing.** `bar_rw` handles PIO, but nothing
registers this device on the PIO bus, unlike `crates/vmm-virtio/src/pci.rs`
which registers its own BAR handlers. MMIO BARs do not need it, because
EPT maps them straight to hardware. An I/O BAR does, so its ports reach
neither hardware nor emulation.

`PciPassthru::new` takes no logger and `bin/rshyve/src/main.rs:1420`
calls it, so dropped config writes and failed unmaps are currently
recorded nowhere. `PciPassthru::new_with_logger` exists for the caller to
switch to.

## Migration and the control socket (rshyve only)

### Control socket

`--control-socket <PATH>`, newline-delimited JSON, one response line per request. Twenty-one commands (`bin/rshyve/src/control/protocol.rs:12`): `status`, `pause`, `resume`, `shutdown`, `reset`, `stop`, `migrate-source`, `migrate-dest`, `migrate-status`, `migrate-config`, `metrics`, `metrics-prometheus`, and the nine hotplug commands `device-list`, `device-add`, `device-remove`, `cpu-list`, `cpu-add`, `cpu-remove`, `mem-list`, `mem-add`, `mem-remove`. Anything else returns `{"success":false,"error":"invalid command"}`.

Authorization is uid plus zone, not a capability: the peer's uid must equal the VMM's euid, and on illumos its zone must match ours or be the global zone (zone 0 is allowed deliberately so a global-zone migration agent can drive an in-zone VMM). There is no token and no per-command authorization, so any peer that passes that check can issue `migrate-source`, `stop` or `reset`. The socket itself is 0600 in a 0700 directory, re-stat'd after bind and unlinked if the mode or owner is wrong.

`pause` and `resume` are state CAS operations with a 5 second device quiesce budget; on timeout the devices are resumed, state rolls back to Running, and the error names the stuck device types. `shutdown`, `reset` and `stop` map to `VM_SUSPEND_POWEROFF`, `VM_SUSPEND_RESET` and `VM_SUSPEND_HALT` and do **not** halt devices first.

`metrics-prometheus` emits a multi-line body on a protocol otherwise defined as one line per request, so a strict line-oriented client will mis-frame it.

### Live migration

One driver, over one transport trait. `crates/vmm-migrate/src/wire.rs` defines `Transport`, a `Read + Write` stream whose read deadline the protocol shortens once the guest is paused, and `source.rs` and `destination.rs` are generic over it. Two streams implement it: `UnixStream`, the only kind that works inside a bhyve-brand zone, where there is no `/dev/poll`, and `TcpStream` for a global-zone or lab run. Neither end starts an async runtime. The framing is `tungstenite` WebSocket binary frames carrying RON.

The same driver serves both, so both carry the same state. An earlier tree had a second implementation for TCP that shipped no device state at all; it is gone.

The sequence is: full RAM pass, dirty convergence passes back to back until one takes 50 ms or less (bounded at 30 passes or 30 s), pause vCPUs, pause and drain devices, durable flush of every device, an optional ZFS barrier, a final dirty pass with the device export inside it and one more round for pages dirtied since, then time data, device state and the exported queue state. If anything after pause fails, devices and vCPUs are resumed so the guest survives.

Kernel state transferred: the ioapic, atpit, atpic, hpet, pm_timer and rtc system devices; per vCPU the 33 registers, 10 segment descriptors, MSRs, LAPIC, VMM_ARCH, run state and SIPI vector; and VMM time with boot_hrtime re-basing and TSC rescaling.

Guarded refusals: a vTPM or a bootrom variable store blocks `migrate-source`, and so does any device whose state nothing carries, which is `ahci-cd`, `virtio-console`, `virtio-fs`, `virtio-vsock` and `xhci` (`UNMIGRATABLE`, `bin/rshyve/src/control/migrate.rs:152`). `--migrate-listen` combined with `--vtpm`, a varstore, or an `fbuf`/`ahci-cd` spec is a startup error. The remaining asymmetry is covered at the preamble rather than at startup: the two ends exchange their whole device set and a migration whose sets differ is refused before a page is accepted, so a destination holding a device the source has not cannot start the guest.

Orchestration is not in this repository. Replicating the disk, sequencing the two hosts and bridging the transport belong to whatever drives the migration; this tree provides the VMM side, the control-socket verbs and the barrier contract (see [migration.md](migration.md#the-zfs-barrier)). `tools/vmm-migrate.sh` drives one end to end for lab use and is a test harness, not a supported component.

## Hotplug (both binaries)

Off unless `--hotplug` is given (`crates/vmm-config/src/lib.rs:215`, read once at `crates/vmm-machine/src/firmware.rs:52`). Without that flag no hotplug I/O port is claimed and no hotplug AML is emitted, so a VM that does not ask for hotplug keeps the ACPI tables and the free ports it had before this feature existed.

With `--hotplug`, four PIO blocks are claimed (`crates/vmm-machine/src/pm.rs:139,166`):

| Block | Port | Length | Condition |
|---|---|---|---|
| GPE0, status and enable | `0xAFE0` | 4 | always |
| PCI slots: `PCIU`, `PCID`, `B0EJ` | `0xAE00` | 12 | always |
| CPU slots | `0xAF00` | 12 | only when `maxcpus` is above the boot CPU count |
| Memory slots | `0x0A00` | 0x18 | always |

Sources: `crates/vmm-devices/src/acpi_gpe.rs:37`, `crates/vmm-devices/src/hotplug/pci.rs:37`, `cpu.rs:29`, `mem.rs:32`. GPE bit 1 runs `_E01` (PCI scan), bit 2 runs `_E02` (CPU scan) and bit 3 runs `_E03` (memory scan), emitted at `crates/vmm-devices/src/acpi/hotplug/mod.rs:28`.

Both binaries drive the same engines (`crates/vmm-machine/src/hotplug.rs` and its `cpu`/`mem` submodules). They differ only in how a request arrives: rshyve takes JSON on `--control-socket`, firehyve takes the vsock `CONTROL` verb, because firehyve rejects `--control-socket` at startup. A firehyve VM started with `--hotplug` but no `virtio-vsock` device therefore has the engines and no way to reach them; the startup log says `vsock CONTROL enabled` only when a device carries the slot (`bin/firehyve/src/main.rs:343`).

### PCI and disk hot-add

`device-add <slot,driver[,config]>` takes the same spec grammar `-s` takes and runs it through the same catalog closure the boot-time pass uses (`bin/rshyve/src/main.rs:513`, `bin/firehyve/src/devices.rs:188`). A control peer can therefore name no driver and no path that a command line could not. firehyve additionally refuses `virtio-vsock` at run time, because the CONTROL slot a vsock device carries is armed once at startup (`devices.rs:196`).

Bus 0, function 0, slots 2 through 31 only (`crates/vmm-devices/src/hotplug/pci.rs:68`). Slot 0 is the host bridge and slot 1 is the LPC bridge; neither gets an `_EJ0` method, so neither can be plugged. The slot is checked free against both the registry and the live bus before anything is built, so a refused request opens no file and takes no MSI-X vector. The id handed back is `driver@slot`, and it is what `device-remove` takes. `hostbridge` and `lpc` build no PCI device and are refused; in rshyve an unrecognised driver name also builds nothing and is refused here, rather than being logged and skipped the way the boot-time pass does it.

The drivers a binary can hot-add are the ones its own `-s` catalog builds, minus the names that build no PCI device. Whether a given driver behaves well when it arrives after boot rather than at boot has been checked on the lab node for `virtio-blk` only; the rest are reachable by construction, not by test.

### PCI hot-remove

`device-remove <id>` is a REQUEST, not an order (`crates/vmm-machine/src/hotplug.rs:546`). It marks the slot `remove-pending`, raises the eject event, and returns. Nothing is torn down and no backing file is closed. The device leaves only when the guest runs `_EJ0`, which a drain thread picks up every 100 ms (`hotplug.rs:292`).

A guest that never runs `_EJ0`, because it has no `acpiphp` driver or because the device is busy, leaves the slot in `remove-pending` for the life of the VM with no host state changed. That is the intended outcome, and `device-list` reports the state so an operator can see it. Three further refusals keep a device rather than forcing it out:

- an eject the guest ran without an operator request is refused, and the slot is offered back to the guest.
- a device that does not quiesce inside 5 seconds keeps its slot and is resumed.
- `passthru` returns no `Lifecycle` (`bin/rshyve/src/main.rs:1429`), so nothing can report that its DMA has stopped. It can be hot-added and can never be removed.

A hot-added device stops the VM being a migration source. `migrate-config` does append the device's `-s` spec to the destination command line (`bin/rshyve/src/control/migrate.rs:535`), but that is a report only: `migrate-source` refuses the VM first, because device state on the wire is matched by position. See [What cannot be done](#what-cannot-be-done-and-why).

### CPU hot-add

Needs `--hotplug` and `-c cpus=N,maxcpus=M` with `M` above `N`. When the two are equal the DSDT describes no CPU slot and the register file claims no port, on the reasoning that a controller for an event the VMM cannot raise is dead weight in every DSDT (`crates/vmm-machine/src/pm.rs:94`, `crates/vmm-devices/src/acpi/hotplug/cpu.rs:57`). `maxcpus` below the boot count, or above the kernel's 64, is a startup error (`crates/vmm-config/src/lib.rs:289`).

`cpu-add <id>` activates the vCPU in the kernel, starts its thread, and raises the CPU GPE. It reports that the CPU is present and the guest has been told; onlining it is the guest's own step. Activation is deliberately lazy: a vCPU the kernel has activated but the guest never onlines never joins `halted_cpus`, so pre-activating spare CPUs would stop `VM_SUSPEND_HALT` firing for the life of the VM.

An add that reaches `VM_ACTIVATE_CPU` and then fails consumes that id for the life of the VM. There is no `vm_deactivate_cpu` to undo the activation, so the id is recorded and refused rather than retried into a second active CPU with no thread. `cpu-list` reports such an id as `consumed`.

### Memory hot-add

Needs `--hotplug`, `-S`, and `-o hotplug.maxmem=SIZE`. All three are checked before the VM is created, so a window an operator cannot use costs no boot (`crates/vmm-config/src/lib.rs:374`). `-S` is required because a hot-add runs `VM_ALLOC_MEMSEG` on a live VM under the kernel write lock, which freezes every vCPU until it returns: with reservoir memory that is microseconds at any size, and without it the same call creates and zeroes every page inside the lock, measured at about 400 ms per GiB.

`-o hotplug.memslot=SIZE` sets the slot size, default 128 MiB, which is the Linux x86-64 memory block size. There are 8 slots (`crates/vmm-devices/src/hotplug/mem.rs:44`), the slot size must divide `maxmem`, and a window needing more than 8 slots is refused rather than clamped. The window sits above every RAM region the VM booted with and so always above 4 GiB, which keeps it out of the 32-bit MMIO hole.

`mem-add <bytes>` rounds up to a whole slot and returns the slot it filled. A second, lower ceiling comes from the kernel: one VM gets five memory segments and boot spends some of them, so the number of adds can be below the slot count. A UEFI VM with a framebuffer and more than 3 GiB spends four and leaves one add; direct boot leaves three. The engine reports that as `NoSegment` rather than letting the kernel answer `EINVAL`.

### What cannot be done, and why

`cpu-remove` and `mem-remove` are recognised on both transports and always refused with the reason. illumos has no `vm_deactivate_cpu`: `active_cpus` is only ever set, and the one place that clears it is `vm_init` on `VM_REINIT`. It has no `VM_FREE_MEMSEG` either, and `vm_free_memseg` has only destroy-path callers, so a memory segment lives until the VM does. Both refusals are answers, not silent failures, because an operator will try the command and "unknown command" does not say why (`bin/rshyve/src/control/dispatch.rs:251`, `bin/firehyve/src/control.rs:296`).

A VM that hot-added anything cannot be a migration source. Two separate refusals, both in `migrate_source` under the topology lock:

- **A CPU or memory add** (`bin/rshyve/src/control/migrate.rs:84`). The destination is started from the same command line, so it would get the boot CPU count and the boot memory size; a running vCPU would have no thread there, and memory the guest is using would have nowhere to land.
- **A PCI add, or a slot still pending removal** (`migrate.rs:64`). Device state on the wire is matched by position: the source tags every queue group `pci_slot: 0`, the preamble carries no device list, and the destination claims the first group whose queue count matches its own. While the live set equals the boot `-s` list both sides build the same devices in the same order, so the match is accidental but correct; a hot-add breaks it and the guest would come up on the far side holding another device's ring state.

The second refusal is the newer of the two and is what makes the `migrate-config` device report above a report only.

### Evidence, and its limits

Everything above is traced to code. The guest-side half has narrower evidence:

- **Lab observed, one guest.** On the firehyve test node, a Linux 6.12.13 guest built with `kernel/hotplug.config` boots reporting `Allowing 2 present CPUs plus 6 hotplug CPUs` and `ACPI: Enabled 3 GPEs in block 00 to 0F`, registers `acpiphp` slots 2 to 31, and logs no ACPI error. `cpu-add`, `mem-add` and `device-add` each succeed on it, and `mem-add` moves the host reservoir's free and allocated counters by exactly the size added.
- **Not covered.** The AML is not validated by `iasl`, in CI or anywhere else. No CI job boots a guest at all, so the guest-side result is a single lab run on one kernel configuration. Windows, illumos and BSD guests have not been tried. Hot-remove has been exercised only as far as the refusals above; no measurement of a guest completing an `_EJ0` on a busy disk is recorded.

## Metadata agent (rshyve only)

Not a network service. A host thread drives the COM2 UART; the guest talks to it over `/dev/ttyS1` on Linux or `/dev/term/b` on SmartOS. It starts only when no `-l com2,<backend>` is present.

Store contents (`bin/rshyve/src/mdata.rs` `build_store_from_zone_config`): `sdc:hostname` from the positional VM name, `sdc:uuid` from `-U` falling back to the hostname, `sdc:nics` from `--mdata-nics`, `sdc:resolvers` from `--mdata-resolvers` (default `8.8.8.8`, `1.1.1.1`), `root_authorized_keys` from `--mdata-ssh-keys`, `root_pw` from `--mdata-root-pw`, plus fixed empty entries for `user-script`, `user-data`, `sdc:vendor-data`, `sdc:operator-script`, `sdc:routes` and `sdc:dns_domain`.

Protocol: V2 NEGOTIATE, GET and KEYS work; PUT and DELETE return FAILURE because the store is read-only. V1 GET and KEYS work as a fallback. The V2 frame parser **ignores the declared body length and the request CRC32**; it computes a CRC only for responses.

In production the agent is expected to be unused, because `-l com2,/.zonecontrol/metadata.sock` routes COM2 to the external global-zone agent instead.

## fhrun

`fhrun <manifest.json>` boots a microVM around a Linux binary and waits for it. Flags: `--check`, `--print-argv`, `--emit-initramfs <out.cpio>`.

Manifest keys as parsed (`tools/fhrun/src/manifest.rs`). Required: `name` (1..=127 chars, no NUL), `bin`, `kernel`, `init` (all three must be existing files). Optional: `args` (default `[]`), `env` (default `{}`), `workdir` (default `/`), `vcpus` (default 1), `memory` (default `128M`, never parsed by fhrun), `extra_files`, `net`, `nics`, `consoles`, `guest_metadata`, `vmm` (default the bare name `firehyve`, resolved through PATH; accepts the legacy key `firehyve`), `kernel_extra_cmdline` (append only).

NIC keys: `vnic` (required), `mac` (required), `ip` (CIDR, required), `gateway`, `role`. Validation is shape only. `mac` is checked for six colon-separated fields and is then **never used**: it is not in the VMM argv and the guest discards it, because viona programs the address on the host. `ip` is checked only for containing `/`.

Console keys: `socket_path` (default `<runtime-dir>/console<N>.sock`), `guest_device` (default `/dev/hvc<N>`, a declaration only), `role`.

Emitted argv is fixed: `-c -m --kernel --initrd --cmdline --no-track-dirty -l com1,stdio`, then one `-s <7+i>,virtio-net-viona,<vnic>` per NIC and one `-s <15+i>,virtio-console,<socket>` per console, then the VM name. There is no manifest field that requests a disk, a virtio-fs share, or an RNG, even though firehyve implements all three.

The kernel command line is fhrun-owned and cannot be edited, only appended to: `console=ttyS0 earlyprintk=ttyS0 root=/dev/ram0 init=/init rdinit=/init panic=-1 fhrun=1 tsc=reliable`.

`fhrun-init` is a separate Cargo workspace excluded from the root, so `cargo build --workspace` never builds it. It must be cross-compiled to `x86_64-unknown-linux-musl` and named in the manifest's `init` field. It mounts devtmpfs, `/proc` and `/sys`, configures IPv4 statically by ioctl, redirects stdio to `/dev/console`, and `execve`s the payload. It does not fork, so the payload's exit status can never be observed or propagated.

## Limits

| Area | Number |
|---|---|
| PCI address range | dev 0..=31, func 0..=7; bus parsed 0..=255 but **not honored** (flat 32x8 array in `pci/bus.rs`) |
| Reserved slots | 0 (host bridge) and 1 (LPC bridge); a real driver there panics on the duplicate-BDF assert |
| Kernel command line | strictly under 4096 bytes, at GPA 0x20000; the kernel's own `cmdline_size` is never consulted |
| initrd placement | below `min(mem_size, 0xC000_0000)`, so a 3 GiB ceiling regardless of VM RAM |
| E820 handoff | PVH hard-errors above 64 entries; bzImage silently truncates at 128 |
| Direct-boot page tables | identity map the first 4 GiB with 2 MiB pages |
| Bootrom file | 4 KiB to 16 MiB, multiple of 4096; ROM plus varstore must also fit 16 MiB; 4 MiB of SEC RAM is consumed at 0xFCC00000 |
| Variable store file | 4 KiB to 16 MiB, page aligned, regular file, exclusively `fcntl`-locked |
| virtio-blk | ring 128; `sectorsize` power of two 512..=65536; `num-queues` 1..=8; 8 workers for one queue, 2 per queue for MQ |
| NVMe | 1 namespace, 512-byte LBA, 15 I/O queues of up to 4096, MDTS 9 (2 MiB), 8 workers |
| ahci-cd | 1 port, 1 ISO, PRDTL 512, 1 MiB max transfer, 2048-byte blocks |
| viona | 2 queues of 256, 3 MSI-X vectors |
| virtio-console | 2 queues of 128, 64 KiB host-input backlog, 250 ms client write timeout, 1 client |
| virtio-fs | `queue-size` 8..=1024 rounded up to a power of two (default 128), tag 36 bytes, max_write 512 KiB, max_background 64, 1 MiB per request body |
| fbuf | BAR1 fixed at 16 MiB, so 2048x2048 max; 33 ms frame tick capping input near 30 events/s; VNC password truncated to 8 bytes; 1 client |
| xHCI | MaxSlots 64 advertised, 1 implemented; BAR0 8 KiB; INTx only |
| vTPM | locality 0 only, 3968-byte command buffer, one VM per process |
| Control socket | 8 concurrent connections, 64 KiB per request line, 30 s read, 10 s write |
| PCI hotplug | slots 2..=31 on bus 0 function 0; 5 s quiesce budget per eject; the eject drain runs every 100 ms |
| CPU hotplug | `maxcpus` at most 64, the kernel's `VM_MAXCPU`; ids are add-only and a failed add spends one |
| Memory hotplug | 8 slots, default 128 MiB each, window above 4 GiB; 5 kernel memory segments per VM, some spent by boot |
| Migration | 64 pages per batch (cap 256), 8 MiB max serialized payload, 5 convergence iterations at 500 ms with a 1024-page threshold, 5 s pause quiesce, 30 s queue-export budget, 120 s ZFS barrier |
| mdata | 4096-byte input lines, printable ASCII only, 256-byte UART FIFOs |
| fhrun | 4 NICs (PCI slots 7..=10), 2 consoles (slots 15..=16) |
| Host | one bhyve VM instance per non-global zone; a second `VM_CREATE` returns EINVAL |
| Host tunable | STREAMS `strmsgsz` at 8192 or above, or 0 for unlimited; the default is 65536. See [Known caveats](#known-caveats) |

## Not supported

Absent, with no partial path:

- **`ahci-hd`.** rshyve fails at startup with "AHCI device type 'ahci-hd' is not implemented; use virtio-blk or nvme" (`bin/rshyve/src/main.rs:974`). There is no AHCI hard-disk code in `crates/vmm-storage`; the module is ATAPI only.
- **Any emulated NIC other than viona-backed virtio-net.** No e1000, rtl8139, vmxnet, tap, netgraph or slirp.
- **virtio-scsi, virtio-9p, virtio-balloon, virtio-input, virtio-gpu.**
- **PCI passthrough on an AMD host**, and passthrough of any device that advertises MSI-X. Neither is a gap in this VMM alone: illumos builds no `vmm_amdvi` module, and its VT-d driver does no interrupt remapping. See [PCI passthrough](#pci-passthrough-rshyve-only).
- **Interrupt remapping for a passed-through device**, and therefore any equivalent of the Linux `allow_unsafe_interrupts` override. There is no knob to turn the MSI-X refusal off.
- **Hot-remove of a CPU or of memory.** Both are impossible at the illumos kernel boundary, not merely unimplemented: there is no `vm_deactivate_cpu` and no `VM_FREE_MEMSEG`. Both commands exist and are refused with that reason. See [Hotplug](#hotplug-both-binaries). Hot-ADD of a PCI device, a CPU or memory is supported behind `--hotplug`, and PCI hot-remove works when the guest runs `_EJ0`.
- **qcow2 or any non-raw image format.** Every backend does raw offset arithmetic on a plain file.
- **Block I/O throttling.**
- **VGA emulation, a VGA BIOS, and text mode.** No handlers exist for 0x3C0 or 0x3D4.
- **VNC over TCP**, and the bhyve `wait` option that holds boot for a client.
- **A USB keyboard.** VNC key events reach the guest only through PS/2, so a guest or firmware with no PS/2 keyboard driver has no keyboard.
- **PS/2 mouse motion.** The aux device answers commands and reports zero movement forever.
- **Clipboard integration** in either direction.
- **A boot-protocol flag.** The kernel file decides. No multiboot, no raw vmlinux without a PVH note, no real-mode entry, no bzImage protocol older than 2.12, no 32-bit-only bzImage.
- **Kernel relocation or KASLR placement**, initrd above 4 GiB, and multiple initrd modules.
- **UEFI in firehyve**, in any form: no bootrom, no variable store, no CSM.
- **Migration of the UEFI variable store, vTPM NV state, framebuffer state, xHCI state, AHCI state, Hyper-V state, virtio-fs state, virtio-console state, or LPC UART state.** The first three are hard refusals; the rest are simply not in the protocol and not on any reject list.
- **Migration of disk contents.** The protocol carries no block data. Disks move separately by ZFS send/recv, and only for ZFS zvols.
- **FPU/XSAVE state transfer.** `state.rs` sets `let fpu = Vec::new();`.
- **A migration cancel command**, migration IDs, expiry, and replay protection.
- **vTPM state export, TPM 1.2, localities 1-4, TPM interrupts, `TPM2_Cancel`, and a hypervisor-supplied TCG event log.**
- **Real Hyper-V hypercalls, SynIC, synthetic timers, and virtual APIC.**
- **Per-bit or per-leaf CPUID overrides.** Only the three `--cpu-baseline` profiles exist.
- **Any host OS but illumos, and any architecture but x86_64, at run time.** Both binaries type-check on Linux in CI; only illumos can link and run them.
- **A prebuilt platform image, a published release, or an installer.** Integration is manual.

### Accepted by the CLI and then ignored

Three C bhyve flags are refused instead of ignored, so an operator who puts one in `bhyve_extra_opts` gets an error rather than a VM that is not what was asked for: `-D` (destroy on power-off), `-k` and `--json-config` (`crates/vmm-config/src/lib.rs:10`). `boot.c` passes none of the three. The instance is reclaimed when the process exits whatever `-D` would have said, because the VMM arms the kernel autodestruct flag on every VM.

These parse without complaint and do nothing. Each one is a way to believe you configured something you did not.

| Input | Reality |
|---|---|
| `-o` / `--config-option`, any key outside `hotplug.` | Passed over. `-o` is a free key space this VMM shares with vmadm. The two keys it does read are `hotplug.maxmem` and `hotplug.memslot`; a misspelled `hotplug.` key is a startup error rather than a silent drop (`crates/vmm-config/src/lib.rs:338`). |
| `-A` / `--acpi` | Declared at `crates/vmm-config/src/lib.rs:91` and read nowhere. ACPI tables are built unconditionally, so the flag is redundant rather than harmful. |
| `nvme,...,sectorsize=N` | Parsed by the shared block parser, then discarded. The `nvme` arm always builds a 512-byte LBA. |
| `nvme,...,num-queues=N`, `nvme,...,nodelete` | Parsed and discarded. |
| `virtio-blk,...,nodelete` | Parsed, stored, never read (`block.rs:113`, `#[allow(dead_code)]`). |
| virtio-blk DISCARD / WRITE_ZEROES | Feature bits are advertised on every writable disk and the handlers return OK without touching the file. `fstrim` reports success and does nothing. The related config fields at offset 0x24 and beyond read zero. |
| `fbuf,...,vga=on` / `vga=off` / `vga=io` | Discarded. There is nothing to enable. |
| `fbuf,...,tcp=...`, `fbuf,...,wait` | Ignored, not rejected. VNC still binds the default Unix socket. |
| `xhci,tablet` / `xhci,keyboard` / anything | The config field is never read. |
| `passthru,<path>,<anything>` | Only the first field is used. |
| `virtio-rnd,<anything>` | The config is never read. |
| `-s 7,hostbridge`, `-s 31,lpc` | The slot number is ignored; the chipset places these at 0.0.0 and 0.1.0 regardless. |
| `-l com3,...`, any `-l` device that is not `bootrom`, `com1` or `com2` | Silently ignored, no diagnostic. |
| `-l bootrom,...` together with `--kernel` (rshyve) | The bootrom is never looked up. No warning. |
| `--cmdline` with UEFI boot (rshyve) | Only the direct-boot loader reads it. |
| A misspelled `-s` driver name (rshyve) | `warn`-level log and skip. The VM boots without the device. |

## Known caveats

**Live migration is experimental and its transport is unauthenticated and unencrypted.** Guest RAM, all vCPU register and MSR state, and device state cross the wire in cleartext with no peer identity, no session binding, no migration ID and no replay protection. The destination resumes whatever the first connecting peer sends. The preamble checks the CPU count, the memory size, the device set and the CPU features, and none of those is an identity check. `SECURITY.md` states this and `docs/security/open-findings.md` tracks it as OSF-1 (high). The only mitigation available today is external: migrate over a separately authenticated tunnel, ideally an SSH-forwarded Unix socket so the VMM never binds TCP.

**A writable virtio-fs share is a root-equivalent grant over that subtree.** File operations run as the VMM's euid with no uid/gid squashing, and the guest chooses the uid, gid and mode on SETATTR, setuid bit included. Treat a read-write export the way you would treat handing the guest a root shell scoped to that directory. `ro` shares carry none of this: every mutating opcode returns EROFS, including an `O_TRUNC` open.

**The destination refuses a CPU feature gap by default.** The comparison covers leaf 1 ECX and EDX, leaf 7 EBX, ECX and EDX, and the XCR0 component mask, and both ends report what their own `--cpu-baseline` leaves a guest, which is what the guest saw and what it will see (`crates/vmm-migrate/src/cpu.rs`). A destination missing any bit the source exposes fails the preamble before a page is taken. The `migrate-dest` control command takes an `allow_cpu_feature_mismatch` field that downgrades the refusal to a logged warning for that one migration; the guest then takes #UD on the first instruction this host lacks. A destination started with `--migrate-listen` never sets the field, so that path always refuses. The refusal text names a `--allow-cpu-feature-mismatch` command-line flag that does not exist.

**Device state is carried by PCI address.** Each payload entry names the bus/device/function its device had on the source, and the destination restores by that address alone, for virtio transports and NVMe controllers alike (`crates/vmm-migrate/src/codec.rs`, `crates/vmm-virtio/src/pci/migrate.rs`). An entry for an address this VM has no device at, two entries for one address, or a local device with no entry each fail the migration before the guest runs, and the preamble compares the two device sets by kind and address first. Two virtio-blk disks with the same queue count no longer restore into each other. The export is a plain read taken after the pause, so a device that fails to export fails the migration instead of substituting empty state.

**The vTPM ships two unpatched libtpms CVEs.** The vendored tree is libtpms at revision `521c5107` and carries CVE-2026-6726 (CVSS 8.5, `ObjectFlush()` leaves key material in a freed object slot) and CVE-2026-6727 (CVSS 8.3, `OaepDecode()` retains early-exit comparisons reachable from a guest-issued `TPM2_RSA_Decrypt`). Both are guest-to-host through the vTPM command interface, cross-referenced to CERT VU#431093, and recorded in `THIRD_PARTY.md:148-159` and OSF-4. `THIRD_PARTY.md` states both must be resolved before the vTPM is presented as production-ready.

**A vTPM state directory and a UEFI `VARS.fd` are one unit of VM state.** Losing either makes the other useless. Nothing in the code enforces this; snapshots and backups must capture both together (`docs/testing.md:686`).

**Hyper-V enlightenments have never been validated against an installed Windows workload** (`FEATURES_TODO.md:290`). Migration does carry the enlightenment: the source drops both overlay pages before the final RAM pass and exports GUEST_OS_ID, HYPERCALL, REFERENCE_TSC, the five crash parameters and the reference-time counter; the destination replays those MSR writes, which reinstalls the overlays, and refuses a payload carrying an enlightenment for a VM started without `--hyperv` (`crates/vmm-hyperv/src/lib.rs`, `crates/vmm-migrate/src/destination.rs`). That path has unit tests and no Windows guest behind it.

**`-S` does not mean what its help text says.** It is documented as "Wire guest memory (don't swap)", but guest memory is mapped `VM_MEMMAP_F_WIRED` unconditionally. What `-S` actually does is set `VCF_RESERVOIR_MEM` at VM create, which means the host VMM reservoir must already be sized out of band. With an empty reservoir, VM create with `-S` fails.

**Both binaries destroy a same-named VM instance on startup.** `force: true` is hardcoded, not a flag, and `VmmHdl::create` retries 20 times at 100 ms after a best-effort destroy. Do not reuse the name of a live VM.

**No minimum bhyve API version is enforced.** The version is queried and logged, then execution continues. What the code actually depends on is v5 (CPUID control), v8 (the `VCF_TRACK_DIRTY` create flag), v11, v12 and v13 (the vmm-data reads and writes a migration makes, and the RTC at data version 2), and v16 for the suspend detail that names the vCPU which took a triple fault. The one explicit check, `set_dirty_pages` refusing below v17, guards a function nothing calls. On an older kernel the binaries start and then fail at an ioctl or give less without saying so. v18 is the only version with test evidence behind it. The viona interface version is never checked at all, so a mismatched module surfaces as a raw ioctl errno.

**A vmm_drv lease that does not break keeps the process alive, and no watchdog can end it.** Teardown ends in `VM_DESTROY_SELF`. That ioctl reaches `vmm_lease_block`, which asks every vmm_drv lessee to give its lease up and then waits in an untimed `cv_wait` until the lease list is empty (`uts/intel/io/vmm/vmm_sol_dev.c`). The wait ends only when every lessee gives its lease up, and viona gives its up asynchronously: the expire callback returns `B_FALSE` and only signals the ring worker, which breaks the lease the next time it checks (`uts/intel/io/viona/viona_ring.c`). A worker that does not reach that check holds the lease, and the lease holds the destroy. `viona_tx_wait_outstanding` is one such point, an untimed wait on TX frames the MAC layer still holds; it is reachable only on a link with guest-memory loaning on, which is not the shipped configuration, because illumos defaults `viona_default_tx_copy` to true and this VMM never sets the parameter. The VMM bounds what it can: the destroy runs on its own thread with a 15 s budget, teardown logs `the VM destroy did not return inside its budget` and stops waiting, and the shutdown watchdog then calls `_exit(4)`. The process still stays. A thread in `cv_wait` does not carry `T_WAKEABLE`, so `pokelwps` cannot make it run and `exitlwps` waits for it (`uts/common/os/lwp.c`). No signal, `close` or `_exit` reaches it, `SIGKILL` included. The instance and its guest memory stay held until the lease breaks: release the downstream holder and teardown finishes on its own, otherwise only a node reboot frees the memory. Closing this needs a kernel change; see [FEATURES_TODO.md](../FEATURES_TODO.md#upstream-bound-the-vmm_drv-lease-wait) and the lab procedure in [docs/testing.md](testing.md#wedged-lease-teardown).

**The bounded socket writes need `strmsgsz` at 8192 or above.** virtio-console writes to its client with a 250 ms budget; the vsock host socket and its `CONTROL` reply use 500 ms. Each of those writes runs under a lock or on a vCPU thread, so each must end, and illumos offers no flag that ends one. `setsockopt(SO_SNDTIMEO)` on an AF_UNIX stream reports success and is then never read: the send reaches `strwrite`, which waits in `strwaitq` with a timeout of -1 (`fs/sockfs/socktpi.c`, `os/streamio.c`). `MSG_DONTWAIT` becomes `FNONBLOCK` on the receive path only. The size of the send is the only control left, so `write_all_bounded` waits for `POLLOUT` and then sends at most 8 KiB (`crates/vmm-core/src/unixsock.rs`). That works because the stream head cuts a write into messages of `sd_qn_maxpsz`: `tl` advertises `INFPSZ` and the stream head caps it at `strmsgsz`, 65536 by default (`conf/param.c`). A send at or below that cap is one message and does not wait. A host tuned below 8192 splits the send, and the wait between two messages has no timeout and, after the first message, no signal escape either. Keep `strmsgsz` at its default, or at any value of 8192 or more; 0 means unlimited and is safe as well. This is part of the supported host configuration, recorded in [Platform Integration](platform.md#host-tunables). The margin is measured, not assumed: a lab node reads 65536 against the 8 KiB chunk, 8x ([hardware-run.md](hardware-run.md#the-socket-bound-is-measured-not-assumed)). Do not read a macOS or Linux test pass as evidence here. macOS honours `SO_SNDTIMEO` on an AF_UNIX stream and illumos does not, so the portable suite never reaches this path.

**A device reset has no bounded duration on illumos, and no userspace change gives it one.** A reset ends by waiting out the interrupt injections already in flight (`IntrGate::settle`, `crates/vmm-virtio/src/pci/intr.rs:266`), and both injection ioctls take the bhyve VM read lock at dispatch (`uts/intel/io/vmm/vmm_sol_dev.c:503,506`). illumos `rw_enter(RW_READER)` yields to a waiting writer (`uts/common/os/rwlock.c:64-70`), so anything holding the VM write lock delays every reset on that VM: memory hot-add, `VM_PAUSE`, `VM_RESUME`, and VM-wide device-state export. `VM_ALLOC_MEMSEG` alone held the lock 69 ms for a 256 MiB add and scales at 233 to 270 ms per GiB, so a 16 GiB hot-add stalls a reset for seconds. This is an unbounded delay, not a hang: the reset completes when the lock is released. It matters more on illumos than elsewhere, because `virtio_legacy_device_reset_locked` and `virtio_modern_device_reset_locked` (`uts/common/io/virtio/`) are each a single status write with no read-back, so the guest is stopped inside one register write for the whole duration. `settle` cannot take a deadline, because admission tests the driver session before the injection ioctl and an injection already dispatched cannot be recalled. Measured numbers and the two things that would bound it are in [hardware-run.md](hardware-run.md#why-a-device-reset-has-no-upper-bound).

**Two test gates still hold coverage off the platform that runs the code.** `bin/rshyve/src/peercred.rs:130` gates its test to Linux and macOS, so the control socket's peer check is untested on illumos, and `crates/vmm-core/tests/autodestruct.rs` is illumos only, so CI never runs it. Nothing compares per-crate test counts across platforms, so a new gate is invisible until somebody reads for it. The viona half of this gap is closed: `VionaInner` now holds one `Arc<dyn LinkOps>` on every platform (`f896980`), which took viona tests executing on illumos from 3 to 39 with no platform gate left, and `dfd478a` moved `LinkOps` into `viona-api` beside the `VionaFd` that implements it, so the illumos adapter sends its ioctls directly. [hardware-run.md](hardware-run.md#coverage-that-does-not-reach-illumos) carries the detail.

**Dirty page tracking is on by default in rshyve.** The only control is the `--no-track-dirty` opt-out; `--track-dirty` is gone, because it could never be false. Tracking asks the kernel for `VCF_TRACK_DIRTY` at VM create. Nothing probes for hardware support first or degrades if the flag is refused.

**No part of the VMM constructs an async runtime any more.** TCP migration used to build a tokio runtime, which a bhyve-brand zone cannot run because it has no `/dev/poll`. Both transports are now the same blocking driver. An in-zone migration should still use the Unix socket with a global-zone agent bridging to the peer, because that keeps an unauthenticated protocol (OSF-1) off a listening TCP port; TCP from inside a zone is untested. `bin/rshyve/Cargo.toml` still declares tokio and no rshyve source file uses it. firehyve's crate graph contains no tokio at all.

**CI type-checks; it does not link or boot.** A green run proves formatting, portable-crate clippy and tests, illumos type-checking of the FFI crates, a current lockfile, and the dependency and public-surface policies. It does not prove the VMM builds, boots a guest, or passes a platform test. USDT probe emission is a no-op on the Linux runner and is verified only on the build host. The stock-platform-image shared-library gate (`tools/check-needed.sh`) runs on the build host, not in CI, so CI never sees a real illumos ELF. `tools/check-ci-crates.sh`, the guard that asserts CI covers every portable crate, runs in the `ci-coverage` job. Every runtime claim in `docs/testing.md` is lab-verified against an untracked host list, not reproducible publicly.

**The MSRV (1.94) is unverified for both shipping binaries.** The MSRV job's crate list omits `rshyve`, `firehyve`, `vmm_tpm` and `vmm_tpm_sys`. There is no `rust-toolchain.toml`, and `tools/fhrun-init` declares no `rust-version` at all.

**Build on a platform image no newer than the oldest node you deploy to.** `tools/check-needed.sh` pins `MAX_OPENSSL_SMARTOS=3.0.9`, because `ld.so.1` refuses a binary whose recorded `OPENSSL_SMARTOS_<ver>` the node's library does not define. The platform-OpenSSL link path is taken only when host and target match and `/lib/amd64/libcrypto-smartos.so.3` exists; a cross build silently falls back to pkg-config, which is the configuration that once produced a binary recording `NEEDED libcrypto.so.3` that no platform image ships.

**virtio-fs, virtio-console, virtio-vsock and xhci carry no migration state at all, so a VM holding one cannot be a migration source.** `migrate-source` names the device and refuses. In-flight FUSE requests, the console backlog and the attached client have no place on the wire, and refusing is what keeps the guest from finding the device at reset on the far side.

## Documentation that contradicts the code

Treat `STATUS.md` as historical. It still titles the project "vmmnew", predates the two-binary split, never mentions `firehyve` or `fhrun`, and maps fbuf, xHCI, NVMe and passthru to a monolithic `crates/vmm-devices/` that no longer holds them.

| Claim | Where | Reality |
|---|---|---|
| "`ahci-hd` falls back to virtio-blk" | `docs/devices.md:14` | Hard startup error, `bin/rshyve/src/main.rs:974`. A test asserts the message does **not** suggest virtio-blk. The most dangerous stale line in the docs. |
| "`ahci-cd` falls back to virtio-blk" | `docs/devices.md:13` | A real ATAPI AHCI controller, `crates/vmm-storage/src/ahci/`, attached at `main.rs:1231`. `docs/status.md:39` gets it right, so two shipped docs disagree. |
| "LPC bridge at slot 31" | `docs/devices.md:8` | BDF 0.1.0, slot **1**, `chipset/i440fx.rs:96`, agreeing with `acpi.rs:258`. Slot 1 is the unusable one, not 31. |
| "virtio-blk: single queue, 128 entries" | `docs/devices.md:60` | Multiqueue up to 8, default `min(num_vcpus,4)`. 128 is the ring size, not the queue count. |
| virtio-blk options are `nodelete` and `sectorsize` | `docs/devices.md:59` | The parser also honors `ro` and `num-queues=N`; `num-queues` appears nowhere in the docs. `nodelete` is inert. |
| "Automatically adds -S and -A when passthrough devices are present" | `docs/devices.md` | No such code. `cli.wire_memory` is read once and nothing inspects the `-s` specs. An operator relying on this runs passthrough over unwired memory. |
| One device table with no per-binary column | `docs/devices.md` | firehyve accepts 8 of 13 drivers and hard-errors on the rest. `docs/cli.md:117` states this correctly. |
| `--cpu-baseline` values are host, avx2, sse42 | `docs/cli.md:35` | `native`, `no-avx512`, `sse4.2` and `westmere` are also accepted, case insensitively. |
| Bootrom grammar is `-l bootrom,<path>|uefi|bios` | `docs/cli.md:74` | The varfile field (`-l bootrom,<rom>,<varfile>`) is omitted entirely, as is the `=`-containing legacy-option field. |
| Migration examples invoke a binary called `vmm` | `docs/migration.md` | The binary is `rshyve`. |
| `--migrate-listen 0.0.0.0:4567` as the destination | `docs/migration.md`, `docs/cli.md` | Works only because the CLI flag applies no validation; the `migrate-dest` control command runs the same address through a validator that rejects `0.0.0.0`. Two entry points to one feature disagree. |
| The documented control commands are the whole set | `docs/cli.md` | Twenty-one exist (`bin/rshyve/src/control/protocol.rs`). `shutdown`, `reset`, `stop`, `migrate-dest`, `migrate-config`, `metrics` and `metrics-prometheus` are documented nowhere, as is `migrate-source`'s `zfs_barrier` field. |
| `password=<b64>` for fbuf | `docs/platform.md:30` | The value is used verbatim; there is no base64 decode. `docs/devices.md:128` gets it right. |
| `-s 30:1,xhci,tablet` selects the device | `docs/platform.md:31`, `docs/devices.md:130` | The config field is never read. |
| "MSFT0101 DSDT device is still missing" | `FEATURES_TODO.md:10,30,117` | Landed. `crates/vmm-devices/src/acpi.rs:366` with five tests. |
| "Keyboard input does not exist and pointer events are not connected" | `FEATURES_TODO.md:27` | Both wired. PS/2 keyboard at `bin/rshyve/src/input.rs`, tablet pointer at `main.rs:1205`. The same line's claim that wide framebuffer stores are fatal is also stale; BAR1 is devmem-backed. |
| "xHCI advertises MaxSlots = 1" | `FEATURES_TODO.md:111` | `MAX_SLOTS = 64` at `crates/vmm-hid/src/xhci/bits.rs:32`, with a test. Only one slot is implemented, which is the real remaining limit. |
| virtio-fs listed as a future idea | `FEATURES_TODO.md:272` | Implemented and wired into both catalogs. |
| fhrun boot numbers reproducible from a manifest | `docs/firehyve-whitepaper.md:174` | The measured command line ("no `console=`", plus `no_timer_check noapictimer`) is unreachable: `launcher.rs:53` always emits `console=ttyS0` and `kernel_extra_cmdline` can only append. |
| fhrun exit codes are 0/1/2 | `docs/fhrun.md:75` | The VMM's code passes through, and firehyve exits 2 on a guest fault and 3 on a triple fault, so 2 is ambiguous and 3 is undocumented. |
| `guest_metadata` "reaches the payload untouched" | `docs/fhrun.md:102` | Nothing delivers it. The payload gets only argv and env; the metadata is readable only if the payload opens `/firehyve-spec.json` itself, which is mode 0644 inside the guest. |

Several source comments are stale in the same way and are worth knowing if you read the code: `bin/rshyve/src/bootrom.rs:50` describes 48 MiB of SEC RAM at 0xFC000000 while the code maps 4 MiB at 0xFCC00000; `crates/vmm-boot/src/direct.rs:43` keeps a `GPA_KERNEL` constant for a fixed-address placement path that does not exist; and `crates/vmm-virtio/src/lib.rs` still calls the transport "legacy" when it is transitional.