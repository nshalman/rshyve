# rust-bhyve feature backlog

What is not built yet, and why. Everything that ships is described in
[docs/features.md](docs/features.md); what a live guest established is in
[docs/hardware-run.md](docs/hardware-run.md). This file carries only the
open work, so a row here is a claim about the current tree.

Status words: `planned` is accepted backlog, `considered` is a real gap
whose priority is still open, `blocked` waits on an illumos kernel
change.

Every row was re-checked against the tree on 2026-09-20. A row that
names a path is wrong the moment that path stops saying what it says.

## Windows readiness

Headless Windows Server works: Windows Server 2025 evaluation WinPE
boots from an ISO on a read-only NVMe namespace and reaches SAC on
COM1. The graphical console path, keyboard and pointer input, the
`MSFT0101` TPM device and xHCI slot count all landed since this file was
first written.

| Status | Item | Tree evidence |
|---|---|---|
| planned | Validate Secure Boot enrollment and the Windows hardware checks | Writable UEFI variables persist, but enrollment and the Windows 11 / Server 2025 hardware-check path have not been run on an illumos host. |
| planned | Correct SMBIOS Type 4 topology | `crates/vmm-devices/src/smbios.rs:493` pushes cores per socket, enabled cores and threads per socket as zero while CPU topology is configured elsewhere. Windows Server licensing reads these fields. |
| planned | Userspace MSR table with architectural faults | `crates/vmm-machine/src/vcpu_tasks.rs:491` logs an unhandled RDMSR and returns zero; bhyve injects `#GP`. Reading `MSR_IA32_FEATURE_CONTROL` as zero is not a state real hardware produces, and Windows checks it. |
| planned | Migrate vTPM NV state | The file-backed NV directory is outside the migration protocol and does not travel with the VM. |
| considered | Framebuffer and xHCI migration state | `crates/vmm-hid/src/fbuf/mod.rs:859` and `crates/vmm-hid/src/xhci/mod.rs:1377` implement only `type_name()`. Neither exports or restores device state. |
| considered | One ACPI generator | ACPI is generated once for the fw_cfg table loader and again for fixed guest-memory addresses through separate functions. The two can drift. |
| considered | VM Generation Counter | No such device exists. Windows AD domain controllers and CNG use it to detect snapshot rollback. |
| considered | Configurable RTC basis | `crates/vmm-machine/src/chipset.rs:67` always sets the RTC from Unix UTC. Windows reads the RTC as local time by default. |
| considered | An inbox-driver NIC | viona-backed virtio-net is the only network path, so a fresh Windows install has no network until NetKVM is present. No e1000 equivalent exists. |
| considered | A FreeBSD-firmware utility image | The edk2-bhyve build has no EFI Internal Shell. `ShellX64.efi` at `\EFI\BOOT\BOOTX64.EFI` on a small FAT image, attached as a second read-only NVMe namespace, would also carry Secure Boot enrollment files. |

## Platform work

| Status | Item | Tree evidence |
|---|---|---|
| planned | virtio-scsi | No such device. The useful Windows path for named LUNs, UNMAP/TRIM and many targets; NVMe stays the performance path. |
| planned | virtio-balloon | No such device; `crates/vmm-virtio/src/bits.rs` carries the feature bit only. Needed before guest-cooperative reclaim or overcommit. |
| planned | Block I/O throttling | NVMe and virtio-blk have no per-disk IOPS or bandwidth limit, so there is no multitenant rate isolation. |
| planned | Hugepage memory backing | Guest RAM setup has no hugepage option. An illumos implementation needs measurement and an explicit fallback. |
| planned | Watchdog device | No `i6300esb` or equivalent, so no guest stall detection and no watchdog-triggered reset. |
| planned | QMP-style JSON monitor | The control socket is a fixed newline-delimited command enum. A versioned, introspectable monitor would support external tooling. |
| planned | Backend-independent storage migration | Storage migration is ZFS-zvol specific. There is no block mirror or backend-independent copy path. |
| considered | qcow2 backing format | Useful for portable layered images and internal snapshots. ZFS already supplies copy-on-write storage for the Triton deployment. |

## Blocked on illumos kernel work

