# vmmnew feature backlog

Living document. Update status as items land and record the validation that
justifies moving an item to shipped.

**Current Windows baseline:** Headless Windows Server works today. Windows
Server 2025 evaluation WinPE boots from an ISO on a read-only NVMe namespace
and reaches SAC on COM1. The remaining hard blockers are in the graphical
console path. After those come Windows device visibility and platform fidelity,
especially ACPI TPM discovery and firmware validation.

Conventions:

- ✅ shipped: committed and validated
- 🚧 in progress: branch open or partially landed
- ⏳ planned: accepted backlog, not yet started
- ❓ considered: valid gap whose priority or scope is still open

---

## Shipped

| Item | Notes |
|---|---|
| ✅ Hyper-V Tier 1 enlightenments | CPUID identification, relaxed timing, hypercall stub, reference TSC, VP_INDEX, reset MSR, and crash MSRs. Commit `d24ad41`. Tier 2 remains kernel-blocked. |
| ✅ virtio-rng | Implemented in `crates/vmm-devices/src/virtio/rng.rs` and wired as `-s N,virtio-rnd`. Landed in `192a345`. |
| ✅ Framebuffer/VNC and xHCI tablet device | `fbuf/` provides framebuffer output and an RFB server; `xhci/` provides a USB HID tablet. Landed in `192a345`. This does not mean the graphical Windows path is complete: wide framebuffer MMIO stores are fatal, keyboard input does not exist, and pointer events are not connected to the tablet. |
| ✅ SMBIOS table set | `smbios.rs` emits Types 0, 1, 2, 3, 4, 16, 17, and 19, plus Types 32 and 127. Landed in `bc28ae8`. Type 4 topology fields still need correction. |
| ✅ PCI passthrough | PPT-backed passthrough is implemented in `passthru.rs` and wired as `-s N,passthru,<path>`. Landed in `192a345`. |
| ✅ vTPM 2.0 | Commits `018c723..212a83c` vendor libtpms in `third_party/libtpms`, add `vmm-tpm-sys` FFI, and add a TCG PTP CRB at `0xFED40000`, a TPM2 ACPI table, file-backed NV state, and `--vtpm` / `--vtpm-state-dir`. End-to-end CRB and libtpms tests landed. Windows discovery is still blocked by the missing `MSFT0101` DSDT device. |
| ✅ Guest power-off and reset | Commits `68f5bd9`, `8e4cfd9`, `b47f5cb`, `ecd987b`, and `5a5bea6` fix the `0xCF9` port conflict, wire ACPI S5 and reset into VM suspend, separate power-off from halt, re-exec the VMM after guest reset, and expose `{"command":"reset"}`. The former four-port PCI address claim at `0xCF8` swallowed `0xCF9`, so ACPI reset registration always failed and guest reset left a live VM burning a vCPU. |
| ✅ Kernel-owned MMIO instruction decoding | The illumos vmm kernel decodes MMIO instructions and delivers `VM_EXITCODE_MMIO` with the GPA and operands. `VM_EXITCODE_INST_EMUL` is a diagnostic-only fallback containing 15 raw instruction bytes and a valid-byte count, with no GPA or operands. Userspace does not decode it. |
| ✅ Writable UEFI variable store | Commits `854efb7`, `d1ed5e5`, and `e82d6c1` add `-l bootrom,<rom>,<varfile>`, size-derived flash layout, and locked CFI-backed persistence. A pristine template and a size-valid, page-aligned erased image filled with `0xFF` are both accepted. Secure Boot enrollment still requires host validation. |
| ✅ Per-device fw_cfg boot order | An optional `bootindex=N` token on a `-s` specification generates ordered PCI device paths and is stripped before driver parsing. Configurations without a boot index retain the legacy slot-4 payload byte for byte. Live firmware validation remains in the validation backlog. |
| ✅ PCI, CPU and memory hot-plug | `--hotplug` publishes the GPE0 block at `0xAFE0`, the PCI register file at `0xAE00`, the CPU one at `0xAF00` (only when `maxcpus` is above the boot count), the memory one at `0x0A00`, and the matching AML. Off by default, so an existing VM keeps byte-identical ACPI. rshyve serves `device-add`, `device-remove`, `device-list`, `cpu-add`, `cpu-list`, `mem-add` and `mem-list` on the control socket; firehyve serves the same set on the vsock `CONTROL` verb. `-c cpus=N,maxcpus=M` sizes the CPU slots and `-o hotplug.maxmem=` / `-o hotplug.memslot=` size the memory window, which also needs `-S`. Validation below. |

