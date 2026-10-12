# Live Migration

## Overview

Live migration transfers a running VM from one compute node to another
with minimal downtime. The protocol uses WebSocket over TCP with RON
serialization for structured data.

## Protocol Phases

One driver serves both transports: a Unix socket inside a bhyve zone,
where `/dev/poll` is unavailable and the GZ agent bridges to the peer,
and a TCP socket elsewhere.

The protocol identifier is `vmm-migrate-ron/13`. v13 adds the state a
virtio device keeps behind its transport (see "virtio-gpu and
virtio-snd" below) and the xHCI tablet's unread wheel steps. A VM with
virtio-gpu or virtio-snd needs v13 at both ends; any other VM sends a
payload a v12 peer reads, which ignores the wheel steps. v12 has the
v10 payload, but the framebuffer VRAM is LZ4 only and the source must
send its wall clock. Builds before v12 sent v8 to v10 with the VRAM as
zstd or as LZ4, and v10 with or without the wall clock, under one
version string. So to a v8 to v10 peer the source sends the VRAM in
both forms, and from one the destination takes either form and no wall
clock. v11 was never released: the virtio-gpu work (PR #15) held it
before v12, and moved to v13. A source offers v13, v12, v10, v9, v8, v7
and v6, and a destination takes the newest it reads. Each end then
checks that the chosen version carries everything this VM has, and
refuses an older peer at the protocol choice, before the pause. v10
added the source host's wall clock to the preamble, the interrupt
shadow, the AMD PMU state, and the fault-injection test device (see
"Kernel Devices", "Host clock skew" and "Testing a retry" below). An
AMD source with PMU state, or a VM with the test device, needs v10 or
newer at both ends. v9 added the xHCI controller and the devices
with no PCI address. Every VM has the COM1 and COM2 UARTs, so in
practice every VM needs v9 or newer at both ends.
v8 added the framebuffer state. v7 has the v6 payload and says the
destination attaches a MantaBlock ring disk after the commit (see
[migration-gate.md](migration-gate.md)); a source with a ring disk
refuses a v6 destination before it pauses. A peer on any other version
is refused at negotiation. v6 carries each vCPU's FPU and XSAVE area
with the source host's XSAVE layout, and the count of NVMe Asynchronous
Event Requests a controller holds. A v4 peer carried no FPU state, so
the guest resumed on the destination's FPU registers (OSF-2). v4 named
the store under each NVMe namespace, v3 added held NVMe completions and
the namespace shape, v2 the CQ interrupt-enable bit, and v1 device
identity. v5 was the virtio-gpu work's own stream before it was built
on v10 (PR #15), and is refused.

Every PCI device header on the wire also carries the Interrupt Line
the firmware wrote, since v9.

### Framebuffer

The `fbuf` device carries its PCI header (command and BARs), the mode
the guest set (width and height), and its VRAM. The VRAM is a separate
16 MiB devmem segment, not guest RAM, so the RAM passes do not see it.
The source reads it after the vCPUs pause, cuts it after the last page
that is not zero, compresses it with LZ4 and sends it with the device
state. To a peer before v12 it also sends a zstd frame of the same
bytes, so both kinds of older build read it. A console or a plain
desktop compresses to a few KiB. If the compressed VRAM is over 2 MiB,
the source leaves it out so the device state stays under the message
limit. The destination then starts with black VRAM and the right mode,
and the screen comes back as the guest draws again.

The VNC socket path and password come from the destination's own `-s`
spec, which `migrate-config` gives. They are not on the wire. VNC
clients of the source are dropped and reconnect to the destination.

### virtio-gpu and virtio-snd

virtio-gpu carries every resource (its id, size and backing page
list), the scanout, the cursor and the size offered to the guest. The
pixels are not on the wire: every resource is backed by guest pages,
which the RAM passes carry, and the destination reads each one back
from them and shows the scanout whole. virtio-snd carries the stream's
parameters and state, so a running stream plays the guest's next
message on the destination without a new START. Its pause hands back
every queued message at once, unplayed, so the guest loses a fraction
of a second of sound.

VNC and RDP clients of the source are dropped and reconnect to the
destination, which serves the display with its own `-s` options,
certificate and password file.

### xHCI tablet

The `xhci` device (the tablet VNC and RDP drive) carries its PCI header (command,
BARs and Interrupt Line), the operational, port and interrupter
registers, the controller's cursors into the command ring, the event
ring and both transfer rings, the slot state, whether the guest left
an interrupt-IN TRB armed, and the tablet's position, buttons, wheel steps
not yet reported and USB settings. The wheel is new in v13; a v12 peer
drops it, losing the steps the guest had not yet read. The
device contexts and the rings are guest memory, which the RAM passes
carry, and the model keeps the output device context current there.

No transfer is in flight at the pause. Every ring is walked inline on
the thread that rang the doorbell. The one writer of guest memory that
runs when no vCPU does is a pointer event from VNC or RDP, and the pause holds
it back: the report waits in the tablet. The destination restores the
state paused, replays the BARs so BAR0 decodes again, and on its resume
delivers a held report to the TRB the guest left armed. A source that
rolls back does the same on its own resume.

### Devices with no PCI address

The LPC and ACPI devices that user space emulates travel in their own
list, each under its registry id. The preamble names them, and the
destination refuses a source whose set is not its own before it takes a
page. Each device in the set must restore one state; a missing, extra
or doubled entry fails the import.

| Id | What is carried |
|----|-----------------|
| `lpc/com1`, `lpc/com2` | IER, LCR, MCR, LSR, MSR, the scratch register, the divisor latch, a THRE interrupt the guest has not taken, and both FIFOs with their bytes |
| `lpc/ps2` | The i8042 command byte, output port, pending response and command, its RAM, and the keyboard and mouse buffers and settings |
| `acpi/pm1` | PM1 status, enable and control (SCI_EN), and the reset register |
| `acpi-gpe` | GPE0 status and enable |
| `chipset/i440fx` | The 0xCF8 address latch, the PIRQ routing, the POST code, and both bridge headers |
| `fwcfg` | The item selector and read offset |
| `acpi/cpu-hotplug`, `acpi/mem-hotplug` | The slot selector, and the CPU command |

The RTC and CMOS, the PIT, the i8259 pair, the I/O APIC, the HPET and
the ACPI PM timer are in the kernel and travel as kernel payloads. A
UART's IIR and interrupt line, and the GPE0 SCI, are computed again
from the carried registers. The model has no FCR state.

The UARTs, the i8042 and the xHCI pass no host input to the guest
while paused: a byte that landed after the kernel interrupt state was
exported would raise a line the destination kernel never sees, and the
port would stall. Each holds the input for its resume instead. A UART
holds up to 256 bytes, and the i8042 up to 64 keys and scan codes, in
the order they came; past that, input is dropped. The xHCI tablet keeps
its latest pointer position, buttons and unread wheel steps, which
migrate with it. What a UART or the i8042 held on the source does not
migrate; the console and the mdata agent retry. On the destination
these devices stay paused from the restore to the resume. Their interrupt lines are set
on restore only so this side's count of each line matches the level
that the kernel import then puts back.

TX bytes a UART still holds go out on the destination's resume, and the
THRE interrupt that follows lets an interrupt-driven guest driver send
on. Bytes the source had already handed to a host backend stay on the
source. The mdata agent's own state is not carried: a request that was
in flight is not answered, the guest's mdata client times out, and it
negotiates again with the destination's agent.

### FPU and XSAVE state

The preamble carries the source host's XSAVE layout from
`VM_DESC_FPU_AREA`: the area size and the size and offset of each
component the host enabled. Before it takes a page, the destination
refuses the source if a component the guest was offered in XCR0 is not
on this host at the same size and offset. `allow_cpu_feature_mismatch`
does not change this.

Each vCPU's area travels as `VM_GET_FPU` wrote it. The destination
checks the length against the source layout, refuses a compacted area,
reserved header bits, and components the source does not have, then
copies each component to its offset in an area of the local size. A
component the guest was never offered and this host cannot hold goes
back to its initial state. All vCPUs are checked before any state is
written, then `VM_SET_FPU` loads each area after the registers.

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
  |                                 |
  |  3. Pause
  |  vCPUs pause                    |
  |  device workers drain and park  |
  |  kernel rings stop              |
  |  backing stores flush           |
  |  ZFS barrier reports            |
  |  cutover gate says GO           |
  |-------- PauseSignal ----------->|
  |                                 |
  |  4. RamPushPostPause (final dirty pass)
  |-------- PageBatch* ------------>|
  |  device export                  |
  |-------- PageBatch* ------------>|  pages dirtied since, if any
  |-------- MemEnd ---------------->|
  |<------- MemDone ----------------|
  |                                 |
  |  5. TimeData (TSC + boot_hrtime adjustment)
  |-------- TimeData -------------->|
  |                                 |
  |  6. DeviceState (kernel + emulated devices)
  |-------- DeviceState ----------->|
  |                                 |  identity check, import
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
an operator decides; `stop` reaps it.

A stop (SIGTERM or `stop`) during a migration latches the suspend at
once. Before the commit it also cancels the migration. The migration
thread checks for the cancel before every forward send, and every 50 ms
while it waits for a connect, a name lookup, an accept or the durable
flush. The stop shuts every socket the migration opened, the ZFS
barrier's too, so a send or a read blocked on a quiet peer fails. This
end tells the peer and gives up, and the source rolls back. Two waits
are not cut short: the device quiesce, at most 30 seconds, and the vCPU
and kernel ring pause ioctls. A stop that finds the vCPU pause running
waits up to 10 seconds for it, then answers busy.

Connects are non-blocking, so a stop closes a connect still in progress
on the migration thread itself. Only a name lookup runs on a thread of
its own, and one the stop left behind holds no socket and ends when the
resolver answers. A durable flush the stop left behind finishes the
device it is on and then stops. Teardown waits for it before it touches
a device, under the same clock as the rest of teardown. If that clock
runs out, the process ends the way the SIGTERM watchdog ends it, and
teardown never runs beside the flush.

After a side's commit, a stop lets the handover finish, and shuts the
sockets only after 30 seconds, because a send has no deadline on
illumos. A stop on the destination after it told the source it was
ready still ends the destination, and the guest does not start there.
If the source committed, it stays paused, and the guest runs on
neither host until an operator acts on the source. The source has no
way yet to take the guest back; see
[issue #29](https://github.com/TritonDataCenter/rust-bhyve/issues/29).

On either side the pause is given back, and teardown starts, only when
the migration thread ends, so teardown never runs under it. A release
that fails then is retried at once, then four times 2 seconds apart,
then every 30 seconds. Once the fast tries are spent, `status` reports
it and the VM stays `stopping`:

```json
{"success":true,"state":"stopping",...,"release_stuck":true}
```

The field goes as soon as the pause is given back. If releases keep
failing for 5 minutes, the VMM logs why and ends itself the way the
SIGTERM watchdog does, so the zone's restart policy and the instance's
autodestruct clean up.

If a migration's rollback or final resume cannot release the vCPUs and
the device workers will not park again, the VM goes to `faulted`. So
does an operator `resume` that fails the same way. A faulted VM takes
only `stop`, so `paused` always means parked devices.

The final dirty pass runs after every device worker has parked, so it
is the last word on guest memory by construction. Before that change it
ran while the console and vsock reader threads could still fill a
receive ring.

The vCPUs pause before the devices drain, as in Propolis, so the guest
submits nothing new while in-flight requests complete. The device
export runs inside the final pass, and the pass sends the pages dirtied
since its first round before its `MemEnd`. Each read of the dirty bitmap
drops the VMM's mappings of guest RAM, and the next fault through the
tracked view marks the page dirty, even for a read. A page still dirty
at the commit fails the migration, and the error names the first pages
and the device ring each one belongs to.

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

The registers include DR0 to DR3, DR6 and DR7, so a guest debugger's
hardware breakpoints survive. EFER travels twice: in the kernel's MSR
list and as a register; both carry the same value. The kernel's MSR
list has no DEBUGCTL, and no register gives it, so it is not carried
(Propolis does not carry it either).

VMM_ARCH carries the pending NMI, ExtINT, exception and interrupt
info. It is written last, after the run state.

The destination wants every register exactly once: the whole list from
a v10 source, and the list without DR0 to DR3, DR6 and the shadow from
a v9 one. A register left out would resume at the destination's reset
value.

From v10, the interrupt shadow goes too. VMX cannot force a shadow on
(`vmx_setreg` answers EINVAL), so the destination logs a warning and
goes on: the guest may take an interrupt one instruction early. That
beats a failure after the gate's go, which leaves the source paused.

From v10, an AMD source whose kernel lists `VDC_PMU_AMD` (bhyve v18)
sends each vCPU's PMU state. Every v18 kernel lists the class, Intel
included, but only SVM keeps the state, so the source reads it only on
an AMD host, and a destination that is not AMD refuses it.
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
command and BAR bases, the MSI-X table, the namespace shape, and the
store under the namespace.

For a VM started with `--hyperv`: `HV_X64_MSR_GUEST_OS_ID`,
`HV_X64_MSR_HYPERCALL`, `HV_X64_MSR_REFERENCE_TSC`, the five crash MSRs,
and the partition reference counter. The hypercall and reference-TSC
overlay pages are not shipped: the source removes them before the final
RAM pass, so the memory image holds the guest's own bytes, and the
destination reinstalls both by replaying the MSR writes. The reference
counter continues from the source's last value, because a guest is told
that counter is monotonic.

A payload carrying enlightenment state for a VM started without
`--hyperv` is refused, and so is a payload without it for a VM started
with `--hyperv`.

A source that rolls back puts both overlay pages back from the MSR
values. A destination that takes a new `migrate-dest` after a failed
import drops the overlays of that import first, so the new RAM passes
land in the guest's own pages.

Every device on the wire must have exactly one entry, and the state in
it must be of the device's own kind. An entry with no state is taken
only from a device that may have none, such as a disabled NVMe
controller. A virtio device, the framebuffer and the xHCI controller
always carry one.

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

### Host clock skew

The guest clock moves by the skew between the two hosts: the time import
counts the wall-clock gap as time the guest was paused. Propolis assumes
synced clocks. From v10 the source sends its wall clock in the preamble
(from v12 the destination refuses a source without it), and the
destination compares it with its own before a single page moves. It logs
the skew, warns over 1 s, and refuses over 60 s. `migrate-status` on the
destination shows it as `migrate_clock_skew_ms`, positive when the
destination is ahead.

## Guest memory

The first pass sends every page of guest RAM, whatever the dirty
bitmap says. A failed attempt cleared the bits of the pages it sent,
and the destination of the next attempt holds none of them, so a
dirty-only first pass would leave those pages behind. Propolis puts the
bits back with `VNO_OP_SET_DIRTY` instead; a full first pass needs no
kernel support and cannot drift.

A page the source cannot read, or the destination cannot write, fails
the migration. Nothing is sent or skipped in its place.

After the device export, the source reads the dirty bitmap once more.
Every writer stopped before the final pass, so a dirty page there is a
write the destination never got. The source then sends an error in
place of the end of the RAM phase and rolls back. The check comes
before the destination says it holds everything, so the destination
fails cleanly and is not left in doubt.

## What migration does not carry

The source refuses to migrate a VM that holds any of these, because the
guest driver would find the device at reset on the far side with no
error anywhere:

| Device | What is lost |
|--------|--------------|
| `ahci-cd` | Port registers, command list, attached media |
| `virtio-fs` | The FUSE session and every open handle in it |
| `virtio-vsock` | Every open connection |
| `virtio-console` | Every open connection |
| `passthru` | Host hardware state; DMA writes are not dirty-tracked |

The list lives in `UNMIGRATABLE` in `bin/rshyve/src/control/migrate.rs`.
Grow it there and here together. A `passthru` device has a PCI handle
and no lifecycle, and `device_blockers` refuses any such device on the
source and on the destination.

A VM that hot-added a CPU, memory, or a device since boot is also
refused: the destination is built from the boot command line and would
have none of them.

An NVMe disk on a MantaBlock volume moves with a handover: the source
detaches it after the commit, and the destination attaches it anew
before the guest runs. The cutover gate, the destination `ring-shape=`
option and the engine check are in
[migration-gate.md](migration-gate.md).

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

A stop can close the barrier socket at any point: before the connect
completes, after it completes and before the agent answers, or while
the agent is still writing its byte. The agent is not part of this
repository, so this is a requirement on it: a connection that closes
before the agent sends its byte is an abort. The agent must not take it
as a go-ahead, and must not start or finish a switchover because of it.

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

A `migrate-dest` that fails does not go back to `paused`: the import
may have written part of the guest, and the source may roll back and
run it. The VM goes to one of three states:

| State | When | Takes |
|-------|------|-------|
| `import-failed` | it failed before it told the source it was ready | `migrate-dest` again, or `stop` |
| `import-in-doubt` | it told the source it was ready and never heard the commit | `resume` with `"force":true`, or `stop` |
| `import-committed` | the source's commit arrived and the last step failed | `resume`, or `stop` |

In doubt, the source may have committed, and this VM may hold the only
copy of the guest. Look at the source first. If it rolled back and runs
the guest, stop this VM. If the source does not run the guest, make sure
it is stopped, then resume with force:
`{"command":"resume","force":true}`. A `stop` here discards what may be
the only copy.

`import-committed` holds the only copy of the guest: the source has
committed and stays paused. It refuses another `migrate-dest`, which
would overwrite that copy. A plain `resume` is safe. A failure after the
commit whose devices would not park again ends `faulted`.

These states, and the stop and cancel handling above, apply only to a
`migrate-dest` sent on the control socket. An import started at boot
with `--migrate-listen` has no control plane yet: a stop does not
cancel it, and a failed import fails the start.

The destination VM should be destroyed by the caller.

## Testing a retry

Two hidden flags add a device that fails a migration on purpose, for a
lab or CI run:

```
--migrate-test-fail-exports N   # the first N exports fail
--migrate-test-fail-imports N   # the first N imports fail
```

Either flag adds the device, under the id `test/migrate-fault`. It
carries no state, and both ends need it, or the device set check
refuses the migration: start the other end with the flag at 0. A
failed export rolls the source back, unless the gate said go. A failed
import ends the destination in `import-failed`, and the source rolls
back. A retry then shows that the guest memory is whole. Propolis has
the same device (`MigrationFailureDevice`).
