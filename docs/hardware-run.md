# What a live guest established

The test suite is evidence about compilation and internal logic. It is
not evidence about how a real driver sees the device. This file records
what a live guest showed on the target platform, what it did not show,
and the one platform limit the run found, so nobody has to measure it
again.

Recorded 2026-09-05 against `hyp-seam` at commit `d1ec631`. Every
number comes from one lab host: SunOS 5.11, x86_64, 40 CPUs, bhyve API
v18. Numbers are comparable only against a later run on the same
host.

## The suite on the target platform

1781 tests, 0 failures. `cargo clippy` and `cargo fmt` both clean. The
release binaries link and start.

`rshyve` records `NEEDED libcrypto-smartos.so.3` and `ld.so.1` resolves
it from `/lib/64`, so the SmartOS crypto link farm in
`crates/vmm-tpm-sys/build.rs` does what
[platform.md](platform.md#openssl-and-the-vtpm) says. CI cannot see
this part: the runner has no platform OpenSSL, and a cross build falls
back to pkg-config, which is the configuration that once produced a
binary no platform image could load.

Pass `--no-fail-fast`. Without it the run stops at the first crate that
fails and the partial output still reads like a completed pass.

## The device reset matrix

293 device resets driven from inside a live guest, with a `dd` running
against the disk the whole time. After every reset the guest read back
a 16 MiB file and compared its SHA-256. 0 failures and 0 mismatches.

| Case | Resets | What it drives |
|---|---|---|
| modern transport | 273 | The transport a modern driver negotiates |
| forced legacy | 10 | Status write with one read-back and no poll |
| bounded | 10 | The reset that must end while work is in flight |

The forced-legacy case is the one that matters, because it is the
closest the run came to the contract an illumos guest imposes. That
contract is stricter still. `virtio_legacy_device_reset_locked`
(`uts/common/io/virtio/virtio_legacy.c:208`) writes `VIRTIO_STATUS_RESET`
and returns. It does not read the status back and it does not wait.
`virtio_modern_device_reset_locked` (`virtio_modern.c:242`) is the same
one write, so an illumos guest gets no read-back on either transport.
Linux is stricter on both: `vp_reset`
(`drivers/virtio/virtio_pci_legacy.c:93`) writes 0 and reads once, and
the modern `vp_reset` (`virtio_pci_modern.c:546`) polls with `msleep`
until status reads 0.

The consequence is not a driver that spins. Both `reset_device` call
sites (`crates/vmm-virtio/src/pci/legacy.rs:203`,
`pci/modern.rs:417`) run on the vCPU exit thread, so the guest's next
instruction does not execute until the reset returns. An illumos guest
is stopped inside one register write for the complete reset duration,
and it has no read-back that could ever tell it the reset is late.

Also passed, same host, same session:

- 5 warm reboots with a dirty guest page cache.
- virtio-fs and viona resets with traffic in flight.
- 30 guest-initiated ACPI S5 poweroffs, with 0 leaked processes and 0
  leaked kernel VM instances.

## What the matrix does not show

State these as plainly as the results.

- **No illumos guest was booted.** The driver contract above is read
  from the illumos source, not observed. Nothing here proves how that
  driver behaves against this device.
- **No wedge was produced.** The teardown budget and the shutdown
  watchdog stayed unexercised against a real vmm_drv lease that will
  not break. See
  [testing.md](testing.md#wedged-lease-teardown) for what that
  procedure does and does not reach.
- **The guest was a busybox initramfs, not a distro image.** A
  multiqueue defect blocked the distro guest. The defect is not
  characterised here, and until it is, no result above carries over to
  a guest with a real block driver stack and a real init.

## Why a device reset has no upper bound

`finish_reset` (`crates/vmm-virtio/src/pci.rs`) ends by calling
`IntrGate::settle` (`pci/intr.rs:266`), an untimed condition wait over
the interrupt deliveries admission let through before it shut. Each of
those deliveries ends in one injection ioctl, `VM_LAPIC_MSI` or
`VM_IOAPIC_ASSERT_IRQ`. That wait is the residual cost of a reset, so
the measurement below is of the ioctl inside it.

| Condition | p50 | p99 | max |
|---|---|---|---|
| idle | 1.28 us | 4.48 us | 18.1 us |
| 40 spinning threads in the guest | not recorded | not recorded | 4.03 ms |
| during a 256 MiB memory hot-add | not recorded | not recorded | 17.9 ms |

The important part is not the numbers. Neither `vlapic_set_intr_ready`
nor `vioapic_assert_irq` can block. But **both injection ioctls take
the bhyve VM read lock at dispatch** (`uts/intel/io/vmm/vmm_sol_dev.c:503,506`
select `vmm_read_lock`), and illumos `rw_enter(RW_READER)` yields to a
waiting writer. The rwlock theory statement is explicit: a reader fails
when the lock is owned or *wanted* by a writer
(`uts/common/os/rwlock.c:64-70`).

The write lock is held by operations that are reachable on a live VM:
`VM_ALLOC_MEMSEG` and `VM_MMAP_MEMSEG` behind memory hot-add,
`VM_PAUSE` and `VM_RESUME`, and `VM_DATA_READ`/`VM_DATA_WRITE` with
`vcpu == -1`, which is VM-wide device-state export
(`vmm_sol_dev.c:488,528`). `vmm_write_lock` also freezes every vCPU
first and calls `vmm_lease_block`, so its hold is longer than the work
it protects.

`VM_ALLOC_MEMSEG` alone held the write lock 69 ms for a 256 MiB add,
and scaled at 233 to 270 ms per GiB. A 16 GiB hot-add would therefore
stall a device reset for seconds.

Read that hold as a measurement, not as something derivable. Neither
published cost model predicts it: [boot-baseline.md](boot-baseline.md)
puts the create-time `VM_MMAP_MEMSEG` mapping at about 58 ms per GiB,
and [features.md](features.md#memory-hot-add) says reservoir allocation
is microseconds at any size. Both were taken at VM create, where no
vCPU has to be frozen and no lease has to be blocked. Measure the
live-VM hold; do not derive it.

### What this means, and what would change it

The complete duration of a device reset has no bound on illumos, and no
userspace change creates one. It is an unbounded delay, not a hang: the
reset finishes when the write lock is released. `settle` cannot be
given a deadline. Admission tests the driver session before the ioctl,
not after, so an injection already dispatched cannot be recalled. A
reset that stopped waiting would return, the guest would enable its
interrupt line for the next driver, and the stale injection would land
on it.

Two things would bound it, and both are outside this VMM:

1. **Exclude memory hot-add against device reset.** Whatever
   orchestrates a node can refuse to hot-add memory to a VM whose guest
   may reset a device, or serialise the two. This is a policy, not a
   fix, and it does not cover pause, resume or device-state export.
2. **A kernel change.** Take the injection ioctls with
   `RW_READER_STARVEWRITER` instead of `RW_READER`. That mode acquires
   while a writer waits, as long as no writer owns the lock, and it is
   documented as safe to use alongside `RW_READER` on the same lock
   (`uts/common/os/rwlock.c:173-178`). It buys a bounded injection at
   the cost of writer starvation, so it is a kernel design decision and
   not a patch to propose from here.

## The socket bound is measured, not assumed

`strmsgsz` reads 65536 on the lab node against an 8 KiB write chunk, so
the margin is 8x. [platform.md](platform.md#host-tunables) and
[features.md](features.md#known-caveats) carry the chain; the value is
no longer an assumption in either.

The macOS test pass proves nothing about this. The two systems behave in
opposite directions: `SO_SNDTIMEO` bounds an AF_UNIX send on macOS and
is accepted and then ignored on illumos, so the code that needs the
bound is exactly the code macOS never exercises.

## Coverage that does not reach illumos

Most viona device tests **do not compile** on the target platform.
`VionaInner` holds either `viona_fd: Arc<viona_api::VionaFd>` on
illumos or `link: Arc<dyn LinkOps>` everywhere else
(`crates/vmm-virtio/src/viona.rs:113,121`). The two are mutually
exclusive fields, so the substitutable seam a test needs exists only
off the target, and `viona/tests.rs` gates both test modules on
`cfg(not(target_os = "illumos"))`.

The gap widens with every viona test added, and it did so during this
work:

| Commit | viona tests | Compiled out on illumos |
|---|---|---|
| `e597ea0` | 25 | 22 |
| `d1ec631` | 29 | 26 |

**The fix is to make the seam exist on illumos too**: hold the link as
`Arc<dyn LinkOps>` on every platform and let the illumos build put the
kernel handle behind it, which `VionaInner::link` already returns
(`viona.rs:159`). Until then, every viona test is a test of the macOS
and Linux build.

Two smaller gates go the same way. `bin/rshyve/src/peercred.rs:130`
gates its test to Linux and macOS, so the control socket's peer check
is untested on the only platform that runs it. In the other direction
`crates/vmm-core/tests/autodestruct.rs` is illumos only, so CI never
runs it.

The whole-workspace effect was measured at about 12 more tests on macOS
than on illumos across the four crates carrying platform gates, and
that delta grew from 5 to 12 across two commits. Treat 12 as a
snapshot, not a constant: it was taken before the viona tests above
landed, and the gates now in the tree already account for more.

**This needs a guard that compares per-crate test counts on both
platforms.** Reading the gates does not work, because the number a
reader reconstructs is already out of date by the time it is written
down. Count both sides and fail on a change nobody explained.