### Validated Windows Server 2025 baseline

Validated on the illumos build host against Windows Server 2025 evaluation
media at commit `5a5bea6`:

- Windows Setup WinPE boots and reaches the SAC prompt on COM1. The ISO's BCD
  entry is `Windows Setup [EMS Enabled]`, and SAC creates `SACSetupAct` and
  `SACSetupErr` channels without a framebuffer or keyboard.
- The ISO is attached as `-s N,nvme,<iso>,ro`. OVMF `PartitionDxe` finds the El
  Torito catalog on the plain NVMe namespace. ATAPI or AHCI CD-ROM emulation is
  not required for Windows installation.
- EFI Shell `reset` and control socket `{"command":"reset"}` both reboot a live
  WinPE guest. The process keeps the same PID because reset uses `execvp`; the
  kernel VM is recreated, a second boot is observed, and the guest returns.

### Validated hot-plug baseline

Validated on the firehyve test node against a Linux 6.12.13 guest built with
`kernel/hotplug.config`:

- The guest boots reporting `Allowing 2 present CPUs plus 6 hotplug CPUs` and
  `ACPI: Enabled 3 GPEs in block 00 to 0F`, registers `acpiphp` slots 2 to 31,
  and logs no ACPI error.
- `cpu-add`, `mem-add` and `device-add` each succeed over the vsock `CONTROL`
  verb. `mem-add` moves the host reservoir's free and allocated counters by
  exactly the size added.

What that does NOT cover, and is still open:

- The AML is not validated by `iasl`, in CI or anywhere else.
- The guest-side result is one kernel configuration on one lab node. No CI job
  boots a guest. Windows, illumos and BSD guests have not been tried.
- Disk hot-remove is advisory by design. A guest that never runs `_EJ0` leaves
  the slot in `remove-pending` for the life of the VM. Only the refusal paths
  have been exercised; a guest completing an `_EJ0` on a busy disk is not
  recorded.
- CPU and memory hot-REMOVE stay blocked at the kernel. See the blocked
  section below.

### UEFI variable-store recovery

Each VM uses a private writable `VARS.fd`. To clear persistent firmware state,
stop the VM and replace that file from the pristine template:

```console
# cp /opt/fw/BHYVE_UEFI_VARS.pristine.fd /var/db/vmm/<vm>/VARS.fd
```

A size-valid, page-aligned erased image filled entirely with `0xFF` is also
valid; firmware formats it on first boot. There is intentionally no one-shot
reset flag because guest reset re-executes the VMM with its existing argv.
Leaving such a flag in a Triton VM configuration would erase the variable store
on every reboot.

EDK2 culls auto-generated boot options when their device paths stop resolving.
It does not cull an OS-created Windows Boot Manager entry when a disk is wiped
or replaced. Per-VM variable files and this stopped-VM reset procedure recover
that state; the fw_cfg boot order does not remove persistent OS-created entries.

## In progress

No feature is currently recorded as in progress.

## Tier 1 - Windows readiness and high-leverage platform work

### Hard blockers for a graphical Windows guest

Headless Server works without these. Attaching `fbuf` currently makes the
graphical path unusable.

