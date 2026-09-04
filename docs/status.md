# Feature Status

## Tested Configurations

| Scenario | Result |
|----------|--------|
| UEFI boot Ubuntu 24.04 (2 vCPU, 1G RAM) | Working |
| NVMe boot disk on ZFS zvol | Working |
| virtio-net with cloud-init SmartOS datasource | Working |
| SSH access via mdata-delivered keys | Working |
| Guest gets IP via mdata sdc:nics | Working |
| Cross-host migration (old CPU -> new CPU) | Working |
| Cross-host migration (new CPU -> old CPU) | Rejected (CPU feature check) |
| Same-host migration with busy workload | Working |
| SIGTERM graceful shutdown | Working |
| dd sequential read (NVMe, O_DIRECT) | 2.8 GB/s (matches C bhyve) |

## Devices

| Device | Status | Notes |
|--------|--------|-------|
| i440fx chipset | Complete | Host bridge + LPC/ISA bridge |
| UART 16550 (COM1/COM2) | Complete | stdio, device path, mdata agent |
| virtio-blk | Complete | nodelete, sectorsize, read-only |
| NVMe | Complete | Async I/O, zero-copy, 8 workers, 2.8 GB/s |
| virtio-net (viona) | Complete | Interrupt polling, promiscphys |
| virtio-rnd | Complete | /dev/random entropy |
| Framebuffer/VNC | Complete | Unix socket, password, resolution |
| xHCI USB tablet | Complete | HID pointer device |
| PCI passthrough | Partial | Intel VT-d only; MSI-X devices refused. See [devices.md](devices.md#pci-passthrough) |
| AHCI CD-ROM (`ahci-cd`) | Complete | ATAPI SCSI/MMC, zero-copy reads |
| AHCI disk (`ahci-hd`) | Not implemented | Rejected at startup; use virtio-blk or nvme |
| LAPIC/IOAPIC/PIC/PIT | Complete | Kernel-emulated |
| HPET | Complete | Kernel-emulated |
| RTC | Complete | Memory size in NVRAM |
| PM Timer | Complete | ACPI power management |

## Boot

| Feature | Status |
|---------|--------|
| UEFI (OVMF) | Complete |
| SMP (multi-vCPU) | Complete (13s boot, any vCPU count) |
| ACPI tables | Complete (table-loader + legacy fallback) |
| SMBIOS tables | Complete (fw_cfg + guest memory at 0xF0000) |
| fw_cfg | Complete |
| E820 memory map | Complete |
| Direct kernel boot | Complete |

## Management

| Feature | Status |
|---------|--------|
| Control socket (JSON/Unix) | Complete |
| SIGTERM -> ACPI poweroff | Complete |
| Built-in mdata agent (V2) | Complete (testing only; production uses external agent) |
| CLI bhyve compatibility | Complete (all boot.c flags supported) |
| Serial console (-l com1,stdio) | Complete |
| Serial to device path (-l com1,/dev/zconsole) | Complete |
| SMBIOS -B parsing | Complete (manufacturer, product, serial, etc.) |

## Live Migration

| Feature | Status |
|---------|--------|
| Same-host migration | Complete |
| Cross-host (old->new CPU) | Complete |
| Cross-host (new->old CPU) | Requires --cpu-baseline |
| CPU feature check at preamble | Warns only | Does not reject a mismatch; sync and async paths compare differently |
| CPUID masking (--cpu-baseline) | Complete (host, avx2, sse42) |
| RAM transfer (pre/post pause) | Complete |
| Kernel device state | Complete (IOAPIC, ATPIT, ATPIC, HPET, RTC, PM Timer) |
| Per-vCPU state (regs, MSRs, LAPIC) | Complete |
| Per-vCPU run state (HALT/INIT/RUN) | Complete |
| VMM_TIME adjustment (Propolis pattern) | Complete |
| TSC frequency scaling | Complete |
| Auto-resume on failure | Complete |
| FPU/XSAVE state | Not yet implemented |

## Platform Integration Readiness

| Requirement | Status |
|-------------|--------|
| All boot.c CLI flags | Supported |
| PCI slot assignments (0, 3-6, 29, 30, 31) | Supported |
| SMBIOS "SmartDC HVM" for cloud-init detection | Default |
| COM2 device path for external mdata agent | Supported |
| SIGTERM -> ACPI poweroff for zoneadmd | Supported |
| Dirty page tracking for migration | Supported |
| Control socket for migration commands | Supported |

## Known Limitations

- `ahci-hd` not implemented (`ahci-cd` is); use virtio-blk or nvme for disks
- FPU/XSAVE state not transferred during migration
- Migration transport is unauthenticated and unencrypted; see SECURITY.md
- NVList/zhyve.cmd reader not implemented (boot.c integration uses CLI flags directly)
- Large VMs (>32 GB) not tested with migration
- virtio-net TX may show initial delay on first packet (viona interrupt poll startup)
