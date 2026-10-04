# Live Migration

## Overview

Live migration transfers a running VM from one compute node to another
with minimal downtime. The protocol uses WebSocket over TCP with RON
serialization for structured data.

## Protocol Phases

One driver serves both transports: a Unix socket inside a bhyve zone,
where `/dev/poll` is unavailable and the GZ agent bridges to the peer,
and a TCP socket elsewhere.

The protocol identifier is `vmm-migrate-ron/3`, and a source offers
only that. A destination also takes `vmm-migrate-ron/2`, whose payload
is a strict subset: v3 adds the NVMe completions held for a full guest
completion queue, which a v2 source dropped, and the NVMe namespace
shape, of which a v2 source had one (512-byte blocks, volatile write
cache). A v2 payload restores with none held, onto that shape only. Anything older is refused, because a peer that reads a field
as something else resumes a guest on state that is not its own. v1 put
device identity on the wire; in v0 every virtio queue group was tagged
slot 0 and the destination took the first group whose length matched
its own queue count.

```
Source                          Destination
  |                                 |
  |  1. Sync (WebSocket + protocol negotiation)
  |-------- ProtocolOffer --------->|
  |<------- ProtocolSelect ---------|
  |-------- Preamble -------------->|
  |<------- Okay -------------------|  vCPU count, memory size,
  |                                 |  device set, CPU features
  |  2. RamPushPrePause (VM still running)
  |-------- PageBatch* ------------>|
  |-------- MemEnd ---------------->|
  |<------- MemDone ----------------|  (repeats until converged)
  |-------- PauseSignal ----------->|
  |                                 |
  |  3. Pause
  |  device workers park            |
  |  kernel rings stop              |
  |  vCPUs pause                    |
  |  backing stores flush           |
  |  ZFS barrier reports            |
  |                                 |
  |  4. RamPushPostPause (final dirty pass)
  |-------- PageBatch* ------------>|
  |-------- MemEnd ---------------->|
  |<------- MemDone ----------------|
  |                                 |
  |  5. TimeData (TSC + boot_hrtime adjustment)
  |-------- TimeData -------------->|
  |                                 |
  |  6. DeviceState (kernel + emulated devices)
  |-------- DeviceState ----------->|
  |                                 |  identity check, import,
  |                                 |  device workers start, resume
  |  7. Finish                      |
  |<------- Okay -------------------|  "I hold everything"
  |-------- Okay ------------------>|  the source's commit
  |                                 |  device workers start, resume
  |<------- Okay -------------------|  "the guest is running here"
```

The three-step finish is the handoff, and the order is the whole point.
The destination says it holds everything; the source commits, which is
its promise never to run the guest again; only then does the destination
start the guest, and its last message is what lets a finished source be
reaped. A destination that resumed before the commit would run a second
copy of the guest against the same disk whenever the source rolled back.

A source that has committed and then loses the peer stays paused and is
never released: it cannot take the guest back, because the destination
may be running it. The control socket reports the VM still migrating and
an operator decides.

The final dirty pass runs after every device worker has parked, so it
is the last word on guest memory by construction. Before that change it
ran while the console and vsock reader threads could still fill a
receive ring.

### Read deadlines

Every read has a deadline. Before the pause it is generous, because the
guest is still running on the source and a slow peer costs only time.
After the pause it is short: the guest is stopped on both hosts, and
each second of waiting is guest downtime, so a rollback happens while
the outage is still small.

### Device identity

Each device that carries state is named on the wire by its PCI
bus/device/function. The preamble lists the whole set, and the
destination refuses a source whose set is not its own, before it accepts
a single page. The device payload then carries one entry per address,
including the ones with nothing to carry, so "the source had no state"
is distinct from "the source has a device this VM does not".

Matching by anything weaker restored one device's rings onto another:
two virtio-blk disks have one queue each and are otherwise identical.

## CPU Feature Compatibility

The destination compares the whole effective guest CPUID contract:
CPUID leaf 1 ECX and EDX, leaf 7 subleaf 0 EBX, ECX and EDX, and the
XCR0 component mask. Both ends report what they expose to a guest after
their own `--cpu-baseline` mask, because that is what the guest saw and
what it will see.