| Item | Why / tree evidence |
|---|---|
| ⏳ Add a keyboard device and wire VNC key events | `crates/vmm-devices/src/fbuf/mod.rs` receives RFB `KeyEvent` messages and discards them. No i8042 or PS/2 keyboard exists in the tree. Propolis has a Rust implementation suitable as a porting reference. |
| ⏳ Wire VNC pointer events to the xHCI tablet | `crates/vmm-devices/src/fbuf/mod.rs` discards RFB `PointerEvent` messages. `crates/vmm-devices/src/xhci/tablet.rs` already exposes `pointer_event(buttons, x, y)`, but the RFB server never receives a tablet handle. |
| ⏳ Raise xHCI MaxSlots to the Windows minimum | `crates/vmm-devices/src/xhci/bits.rs` advertises `MaxSlots = 1` in HCSPARAMS1. Windows `USBXHCI.SYS` requires at least 64 slots, matching bhyve's advertised value. |

### Major Windows visibility and platform fidelity

| Item | Why / tree evidence |
|---|---|
| ⏳ Add a DSDT TPM device with `_HID "MSFT0101"` | `crates/vmm-tpm/src/acpi.rs` emits only the TPM2 table and incorrectly says a DSDT node is unnecessary. Windows binds `TPM.SYS` through `ACPI\MSFT0101`, so the shipped CRB is currently invisible to the guest it targets. |
| ⏳ Validate Secure Boot enrollment and Windows hardware checks | Writable UEFI variables now persist, but Secure Boot key enrollment and the Windows 11 / Server 2025 hardware-check path have not been validated on the illumos host. |
| ⏳ Add a userspace MSR emulation table with architectural faults | Unclaimed RDMSR returns zero and unclaimed WRMSR is dropped in `bin/vmm/src/vcpu_tasks.rs`; bhyve instead injects `#GP`. In particular, reading `MSR_IA32_FEATURE_CONTROL` as zero is not a valid real-hardware state and Windows checks it. |

### Moderate Windows gaps

| Item | Why / tree evidence |
|---|---|
| ❓ Make ACPI publication use one generator | ACPI is generated once for fw_cfg table-loader and again for fixed guest-memory locations through separate functions. The two paths can drift. |
| ❓ Add a VM Generation Counter device | No VM Generation Counter device exists. Windows AD domain controllers and CNG consumers use it to detect snapshot rollback. |
| ❓ Make the RTC basis configurable | `main.rs` always initializes the RTC from Unix UTC. Windows interprets RTC as local time by default, producing a wall-clock offset unless the guest policy is changed. |
| ⏳ Correct SMBIOS Type 4 topology | `smbios.rs` emits cores per socket, enabled cores, and threads per socket as zero while `main.rs` configures CPU topology separately. Windows Server licensing consumes these fields. |
| ❓ Add an inbox-driver Windows NIC | The only emulated network path is viona-backed virtio-net. A fresh Windows install needs NetKVM from virtio-win before it has networking; there is no e1000 or comparable inbox-driver NIC. |
| ❓ Add framebuffer and xHCI migration state | Both devices implement only `type_name()` in `Lifecycle`; neither exports or restores device state for live migration. |
| ⏳ Migrate vTPM NV state | The file-backed vTPM NV directory is outside the migration protocol and does not travel with the VM. |
| ❓ Provide a FreeBSD-firmware utility image | The FreeBSD edk2-bhyve build has no EFI Internal Shell. If one is needed, place `ShellX64.efi` at `\\EFI\\BOOT\\BOOTX64.EFI` on a small FAT image attached as a second read-only NVMe namespace; it can also carry Secure Boot key-enrollment files. |

### Remaining Tier 1 platform work

| Item | Why / tree evidence |
|---|---|
| ⏳ virtio-scsi | No virtio-scsi device exists. It remains the useful Windows path for named LUNs, UNMAP/TRIM, and many targets, while NVMe remains the performance path. PCI hot-plug no longer depends on it. |
| ⏳ virtio-balloon | No balloon device exists. It is needed before guest-cooperative memory reclaim or overcommit is possible. |
| ⏳ Block I/O throttling | NVMe and virtio-blk have no per-disk IOPS or bandwidth limiter. Multitenant rate isolation remains absent. |
| ⏳ Hugepage memory backing | Guest RAM setup has no hugepage backing option. Any illumos implementation needs measurement and an explicit fallback policy. |
| ⏳ Watchdog device | No guest watchdog such as `i6300esb` exists. Guest stall detection and watchdog-triggered reset remain absent. |