| Status | Item | Boundary |
|---|---|---|
| blocked | Hyper-V Tier 2 | SynIC, synthetic timers, virtual APIC support, and the TLB-flush and IPI hypercalls all need cooperation from `vmm.ko`. |
| blocked | CPU hot-remove | illumos has no `vm_deactivate_cpu`. `active_cpus` is only ever set, and the one place that clears it is `vm_init` on `VM_REINIT`. `cpu-remove` is recognised and refused with that reason. |
| blocked | Memory hot-remove | illumos has no `VM_FREE_MEMSEG`, and `vm_free_memseg` has only destroy-path callers, so a segment lives until the VM does. `mem-remove` is recognised and refused with that reason. |
| blocked | A `VM_DESTROY_SELF` that can fail | `vmm_lease_block` waits for every vmm_drv lease in an untimed, unwakeable `cv_wait`. A lease that does not break parks the calling LWP, and process exit then waits for that LWP. Detail below. |
| blocked | A bounded device reset | Both interrupt injection ioctls take the bhyve VM read lock, and illumos grants that lock to a waiting writer first. Any VM write-lock holder therefore delays every device reset on that VM, without limit. Detail below. |

### Upstream: bound the vmm_drv lease wait

`VM_DESTROY_SELF` cannot fail and cannot be interrupted, so a VMM whose
lease will not break can never exit. This is the one shutdown wedge the
userspace watchdog cannot end.

**The path.** `vmm_destroy_locked` -> `vmm_destroy_begin` ->
`vmm_drv_purge` -> `vmm_lease_block`
(`usr/src/uts/intel/io/vmm/vmm_sol_dev.c`). `vmm_drv_purge` marks every
hold for release, drops `vmm_mtx`, and calls `vmm_lease_block`. That
function runs each lease's expire callback and then waits for the lease
list to empty. Both waits there are `cv_wait`, not `cv_wait_sig`, and
neither carries a timeout.

**Why a lease stays.** viona breaks its lease asynchronously.
`viona_ring_lease_expire_cb` returns `B_FALSE` and only signals the ring
worker (`usr/src/uts/intel/io/viona/viona_ring.c`), so the break happens
the next time that worker reaches its `vmm_drv_lease_expired()` check. A
worker that does not reach the check holds the lease, the lease holds
`vmm_lease_block`, and that holds the LWP.

`viona_tx_wait_outstanding` (`viona_tx.c`) is one such point: a second
untimed `cv_wait`, on TX frames the MAC layer still holds. It is
reachable only on a link with guest-memory loaning on. illumos defaults
`viona_default_tx_copy` to `B_TRUE` and this VMM never calls
`VNA_IOC_SET_PARAMS`, so loaning is off in the shipped configuration.
The unbounded wait in `vmm_lease_block` is the part that does not depend
on any of that.

**Why nothing frees the process.** A thread in `cv_wait` does not carry
`T_WAKEABLE`, so `ISWAKEABLE()` is false, `pokelwps` cannot make it run,
and `exitlwps` waits for it (`usr/src/uts/common/os/lwp.c`,
`usr/src/uts/common/sys/thread.h`). `proc_exit` calls `exitlwps`, so the
process cannot exit. `SIGKILL` and `_exit` do not change that.

**Why userspace cannot close it.** This VMM already runs the destroy on
its own thread with a 15 s budget, reports the timeout, and ends the
process with `_exit(4)` when the teardown budget expires
(`crates/vmm-machine/src/teardown.rs`, `signal.rs`). The process still
stays, because that exit is what waits for the parked LWP.

**What the kernel needs.** Bound or interrupt the waits in
`vmm_lease_block` so `VM_DESTROY_SELF` can return `EINTR` or `EBUSY`.
Two points go with it. The `VM_DESTROY_SELF` case asserts
`VERIFY0(vmm_destroy_locked(...))`, so it panics on any non-zero return
today. And `vmm_destroy_locked` does have an interruptible branch
(`cv_wait_sig` under `VDO_ATTEMPT_WAIT`), but it runs after
`vmm_destroy_begin`, so it never covers the lease wait. A failed destroy
is safe for this VMM: `VM_SET_AUTODESTRUCT` is armed at create, so a
later close still reclaims the instance.