A destination missing any bit the source exposes is refused. The guest
may already have executed such an instruction and would take #UD in
kernel context on the next, which no error path recovers.

```
this host lacks CPU features the guest already sees (leaf1.ecx=0x10000000
leaf1.edx=0x0 leaf7.ebx=0x20 leaf7.ecx=0x0 leaf7.edx=0x0 xcr0=0x4)
```

To accept that risk, pass `allow_cpu_feature_mismatch` on the
`migrate-dest` control command. The destination logs a warning naming
the missing bits whenever the override is used.

### CPU Baseline Profiles

Use `--cpu-baseline` to limit guest CPU features for migration
compatibility between different CPU generations:

| Profile | Description |
|---------|-------------|
| `host` | No masking (default). Only same-generation migration. |
| `avx2` | Mask AVX-512. Migrate between ~2014+ Intel CPUs. |
| `sse42` | Mask AVX-512 + AVX2. Maximum compatibility. |

## State Transfer

### Kernel Devices (via vmm-data ioctls)

- IOAPIC, ATPIT, ATPIC, HPET, PM Timer, RTC
- Per-vCPU: registers, MSRs, LAPIC, VMM_ARCH
- Per-vCPU: run state (HALT/INIT/RUN) and SIPI vector
- VMM_TIME with boot_hrtime and TSC frequency adjustment
### Emulated Devices

Per virtio PCI transport: the device status byte as the driver left it,
the negotiated features, the config MSI-X vector, the PCI command and
BAR bases, the whole MSI-X table (Message Control, every entry with its
mask, and the pending bits), and every configured queue with its ring
addresses, its size, its cursors and its vector.

A queue is live if the driver enabled it through the modern register, or
programmed it through the legacy PFN register, which has no enable bit.
Reading the modern flag alone migrated every legacy queue as dead, and
the destination then skipped both its backend restore and its wake.

MSI-X is restored as the source had it. Forcing it on gave an INTx guest
a device whose queue interrupts all routed to `VIRTIO_MSI_NO_VECTOR`,
and moved device config from 0x14 to 0x18 so a legacy read of capacity
returned the vector registers instead.

Per NVMe controller: CC, CSTS, AQA, the admin queue bases, every admin
and I/O queue pair with its size, cursors, phase and vector, the PCI
command and BAR bases, and the MSI-X table.

For a VM started with `--hyperv`: `HV_X64_MSR_GUEST_OS_ID`,
`HV_X64_MSR_HYPERCALL`, `HV_X64_MSR_REFERENCE_TSC`, the five crash MSRs,
and the partition reference counter. The hypercall and reference-TSC
overlay pages are not shipped: the source removes them before the final
RAM pass, so the memory image holds the guest's own bytes, and the
destination reinstalls both by replaying the MSR writes. The reference
counter continues from the source's last value, because a guest is told
that counter is monotonic.

A payload carrying enlightenment state for a VM started without
`--hyperv` is refused.

Every field is checked before any of it is applied: queue sizes against
the device's own, cursors inside their ring, ring addresses mapped and
aligned, vectors inside the table, and each NVMe submission queue's
completion queue present. The migration peer is unauthenticated
(OSF-1), so a payload the destination cannot hold fails the migration
rather than half-programming a device.

### Time Adjustment

TSC and boot_hrtime are adjusted using the Propolis pattern:

```
guest_uptime = src.hrtime - src.boot_hrtime
migrate_delta = dst.wall_clock - src.wall_clock
new_boot_hrtime = dst.hrtime - (guest_uptime + migrate_delta)
```

If TSC frequencies differ between hosts, the guest TSC is scaled
proportionally and the destination's `guest_freq` is used.

All of it is checked arithmetic over `i128`. The peer chooses every
source field, and the kernel's only guard on the result is that
`boot_hrtime` is not in the future, which a wrapped value satisfies. A
wall-clock gap beyond an hour is refused as clock disagreement rather
than migration time.