## Blocked on illumos kernel work

| Item | Boundary |
|---|---|
| ⏳ Hyper-V Tier 2 | SynIC, synthetic timers, virtual APIC support, TLB-flush hypercalls, and IPI hypercalls require cooperation from `vmm.ko`. They are out of scope for the userspace backlog until the kernel interfaces exist. |
| ⏳ CPU hot-remove | illumos has no `vm_deactivate_cpu`. `active_cpus` is only ever set, and the one place that clears it is `vm_init` on `VM_REINIT`. `cpu-remove` is recognised and refused with that reason. |
| ⏳ Memory hot-remove | illumos has no `VM_FREE_MEMSEG`, and `vm_free_memseg` has only destroy-path callers, so a segment lives until the VM does. `mem-remove` is recognised and refused with that reason. |

## Tier 2 - strategic, multi-week work

| Item | Why / tree evidence |
|---|---|
| ⏳ QMP-style JSON monitor | The current control socket is a fixed, newline-delimited command enum. A versioned, introspectable JSON-RPC monitor would support richer external tooling. |
| ⏳ Disk I/O mirror / backend-independent migration | Storage migration is ZFS-zvol specific. There is no block mirror or backend-independent copy path for migration to different storage. |

## Tier 3 - would consider but not soon

| Item | Why |
|---|---|
| ❓ virtio-fs | Modern 9p replacement. Useful for development workflows; less important for production tenants. |
| ❓ virtio-vsock | Host-to-guest socket channel without IP. Cloud agents can use it; vmmnew has mdata over COM2 today. |
| ❓ qcow2 backing format | Useful for portable layered images and internal snapshots. ZFS already supplies copy-on-write storage for the primary Triton deployment. |

## Skip - out of scope for vmmnew

- **TCG / multi-arch:** vmmnew is x86_64-only and hardware-virtualization-only.
- **SEV, SEV-ES, SEV-SNP, and TDX:** illumos `vmm.ko` does not expose the
  required AMD-V or VMX hooks. Wait for kernel support.
- **Nested virtualization:** illumos bhyve does not provide the required full
  nested-virtualization support.
- **Sound, legacy USB, ARM/RISC-V targets, and vhost-user:** outside vmmnew's
  mission.

## Validation backlog (cross-cutting)

These are validation gates, not new features:

- ⏳ Validate Hyper-V Tier 1 against an installed Windows Server workload.
  The WinPE boot/reset test did not exercise the Hyper-V path.
- ⏳ Complete a Windows Server installation using writable UEFI variables and
  bootindex, then verify that the guest's boot entry persists.
- ⏳ Validate graphical Windows boot, keyboard, and pointer input after the
  listed graphical-console blockers are fixed.
- ⏳ Add migration coverage for Hyper-V state, framebuffer/xHCI state, and
  vTPM NV state as each migration path lands.
- ⏳ Validate the hot-plug AML with `iasl`, and boot a `--hotplug` VM with a
  guest other than the one Linux configuration in the hot-plug baseline.

---

## How to use this file

When picking up new work:

1. Check the in-progress section first. Finish or hand off before starting
   another item.
2. From Tier 1, take the highest listed item that is not blocked.
3. Move it to in progress, land it, then move it to shipped with commit hashes
   and concrete validation evidence.
4. Do not promote Tier 2 or Tier 3 items without a written cost-benefit
   rationale.
5. Update the Windows baseline whenever host validation changes what is known
   to boot or work.

When working with virtio, use the
[Virtio 1.3 specification](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html).

Propolis is the upstream reference:
<https://github.com/oxidecomputer/propolis>.
