# Feature status

The one status document for this project. The crate structure is in
[architecture.md](architecture.md), per-device limits and caveats are in
[features.md](features.md), and what a live guest actually established
is in [hardware-run.md](hardware-run.md). Open security findings are in
[security/open-findings.md](security/open-findings.md).

## Tested configurations

| Scenario | Result |
|----------|--------|
| UEFI boot Ubuntu 24.04 (2 vCPU, 1G RAM) | Working |
| NVMe boot disk on ZFS zvol | Working |
| virtio-net with cloud-init SmartOS datasource | Working |
| SSH access via mdata-delivered keys | Working |
| Guest gets IP via mdata sdc:nics | Working |
| Cross-host migration (old CPU -> new CPU) | Working |
| Cross-host migration (new CPU -> old CPU) | Needs `--cpu-baseline`; a mismatch is warned about, not rejected (OSF-3) |
| Same-host migration with busy workload | Working |
| SIGTERM graceful shutdown | Working |
| dd sequential read (NVMe, O_DIRECT) | 2.8 GB/s (matches C bhyve) |
| 293 guest device resets under load, data verified after each | Working |
| 5 warm reboots with a dirty guest page cache | Working |
| virtio-fs and viona reset with traffic in flight | Working |
| 30 guest ACPI S5 poweroffs, no leaked process or VM instance | Working |
| 140 device resets on a Debian 13 guest, 4 vCPUs (100 modern, 40 legacy) | Working |
| Multiqueue virtio-blk, 128 MiB written and verified per queue with vCPU pinning | Working |

The reset, reboot and poweroff rows come from two lab sessions. The
second ran a Debian 13 guest with 4 vCPUs, a real block driver stack
and a real init. What they do and do not show is in
[hardware-run.md](hardware-run.md).

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
| PCI passthrough | Partial | Intel VT-d only, no AMD IOMMU backend; MSI-X devices refused. Not security-audited (OSF-5). See [devices.md](devices.md#pci-passthrough) |
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

## Live migration

| Feature | Status |
|---------|--------|
| Same-host migration | Complete |
| Cross-host (old->new CPU) | Complete |
| Cross-host (new->old CPU) | Requires --cpu-baseline |
| CPU feature check at preamble | Warns only, does not reject; the sync and async paths compare differently (OSF-3) |
| CPUID masking (--cpu-baseline) | Complete (host, avx2, sse42) |
| RAM transfer (pre/post pause) | Complete |
| Kernel device state | Complete (IOAPIC, ATPIT, ATPIC, HPET, RTC, PM Timer) |
| Per-vCPU state (regs, MSRs, LAPIC) | Complete |
| Per-vCPU run state (HALT/INIT/RUN) | Complete |
| VMM_TIME adjustment (Propolis pattern) | Complete |
| TSC frequency scaling | Complete |
| Auto-resume on failure | Complete |
| FPU/XSAVE state | Not yet implemented |

## Platform integration readiness

| Requirement | Status |
|-------------|--------|
| All boot.c CLI flags | Supported |
| PCI slot assignments (0, 3-6, 29, 30, 31) | Supported |
| SMBIOS "SmartDC HVM" for cloud-init detection | Default |
| COM2 device path for external mdata agent | Supported |
| SIGTERM -> ACPI poweroff for zoneadmd | Supported |
| Dirty page tracking for migration | Supported |
| Control socket for migration commands | Supported |

## Known limitations

- `ahci-hd` not implemented (`ahci-cd` is); use virtio-blk or nvme for disks
- FPU/XSAVE state not transferred during migration
- Migration transport is unauthenticated and unencrypted; see SECURITY.md
- NVList/zhyve.cmd reader not implemented (boot.c integration uses CLI flags directly)
- Large VMs (>32 GB) not tested with migration
- Hyper-V enlightenment state is not in the migration payload, so a
  `--hyperv` guest's reference time restarts on the destination
  (OSF-8)
- virtio-net TX may show initial delay on first packet (viona interrupt poll startup)
- A device reset has no bounded duration. Anything holding the bhyve VM
  write lock delays it, and memory hot-add holds that lock for 233 to
  270 ms per GiB. See
  [hardware-run.md](hardware-run.md#why-a-device-reset-has-no-upper-bound)
- The control socket's peer check runs on macOS and Linux only and the
  autodestruct test runs on illumos only, so neither runs on both.
  `tools/check-test-parity.py` now fails CI when that set moves. See
  [hardware-run.md](hardware-run.md#coverage-that-does-not-reach-illumos)
