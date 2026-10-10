# Cutover gate and MantaBlock ring disks

This page tells an orchestrator (tritonagent and tritond) how to move a
VM with MantaBlock ring disks. The design is spec 61 in manta-storage.

## Summary

1. Start the destination with the source's `-s` list from
   `migrate-config`. Each ring disk then has `ring-shape=`. The
   destination makes the disk with that shape and does not attach it.
2. Start `migrate-source` with a `gate` socket.
3. The source pauses, drains every ring request, flushes, and asks the
   gate. Send `GO` or `ABORT`.
4. After `GO`, the source sends the last state and commits. Right after
   the commit it detaches its ring disks.
5. Move each volume to the destination CN (spec 61 step 6). The engine
   on the destination then opens the volume socket.
6. The destination attaches each ring disk, then resumes the guest.

## Control socket requests

All requests are one JSON line on the VM control socket.

### Source

```json
{"command":"migrate-source",
 "target_addr":"/var/run/rshyve/migrate.sock",
 "gate":"/var/run/rshyve/gate.sock",
 "gate_timeout_ms":10000,
 "zfs_barrier":null,
 "allow_engine_without_migrate_v1":false}
```

| Field | Meaning |
|---|---|
| `target_addr` | A Unix socket path the agent listens on and bridges to the peer, or `host:port` |
| `gate` | A Unix socket path the agent listens on. Absolute. Optional |
| `gate_timeout_ms` | Wait for `GO` or `ABORT`. Default 10000. Range 1 to 25000 |
| `zfs_barrier` | As before. The source waits on it before the gate |
| `allow_engine_without_migrate_v1` | Lab only. See "Engine check" |

### Destination

Start rshyve with `--migrate-listen <path>`. With a path, rshyve
connects to a Unix socket that the agent listens on. Two more flags:

| Flag | Meaning |
|---|---|
| `--migrate-ring-attach-timeout-ms` | Wait for each ring disk to attach after the commit. Default and maximum 120000, so it ends before the source stops waiting for the start (150 s) |
| `--migrate-allow-unproven-engine` | Lab only. See "Engine check" |

The shape is
`ring-shape=1:<blocks>:<sector>:<queues>:<depth>:<buf_pages>:<tags>:<part_blocks>:<flags>`,
with flags `d` (durable), `z` (zeroing), `dz` or `-`. Copy it as
`migrate-config` gives it. The destination asks the engine for the same
queues, depth and buffer, and refuses any other grant.

`ring-shape=` is refused on a VM started without `--migrate-listen`.
The control request `migrate-dest` refuses a VM with a ring disk: a VM
that already runs attached its disks at boot, and the source still owns
those volumes. `migrate-dest` takes a path `listen_addr` for other VMs,
and then connects to it as `--migrate-listen` does.

### The ring config file

The bhyve brand caps `bhyve-extra-opts` at about 250 bytes, and one ring
disk with its shape takes about 240. So the agent puts the ring disks
and the destination flags in a file in the zone root, and gives rshyve
only `--ring-config <path>`:

```json
{"version":1,
 "disks":["7,nvme,mantablock=/mb/<v>.sock,volume=<v>,ring-shape=1:..."],
 "migrate":{"ring_attach_timeout_ms":120000,"allow_unproven_engine":false}}
```

| Field | Meaning |
|---|---|
| `version` | Must be 1. rshyve refuses any other version and any unknown field |
| `disks` | `-s` specs, added after the command line's. Only `nvme,mantablock=` disks, at most 24 |
| `migrate.ring_attach_timeout_ms` | As `--migrate-ring-attach-timeout-ms`. Range 1 to 120000 |
| `migrate.allow_unproven_engine` | Only `false` is taken. The zone can write this file, so the lab flag is `--migrate-allow-unproven-engine` only |

The path must be absolute and not a symbolic link. The file must be a
regular file of rshyve's user (root), mode 0600 or tighter, and at most
64 KiB. rshyve reads it at each start, also after a guest reset.

Without `--migrate-listen`, rshyve removes `ring-shape=` from the file's
disks, so a guest reset on the destination before the agent rewrites
the file boots normally. A `ring-shape=` on the command line is still
refused, as above.

### Source order

```json
{"command":"migrate-resume-source"}
```

This is the `source` decision of spec 61 §6.4. Send it only after the
VM migration record is `source`. rshyve takes it only from a source
that waits for an order:

- It got `GO` and then failed before its commit. It resumes with its
  ring disks as they are.
- It committed and did not hear the destination start the guest. It
  attaches each ring disk again (a fresh Attach), then resumes.

rshyve refuses it while the migration thread runs, and after the
destination confirmed the start (the source is then `stopped`).

### Cancel

```json
{"command":"migrate-cancel"}
```

Stops a source migration before the gate's go. rshyve closes the
migration stream, the gate socket and the ZFS barrier socket at once.
The migration fails with `the migration was cancelled`:

- Before the pause, the guest never stopped and keeps running.
- After the pause and before `GO`, rshyve rolls back and the guest runs
  again.

The destination sees the stream close and exits. Its guest never ran.

rshyve refuses `migrate-cancel`:

- When no source migration runs.
- After the gate's go (`migrate_gate` is `go`), or after the commit.
  Then use `migrate-resume-source` or a stop.

A `GO` that arrives after the cancel is taken as an abort, and rshyve
answers `ACK ABORT`.

### Status

`migrate-status` has these more fields:

| Field | Values |
|---|---|
| `migrate_gate` | `none`, `waiting`, `go`, `abort` |
| `migrate_rings` | `attached`, `attaching`, `detached`, `attach-failed`, `detach-failed`. Absent for a VM with no ring disk |
| `migrate_error` | Why the migration failed. Absent until it fails |
| `migrate_clock_skew_ms` | Destination only: how far its clock is ahead of the source's. Negative when behind |

`migrate_phase` `committed` means the source committed. A source that
fails before its commit shows `migrate_phase` `error`. A source that
fails after its commit keeps `committed`.

## Gate exchange

The agent listens on the `gate` socket. rshyve connects once per
migration. Each line is ASCII, ends with `\n`, and is at most 64 bytes.

```text
rshyve -> agent   READY rshyve-gate/1
agent  -> rshyve  GO          or  ABORT
rshyve -> agent   ACK GO      or  ACK ABORT
```

When rshyve sends `READY`:

- The vCPUs are paused.
- NVMe admission is closed, and every ring request has a completion in
  the guest completion queue or in the held completions. Every write the
  guest saw complete is durable on the volume.
- Each disk is flushed, and the ZFS barrier (if any) answered.

Rules:

1. Only the exact line `GO` is a go. Any other line, an empty line, a
   closed socket, or no line within `gate_timeout_ms` is an abort.
2. On an abort rshyve resumes the guest on the source. The migration
   fails. Nothing on the destination ran.
3. rshyve records the go before it writes `ACK GO`. From then on it
   does not resume the guest by itself. A failure keeps the guest
   paused until `migrate-resume-source` or a stop. The exception is
   the cutover deadline: with no commit 60 s after the go, rshyve sets
   `migrate_gate` to `abort`, fails the migration and resumes the
   guest. The destination cannot start without the commit.
4. `GO` with no `ACK GO` is not an abort. Read `migrate_gate` to know.
5. A stop of the VM closes the gate socket. Take that as an abort.

Write the `committing` state of the VM migration record before you send
`GO` (spec 61 §5 step 4).

## Timeouts

| What | Default | Where |
|---|---|---|
| Gate connect and answer | 10 s, max 25 s | `gate_timeout_ms` |
| From the go to the source's commit | 60 s | fixed |
| Source waits for the destination start after the commit | 150 s | fixed |
| Destination waits for each ring disk to attach | 120 s | `--migrate-ring-attach-timeout-ms` |
| Each Hello and Attach try | 5 s | fixed |
| One source send that makes no progress | 30 s | fixed |

A source send can block when the peer stops reading, for example when
the destination died and the agent's bridge kept the socket open. After
30 s with no progress, rshyve closes the stream and the migration
fails. Before the pause the guest keeps running.

The destination retries the attach (20 ms, doubling to 500 ms) while the
volume socket does not exist or the engine refuses. A grant with
another shape stops the retries.

## After the commit

- The source sends Detach on each ring disk and waits for Detached.
  `migrate_rings` becomes `detached`, or `detach-failed` when the engine
  did not confirm. The engine ends the attachment when the socket closes
  in both cases.
- The destination attaches each ring disk with a fresh Attach and a new
  generation. It checks that the grant has the shape the guest saw. No
  device resumes before every ring disk attached. Then the vCPUs start,
  and the destination tells the source it started.
- If an attach fails, the destination process exits and the guest
  never runs there. The source still holds the paused guest: set the
  record to `source` and send `migrate-resume-source`.

## Engine check

rshyve offers ring Hello feature bit `1 << 3` (`migrate-v1`). An engine
that sets it promises that a fresh Attach takes a generation above every
generation the volume used on any CN (spec 61 E1). Without it, a fresh
Attach on the destination can reuse an op id, and the engine can drop a
write as a duplicate.

- The source refuses a ring disk on an engine without `migrate-v1`.
- The destination refuses an attach from an engine without it.
- `allow_engine_without_migrate_v1` and
  `--migrate-allow-unproven-engine` turn both checks off. Use them in a
  lab only.

## Stream version

The migration stream is `vmm-migrate-ron/10`, which adds the source
wall clock, the interrupt shadow, the AMD PMU state and the
fault-injection test device to v9. A source with a ring disk refuses a
v6 destination right after the protocol choice, before the pause.
Every VM has the COM1 and COM2 UARTs, so every VM needs v9 or newer at
both ends. An older destination does not start with a ring
disk on its `-s` list.