## What migration does not carry

The source refuses to migrate a VM that holds any of these, because the
guest driver would find the device at reset on the far side with no
error anywhere:

| Device | What is lost |
|--------|--------------|
| `xhci` | Controller registers, every endpoint ring and its state |
| `ahci-cd` | Port registers, command list, attached media |
| `virtio-fs` | The FUSE session and every open handle in it |
| `virtio-vsock` | Every open connection |
| `virtio-console` | Every open connection |

The list lives in `UNMIGRATABLE` in `bin/rshyve/src/control/migrate.rs`.
Grow it there and here together.

These are carried as plain guest RAM only, and their device state is
still lost. They are not refused, because the loss is recoverable by the
guest and refusing would block every VM:

- LPC UART: IER, LCR and the FIFO contents. A console loses buffered
  bytes; the port works again at once.
- i8042 / PS-2: controller and keyboard state.
- `fw_cfg`: the selector, which only firmware reads.
- ACPI PM1 and GPE: the event and enable registers.
- FPU and XSAVE state. See OSF-2.

A VM that hot-added a CPU, memory, or a device since boot is also
refused: the destination is built from the boot command line and would
have none of them.

## Usage

### Source Side

```bash
vmm ... --control-socket /tmp/vm.sock myvm

# Trigger migration
echo '{"command":"migrate-source","target_addr":"10.0.0.2:4567"}' \
  | socat - UNIX-CONNECT:/tmp/vm.sock
```

### Destination Side

```bash
vmm ... --migrate-listen 0.0.0.0:4567 myvm
```

The destination creates the VM with RAM and devices but does not start
vCPUs. It waits for the source to connect and transfer state.

A VM that is already running can take an import through the control
socket instead, but only while it is paused: the import writes every
vCPU's registers and every device's rings, and a running guest would
execute against half of them.

```bash
echo '{"command":"migrate-dest","listen_addr":"0.0.0.0:4567"}' \
  | socat - UNIX-CONNECT:/tmp/vm.sock
```

### Monitor Progress

```bash
echo '{"command":"migrate-status"}' | socat - UNIX-CONNECT:/tmp/vm.sock
# {"success":true,"state":"migrating","migrate_phase":"RamPushPrePause",...}
```

## The ZFS barrier

The GZ agent replicates the disk while the guest runs, and runs one last
incremental once the guest is paused. The VMM connects to the barrier
socket to say "the vCPUs are paused", and the agent answers with one
byte: 0 means the destination disk holds every write the guest was told
was complete, anything else means it does not.

The VMM treats a non-zero byte, a timeout, a connect failure, and a
closed socket all as a failed migration and resumes its guest. A guest
that resumed on the destination over an older disk had already
acknowledged writes that disk never received.

## Waking the guest

The restore raises an interrupt on each live queue before the vCPUs
start, and one more once they are running. The second one is the one
that matters: a driver that is not running cannot take an interrupt,
and after a migration the receive ring may hold no available buffers,
so the driver has nothing to poll for and posts none until something
wakes it.

There used to be a third: a LAPIC injection of vector 0xEC on every
vCPU. That is Linux's `LOCAL_TIMER_VECTOR` and an arbitrary vector
anywhere else, so on Windows or illumos it delivered a spurious
interrupt or hit a real handler. The LAPIC, its timer included, is
migrated through `VDC_LAPIC` and the kernel re-arms it on `VM_RESUME`,
so nothing needed the injection.

## Error Recovery

A migration that fails once the guest is paused rolls the source back:
device workers resume, the kernel rings a migration paused are
re-initialised from their saved state, and the vCPUs run again.

A paused viona ring is `VRS_STOP` in the kernel, and `viona_ioc_ring_kick`
answers EBUSY for one, so a guest kick cannot revive it. Only the reset
plus state write the rollback does can, which is why `resume` alone left
the source guest with a dead NIC.

A `migrate-dest` that fails puts the VM back in the state it was in.
Marking it `Stopped` stranded a live guest that no control command could
reach.

The destination VM should be destroyed by the caller.
