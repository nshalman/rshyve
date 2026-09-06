# What a live guest established

The test suite is evidence about compilation and internal logic. It is
not evidence about how a real driver sees the device. This file records
what a live guest showed on the target platform, what it did not show,
and the one platform limit the run found, so nobody has to measure it
again.

Two sessions, both on one lab host: SunOS 5.11, x86_64, 40 CPUs,
bhyve API v18. The first ran 2026-09-05 at commit `d1ec631` on a
busybox initramfs. The second reran the matrix on a Debian 13 guest
after the multiqueue fix in `704fb1e`. Numbers are comparable only
against a later run on the same host.

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

That session ran a busybox initramfs, because a multiqueue defect
stalled every distro image. The rerun below removes that limit.

## The rerun on a distro guest

The defect was found, fixed and proved on hardware. Every worker
completion raised its interrupt on virtqueue 0. Under MSI-X each
virtqueue has its own vector, and a guest handler reads only its own
ring, so a completion on any other queue was never collected. Linux
blk-mq puts vCPU k's I/O on queue k, so any guest with 2 or more vCPUs
stalled. INTx hid it, because the shared handler walks every ring.
Fixed in `704fb1e`.

The matrix then reran on Debian 13 with 4 vCPUs: a real block driver
stack, a real init and a real page cache.

| Case | Count | Result |
|---|---|---|
| modern transport resets | 100 | 0 mismatches |
| forced legacy resets | 40 | 0 mismatches |
| warm reboot, 64 MiB dirty page cache | 1 | clean |
| ACPI S5 poweroffs | 10 | 0 leaked processes, 0 leaked kernel VM instances |

Per queue, the guest wrote 128 MiB and read it back, with `taskset`
pinning the writer to the vCPU that owns that queue. All four MSI-X
vectors fired: 639, 423, 407 and 460 interrupts.

Read the per-vector counts before you call a multiqueue run a pass.
Traffic on one vector only, or on three of four, is the signature of
the defect above, and every aggregate number stays healthy while it
happens.

## What the matrix does not show

State these as plainly as the results.

- **No illumos guest was booted.** The driver contract above is read
  from the illumos source, not observed. Nothing here proves how that
  driver behaves against this device.
- **No wedge was produced.** The teardown budget and the shutdown
  watchdog have never run against a real vmm_drv lease that will not
  break, so the teardown fix is unexercised against a real park. See
  [testing.md](testing.md#wedged-lease-teardown) for what that
  procedure does and does not reach.

## What only a guest could find

Booting a guest found three defects that no test suite on either
platform could see. Each one passed a full green run on macOS and on
illumos first. The pattern repeats, so it is worth naming: each defect
sits where a test's stand-in is not the thing the guest touches.

**Completions raised on the wrong queue** (`704fb1e`), described
above. A test asserts that a completion lands in the used ring, and
that was always correct. The defect was in which vector was raised
next, and no test delivers an interrupt to a driver that reads one
ring.

**Legacy device config read four bytes early** (`651ace5`). The legacy
layout puts two vector registers at 0x14 and 0x16, but only after the
driver enables MSI-X. The code picked the offset on whether the device
*has* an MSI-X table, and every device is built with one, so device
config sat at 0x18 for every driver. A guest that left MSI-X off read
0x14 and got the vector registers instead. It read disk capacity back
as 0x0140000000FF00FF against a real 0x01400000: the four bytes it
wanted, four bytes late. Tests read config through the same rule the
code wrote it with, so the two sides agreed and the suite passed.

**Promiscuous mode passed by pointer** (`0c2aaca`).
`viona_ioc_set_promisc` casts the ioctl data argument straight to its
enum, so a pointer set the mode to a userspace address and the kernel
answered EINVAL. Every VM with `allow_mac_spoofing` failed to attach
its NIC. A recorder that records the call cannot catch this, because
only the kernel reads that argument the way that matters.

A suite cannot check the meaning a kernel or a driver gives to a value.
Boot a guest before you call a device path proved.

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

This was the largest gap in the tree, and for viona it is closed.

`VionaInner` used to hold two mutually exclusive fields: `viona_fd:
Arc<viona_api::VionaFd>` on illumos, `link: Arc<dyn LinkOps>`
everywhere else. The seam a test substitutes into existed only off the
target platform, so `viona/tests.rs` gated both test modules on
`cfg(not(target_os = "illumos"))`, and 26 of 29 viona tests compiled
out on the one platform that runs the code.

`f896980` collapsed the two fields into one `Arc<dyn LinkOps>` on every
platform, with the kernel handle behind it there. Viona tests executing
on illumos went from 3 to 39, and no platform gate is left in them.
`dfd478a` then moved the `LinkOps` trait into `viona-api`, beside the
`VionaFd` that implements it, so the illumos adapter sends its ioctls
directly and one delegation layer is gone.

Do not read 39 as current. The count grows with every viona test, and
that is the point: a number a reader reconstructs is out of date by the
time it is written down.

Two smaller gates remain. `bin/rshyve/src/peercred.rs:130` gates its
test to Linux and macOS, so the control socket's peer check is untested
on the only platform that runs it. In the other direction
`crates/vmm-core/tests/autodestruct.rs` is illumos only, so CI never
runs it.

**A guard that compares per-crate test counts on both platforms is
still missing.** The whole-workspace delta was about 12 tests before
this work and viona was most of it, but what is left does not reduce to
the gates now in the tree. Count both sides and fail on a change nobody
explained.
