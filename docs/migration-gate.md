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
| `--migrate-ring-attach-timeout-ms` | Wait for each ring disk to attach after the commit. Default 120000 |
| `--migrate-allow-unproven-engine` | Lab only. See "Engine check" |

A paused VM can also take an import with `migrate-dest`. A
`listen_addr` that is a path works the same way: rshyve connects to it.

```json
{"command":"migrate-dest",
 "listen_addr":"/var/run/rshyve/migrate.sock",
 "ring_attach_timeout_ms":120000,
 "allow_engine_without_migrate_v1":false}
```

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

### Status

`migrate-status` has two more fields:

| Field | Values |
|---|---|
| `migrate_gate` | `none`, `waiting`, `go`, `abort` |
| `migrate_rings` | `attached`, `attaching`, `detached`, `attach-failed`, `detach-failed`. Absent for a VM with no ring disk |

`migrate_phase` `committed` means the source committed.

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
   never resumes the guest by itself. A failure keeps the guest paused
   until `migrate-resume-source` or a stop.
4. `GO` with no `ACK GO` is not an abort. Read `migrate_gate` to know.
5. A stop of the VM closes the gate socket. Take that as an abort.

Write the `committing` state of the VM migration record before you send
`GO` (spec 61 §5 step 4).

## Timeouts

| What | Default | Where |
|---|---|---|
| Gate answer | 10 s, max 25 s | `gate_timeout_ms` |
| Source waits for the destination start after the commit | 150 s | fixed |
| Destination waits for each ring disk to attach | 120 s | `--migrate-ring-attach-timeout-ms` |
| Each Hello and Attach try | 5 s | fixed |

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
- If an attach fails, the destination keeps the guest paused. With
  `--migrate-listen` the process exits; the source still holds the
  paused guest. With `migrate-dest` the VM is `import-committed`, and a
  `resume` tries the attach again first.

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

The migration stream is `vmm-migrate-ron/7`. Its payload is the v4
payload. A source sends a ring disk only to a v7 destination. An older
destination does not start with a ring disk on its `-s` list.
