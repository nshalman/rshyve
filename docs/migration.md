# Live Migration

## Overview

Live migration transfers a running VM from one compute node to another
with minimal downtime. The protocol uses WebSocket over TCP with RON
serialization for structured data.

## Protocol Phases

```
Source                          Destination
  |                                 |
  |  1. Sync (WebSocket + protocol negotiation)
  |-------- ProtocolOffer --------->|
  |<------- ProtocolSelect --------|
  |-------- Preamble ------------->|
  |<------- Okay ------------------|  (CPU/memory/feature check)
  |                                 |
  |  2. RamPushPrePause (VM still running)
  |-------- MemOffer (all pages) ->|
  |<------- MemFetch --------------|
  |-------- MemXfer + Pages ------>|
  |                                 |
  |  3. Pause
  |  (source pauses all vCPUs)      |
  |                                 |
  |  4. RamPushPostPause (final dirty pages)
  |-------- MemOffer (dirty) ----->|
  |<------- MemFetch --------------|
  |-------- MemXfer + Pages ------>|
  |                                 |
  |  5. TimeData (TSC + boot_hrtime adjustment)
  |-------- TimeData ------------->|
  |                                 |
  |  6. DeviceState (kernel + userspace)
  |-------- DeviceState ---------->|
  |                                 |  (pause, import, resume)
  |  7. Finish                      |
  |<------- Okay ------------------|
  |-------- Okay ----------------->|
```

## CPU Feature Compatibility

The destination checks that its CPU supports all features the source
exposes to the guest. If features are missing, migration is rejected
with a detailed error listing the missing feature bits.

```
CPU feature mismatch: source requires features not available on
destination (leaf7_ebx=0xf1238040 ... xcr0=0x2e0)
```

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

### Time Adjustment

TSC and boot_hrtime are adjusted using the Propolis pattern:

```
guest_uptime = src.hrtime - src.boot_hrtime
migrate_delta = dst.wall_clock - src.wall_clock
new_boot_hrtime = dst.hrtime - (guest_uptime + migrate_delta)
```

If TSC frequencies differ between hosts, the guest TSC is scaled
proportionally and the destination's `guest_freq` is used.

## Usage

### Source Side

```bash
vmm ... --track-dirty --control-socket /tmp/vm.sock myvm

# Trigger migration
echo '{"command":"migrate-source","target_addr":"10.0.0.2:4567"}' \
  | socat - UNIX-CONNECT:/tmp/vm.sock
```

### Destination Side

```bash
vmm ... --track-dirty --migrate-listen 0.0.0.0:4567 myvm
```

The destination creates the VM with RAM and devices but does not start
vCPUs. It waits for the source to connect and transfer state.

### Monitor Progress

```bash
echo '{"command":"migrate-status"}' | socat - UNIX-CONNECT:/tmp/vm.sock
# {"success":true,"state":"migrating","migrate_phase":"RamPushPrePause",...}
```

## Error Recovery

If migration fails after the source VM has been paused, the source
automatically resumes the VM so the guest continues running. The
destination VM should be destroyed by the caller.
