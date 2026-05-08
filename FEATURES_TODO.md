# vmmnew feature backlog

Living document. Update status as items land. Effort estimates assume one
focused engineer; pad accordingly for context-switches.

Conventions:
- ✅ shipped — committed and validated
- 🚧 in progress — branch open or partially landed
- ⏳ planned — agreed scope, not yet started
- ❓ considered — open question whether to do at all

---

## Shipped

| Item | Notes |
|---|---|
| ✅ Hyper-V Tier 1 enlightenments | CPUID + `hv-relaxed` + hypercall stub + reference-TSC + VP_INDEX + reset MSR + crash MSRs. Commit `d24ad41`. Synic/stimer/vAPIC/TLB-flush/IPI deferred to Tier 2. |

## In progress

| Item | Status | Notes |
|---|---|---|
| 🚧 vTPM 2.0 (Microsoft TLFS-compliant CRB) | libtpms backend builds on illumos with 4 patches, vendoring next | See `crates/vmm-tpm-sys/` plan below. Verified `libtpms.so.0.11.0` + `libtpms.a` produced on the dedicated build host. |

## Tier 1 — high-leverage, ship within a few months

Order roughly reflects sequencing: hot-plug infrastructure unlocks several others.

| Item | Effort | Why |
|---|---|---|
| ⏳ virtio-rng | ~3 days | Every guest. Linux blocks on entropy at boot without it. Trivial. |
| ⏳ PCI hot-plug | ~2 weeks | Cloud must-have. Triton VMs today require reboot to attach a disk. Touches PCI bridge + ACPI `_GPE`. Unlocks several Tier 1 items below. |
| ⏳ virtio-scsi | ~2 weeks | Windows storage UX: named LUNs, UNMAP/TRIM, hot-plug, multi-target per controller. NVMe stays the perf path; virtio-scsi for many-disk workloads. Build on hot-plug. |
| ⏳ virtio-balloon | ~1 week | Memory overcommit. Critical for Triton density. Mature drivers in Linux + Windows. |
| ⏳ Block I/O throttling (per-disk IOPS/BPS caps) | ~1 week | Multitenant isolation. Token bucket on virtio-blk + NVMe submission paths. |
| ⏳ Hugepage memory backing (2 MB / 1 GB) | ~3 days | TLB miss reduction for big VMs. illumos has the support; just wire `MAP_HUGETLB`-equivalent. Real win for >32 GB VMs. |
| ⏳ Watchdog device (`i6300esb`) | ~1 week | Guest stall detection. Pokes the host; we suspend/reset on timeout. |
| ⏳ Comprehensive SMBIOS tables | ~3 days | Type-2 baseboard, type-3 chassis, type-4 processor with full feature flags, type-16/17/19 memory devices. Windows Server licensing reads these. |

## Tier 2 — strategic, multi-week each

| Item | Effort | Why |
|---|---|---|
| ⏳ VNC server + virtio-input | ~3 weeks | Graphical console. Currently serial-only. Major UX win for Windows install/recovery and any non-headless workflow. |
| ⏳ QMP-style JSON monitor | ~2 weeks | Replace hand-rolled control socket with introspectable JSON-RPC. Unlocks libvirt-style tooling. |
| ⏳ Disk I/O multipathing / mirror | ~3 weeks | Today live migration assumes ZFS send+receive on the same dataset name. blockdev-mirror lets us migrate to a different storage backend. Useful when going beyond Triton-on-Triton. |
| ⏳ Hyper-V Tier 2 (synic + stimer) | ~3 weeks | Requires kernel cooperation in `vmm.ko`. Only worth doing if measurements show Windows is bottlenecked on APIC timer / synthetic interrupts. |

## Tier 3 — would consider but not soon

| Item | Effort | Why |
|---|---|---|
| ❓ virtio-fs | ~2 weeks | Modern 9p replacement. Useful for dev workflows; less so for production tenants. |
| ❓ virtio-vsock | ~1 week | Host↔guest socket channel without IP. Cloud agents use it; we have mdata-over-COM2 today. |
| ❓ qcow2 backing format | ~3-4 weeks | Copy-on-write images with internal snapshots. ZFS already provides this for zvol-backed guests; value is mostly off-Triton. |
| ❓ PCI passthrough (illumos VFIO equivalent) | ~4+ weeks | High-perf NICs/GPUs. illumos bhyve has limited support; significant integration. Niche. |
| ❓ CPU hot-plug | ~3 weeks | Possible but value-to-complexity ratio is poor. Just size VMs sensibly. |
| ❓ Memory hot-plug (separate from balloon) | ~3 weeks | Same. Use balloon for elasticity; hot-plug if/when justified. |

## Skip — out of scope for vmmnew

- **TCG / multi-arch** — we're x86_64-only, hardware-virt only.
- **SEV/SEV-ES/SEV-SNP/TDX** — illumos `vmm.ko` doesn't expose the AMD-V / VMX hooks. Wait until upstream lands.
- **Nested virtualization** — bhyve on illumos doesn't fully support it.
- **Sound, legacy USB, ARM/RISC-V targets, vhost-user** — outside vmmnew's mission.

## Validation backlog (cross-cutting)

These are not features, but quality work that should follow features:

- ⏳ Validate Hyper-V Tier 1 against Windows Server 2022 (need a Windows install on nuc0)
- ⏳ Migration plumbing for HyperV state (currently lost on migrate; guest re-writes overlays)
- ⏳ Migration plumbing for vTPM state (when vTPM lands, NV blob must travel with the VM)

---

## How to use this file

When picking up new work:
1. Check the in-progress column first — finish or hand off, don't add another.
2. From Tier 1, take the highest-row not blocked.
3. Move it to "in progress", land it, then move to "shipped" with the commit hash.
4. Don't promote Tier 2 or Tier 3 items without a written rationale (cost-benefit trended).