Lab procedure: [docs/testing.md](docs/testing.md#wedged-lease-teardown).

### Upstream: let an interrupt injection through a waiting writer

A device reset ends with an untimed wait over the interrupt injections
already in flight (`IntrGate::settle`,
`crates/vmm-virtio/src/pci/intr.rs`). Each of those ends in one
`VM_LAPIC_MSI` or `VM_IOAPIC_ASSERT_IRQ`. Neither
`vlapic_set_intr_ready` nor `vioapic_assert_irq` can block, so the
delay is not in the work. It is in the lock the ioctl takes first.

**The path.** `vmm_ioctl` classifies both injection ioctls as
`LOCK_READ_HOLD` and calls `vmm_read_lock`, which is
`rw_enter(&sc->vmm_rwlock, RW_READER)`
(`usr/src/uts/intel/io/vmm/vmm_sol_dev.c:503,506,517,362`). illumos runs
a writer priority policy: a reader fails when the lock is owned OR
wanted by a writer (`usr/src/uts/common/os/rwlock.c:64-70`). So one
waiting writer stops every later injection.

**Who holds the write lock.** `VM_ALLOC_MEMSEG`, `VM_MMAP_MEMSEG`,
`VM_PAUSE`, `VM_RESUME` and `VM_DATA_READ`/`VM_DATA_WRITE` with
`vcpu == -1`, which is VM-wide device-state export
(`vmm_sol_dev.c:488,528`). All are reachable on a live VM.
`vmm_write_lock` also freezes every vCPU and calls `vmm_lease_block`
before it takes the lock, so its hold covers more than the work it
protects.

**The measurement.** `VM_ALLOC_MEMSEG` alone held the write lock 69 ms
for a 256 MiB hot-add and scaled at 233 to 270 ms per GiB. A 16 GiB
hot-add stalls a device reset for seconds. Interrupt injection itself
measures p50 1.28 us and p99 4.48 us on an idle VM.

**Why userspace cannot close it.** `settle` cannot take a deadline.
Admission tests the driver session before the ioctl, so an injection
already dispatched cannot be recalled; a reset that stopped waiting
would return, the guest would enable its line for the next driver, and
the stale injection would land on it. This is also worse on illumos
than elsewhere: `virtio_legacy_device_reset_locked` and
`virtio_modern_device_reset_locked`
(`usr/src/uts/common/io/virtio/`) are each a single status write with
no read-back, so the guest is stopped inside one register write for
the whole reset.

**What the kernel needs.** Take the injection ioctls with
`RW_READER_STARVEWRITER` rather than `RW_READER`. That mode acquires
while a writer waits, as long as no writer owns the lock, and it is
documented as safe alongside `RW_READER` on the same lock
(`rwlock.c:173-178`). It buys a bounded injection at the cost of writer
starvation, so it is a kernel design decision. The alternative is
operational, not a fix: keep memory hot-add off a VM whose guest may
reset a device.

Measurements and limits: [docs/hardware-run.md](docs/hardware-run.md#why-a-device-reset-has-no-upper-bound).

## Out of scope

- **TCG and other architectures.** x86_64 only, hardware virtualization
  only.
- **SEV, SEV-ES, SEV-SNP and TDX.** illumos `vmm.ko` exposes none of the
  required AMD-V or VMX hooks.
- **Nested virtualization.** illumos bhyve does not provide it.
- **Sound, legacy USB, ARM and RISC-V targets, vhost-user.**

## Validation backlog

Gates, not features:

- Validate Hyper-V Tier 1 against an installed Windows Server workload.
  The WinPE boot and reset test did not exercise the Hyper-V path.
- Complete a Windows Server installation using writable UEFI variables
  and `bootindex`, then confirm the guest's boot entry persists.
- Add migration coverage for Hyper-V state, framebuffer and xHCI state,
  and vTPM NV state as each path lands.
- Validate the hot-plug AML with `iasl`, and boot a `--hotplug` VM with
  a guest other than the one Linux configuration already measured.
- Boot an illumos guest. Every device reset result to date is against
  Linux drivers. illumos writes the virtio status register once and
  never reads it back (`uts/common/io/virtio/virtio_legacy.c:208`,
  `virtio_modern.c:242`), which is the weakest contract any driver
  offers this VMM, and it has only been read from the source.
- Produce a real vmm_drv lease park. The teardown budget, its log
  records and the shutdown watchdog have never run against a lease that
  will not break. The DTrace procedure in
  [docs/testing.md](docs/testing.md#wedged-lease-teardown) delays the
  release; it does not stop it.

## Working on an item

Take the highest `planned` row that is not blocked. When it lands,
delete the row and describe the behaviour in
[docs/features.md](docs/features.md); do not move it into a "shipped"
table here, because git log already holds that history.

Write virtio devices against the
[VIRTIO 1.3 specification](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html).
[Propolis](https://github.com/oxidecomputer/propolis) is the upstream
reference.
