# vmmnew — Rust bhyve VMM for Triton/SmartOS

## What it is

A new Rust-based bhyve userspace VMM, replacing the existing C `bhyve` command. Designed for live migration as a first-class feature, with CLI compatibility for vmadm/boot.c integration. Architecture informed by Oxide's propolis, purpose-built for Triton.

**~20K LOC Rust | 266 tests | 7 crates | 54 source files**

## What works today

**Full guest boot** — Alpine Linux 3.21 boots to login prompt through both paths:

| Boot mode | Time to login | vs C bhyve |
|-----------|---------------|------------|
| `--kernel` (direct boot) | **2.9s** | 2.1x faster |
| `-l bootrom` (UEFI) | **4.3s** | 1.4x faster |
| C bhyve (UEFI) | 6.1s | baseline |

**Devices:** i440fx chipset, PIIX3 LPC bridge with PIRQ routing, virtio-blk (file-backed), virtio-net (viona kernel-accelerated), dual 16550 UARTs, ACPI (RSDP/RSDT/XSDT/FADT/MADT/HPET/DSDT/SPCR with full AML generation), SMBIOS 2.8, fw_cfg with table-loader protocol, E820, bhyve kernel devices (ATPIC, ATPIT, HPET, IOAPIC, RTC, PM timer), ACPI PM1 registers.

**Direct boot mode** — `--kernel`/`--initrd`/`--cmdline` flags load a Linux bzImage directly into 64-bit long mode, bypassing UEFI entirely. With a minimal initramfs, kernel reaches init in **460ms guest time** (970ms wall clock). Path to sub-125ms boot exists with a custom-compiled kernel.

**PCI interrupt delivery** — `PciIntrPin` calls `vm_isa_assert_irq(atpic, ioapic)` with correct separate values, matching C bhyve's `pci_irq_assert()`. Works with and without firmware-programmed PIR registers (direct boot compatibility).

## Crate structure

```
vmmnew/
  crates/bhyve-api/        Kernel VMM ioctl FFI (forked from propolis)
  crates/viona-api/         viona NIC driver FFI (forked from propolis)
  crates/vmm-core/          VmmHdl, PhysMap, MemCtx, Vcpu, PIO/MMIO buses, interrupt pins
  crates/vmm-devices/       PCI bus, i440fx chipset, virtio, UART, ACPI, SMBIOS, fw_cfg
  crates/vmm-migrate/       Migration protocol + dirty page tracking (scaffolding)
  crates/vmm-config/        CLI parsing (clap, bhyve flag-compatible)
  bin/vmm/                  Main binary — wires everything together
```

## What's next

| Phase | Scope | Status |
|-------|-------|--------|
| **Phase 3: Control socket** | Unix socket at `/var/run/vmm/{name}.sock`, JSON protocol (status, shutdown, console, migrate-start/accept) | Not started |
| **Phase 4: Live migration** | WebSocket transport, iterative dirty page push, pause + device state export/import, destination resume | Not started |
| **Phase 5: Additional devices** | NVMe, framebuffer/VNC, PCI passthrough, DTrace USDT probes | Not started |
| **Phase 6: Production** | boot.c integration, VMAPI migration workflow, performance tuning, hardening | Not started |

### Near-term items

- **Debug trace cleanup** — remove `eprintln!` debug output throughout device code
- **Sub-125ms boot** — custom kernel config (`CONFIG_TINY`, static drivers, no module loading)
- **Multi-vCPU testing** — AP startup via INIT/SIPI, verify SMP guests
- **Disk write support** — currently read-only virtio-blk for ISO boot; need r/w for zvol-backed disks
- **vmadm drop-in** — boot.c changes to invoke `vmm` instead of `bhyve`, `--control-socket` flag

## Build & test

```bash
# Local (macOS cross-check)
cargo test

# SmartOS headnode (actual VM execution)
ssh root@$VMM_HEADNODE
export PATH=/opt/tools/bin:$PATH
cd /opt/vmmnew && CARGO_HOME=/opt/.cargo cargo build --release

# UEFI boot
./target/release/vmm -c 1 -m 512M \
  -s 0,hostbridge -s 4,virtio-blk,/path/to/disk.iso -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin testvm

# Direct boot (fast)
./target/release/vmm -c 1 -m 512M \
  -s 0,hostbridge -s 4,virtio-blk,/path/to/disk.iso -s 31,lpc \
  --kernel /path/to/vmlinuz --initrd /path/to/initramfs.gz \
  --cmdline "console=ttyS0" testvm
```
