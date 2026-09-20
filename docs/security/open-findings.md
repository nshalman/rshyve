# Open security findings

The authoritative register of known, unresolved security issues. Each
entry states the boundary it crosses, the evidence, and what closing it
requires. Resolved entries move to the "Resolved" section with the
commit that closed them.

## Open

### OSF-1 — Live migration has no peer authentication or encryption

**Boundary:** migration peer to VMM. **Severity:** high in any
deployment where the migration path is not already a trusted channel.

`crates/vmm-migrate/src/source.rs` connects and
`crates/vmm-migrate/src/destination.rs` accepts a raw WebSocket over a
TCP or Unix stream with no protocol-level peer identity.
`vmm-migrate-agent` binds `0.0.0.0` and accepts the first connection.

Every field the peer sends is now validated before it is applied, and a
payload the destination cannot hold fails the migration rather than
half-programming a device, so a malformed blob is a refused migration
and not a corrupt guest. That bounds the damage; it is not
authentication.

Guest RAM, CPU state, and device state therefore have no
confidentiality and no peer authentication unless an external control
supplies them.

**To close:** an authenticated, encrypted channel with destination and
source identity verification, session binding to a single VM, migration
ID and expiry, and negative tests for wrong identity, replay, timeout,
truncation, and extra clients. The recommended shape is an
SSH-forwarded Unix socket with pinned host keys, so the VMM never binds
TCP at all.

**Mitigation today:** migrate only across a trusted network, over a
tunnel established and authenticated by other means.

### OSF-1b — vmm-migrate-agent builds privileged shell commands by interpolation

**Boundary:** operator input to root in the global zone. **Severity:**
high where the agent's arguments are not fully trusted.

`bin/vmm-migrate-agent/src/main.rs` interpolates `--uuid`,
`--dest-host`, `--disk-dataset`, `--dest-dataset`, and `--ssh-key` into
command strings and runs them through `bash -c` in `sh()`. The agent is
documented as running in the global zone, and the commands it builds are
`zfs`, `ssh root@`, and `vmadm`, so a metacharacter in any of those
values executes as root. Values beginning with `-` are also treated as
options by the invoked programs, and the remote word is re-parsed by the
destination's login shell.

The agent additionally passes `-o StrictHostKeyChecking=no`, so the
`zfs send` stream carrying guest disk contents is MITM-able.

Separately, `sh()` runs pipelines without `pipefail` and checks a single
exit status, so a truncated `zfs send` is invisible whenever the remote
`zfs recv` exits 0. The agent then migrates RAM onto a short disk.

`check_name` now also refuses a value beginning with `-`, which the
invoked program would read as an option, and one with a `..` path
element, which climbs out of the zone root the agent's paths are built
from. `Cli::Dest` checks its `--uuid` as the source side does. The
remaining gap is the shape of the call, not the character set.

**To close:** replace `sh()` with `std::process::Command` argument
arrays, introduce validated newtypes for UUID, dataset, host, and key
path so clap rejects bad input at parse time, quote the remote word
explicitly, wait on both ends of every pipeline, restore host key
verification, and replace the `echo | socat` control trigger with a
direct Unix-socket client.

**Mitigation today:** treat the agent's arguments as trusted input and
invoke it only from automation that constructs them.

### OSF-2 — Migration does not transfer FPU/XSAVE state

**Boundary:** correctness, guest-visible. **Severity:** high.

`crates/vmm-migrate/src/state.rs` does not export or import FPU/XSAVE
state. A guest using x87, SSE, or AVX state across a migration can
observe corrupted registers.

The comment in `state.rs` claiming a `VM_GET_FPU` ioctl is needed is
wrong: the bindings already exist in `crates/bhyve-api/sys/src/ioctls.rs`
and `structs.rs`.

**To close:** implement `get_fpu`/`set_fpu`/`fpu_area_desc`, reject a
blob whose length does not match the local area size before the ioctl,
and validate across a real cross-host migration with a continuous AVX
workload. A malformed XSAVE blob can set reserved `XCOMP_BV` bits and
fault the guest at its first `XRSTOR`, so this is worse to get wrong
than to leave unimplemented.

### OSF-4 — Vendored libtpms predates two CVE fixes

**Boundary:** guest to host, via the vTPM command interface.
**Severity:** high (CVSS 8.5 and 8.3 upstream).

`third_party/libtpms` is pinned at upstream `521c5107`, which precedes:

- **CVE-2026-6726** — `ObjectFlush()` clears only the `occupied`
  attribute and never zeroes the freed OBJECT, leaving key material in
  the slot.
- **CVE-2026-6727** — `OaepDecode()` retains early-exit comparisons,
  giving a timing side channel reachable from guest-issued
  `TPM2_RSA_Decrypt`.

See CERT VU#431093 and `THIRD_PARTY.md`.

**To close:** backport both upstream fixes onto the pinned revision, or
re-vendor. A re-vendor must be validated by resuming an existing vTPM
state blob, because `NVMarshal.c` is the state serializer and vTPM state
is the root of a BitLocker guest's disk encryption.

### OSF-5 — PCI passthrough is not audited

**Boundary:** guest to host. **Severity:** unknown, potentially high.

`crates/vmm-devices/src/passthru.rs` exposes physical device
configuration space indexed by guest-controlled offsets, and contains
the largest concentration of `unsafe` in the tree. It has not been
systematically reviewed.

**To close:** a dedicated audit of the guest-offset arithmetic and the
BAR mapping paths, plus a decision on whether to gate the device behind
a non-default cargo feature for the initial release.

### OSF-6 — Operational errors are discarded on non-migration paths

**Boundary:** operational correctness. **Severity:** low to medium.

Roughly 120 `let _ = ...` statements remain outside the migration page
paths, some on device state and interrupt delivery. Each needs to be
classified as either genuinely idempotent best-effort cleanup or a real
error that should propagate.

**To close:** classify every site, propagate or deliberately log the
rest, and add fault-injection tests for guest-memory, interrupt, queue,
and device-state failures.

### OSF-7 — VNC console authentication is legacy and unencrypted

**Boundary:** local. **Severity:** low.

The VNC server uses the RFB DES-based challenge, which is not a
meaningful authentication mechanism by modern standards, and provides no
transport encryption. The socket is Unix-domain and now bound
owner-only, so the exposure is limited to a local user who is already
the VMM's own uid.

The control socket half of this finding is closed: it now has peer
credential authorization, a bounded read, deadlines, and concurrent
accept.

**To close:** a modern authenticated, encrypted console boundary for any
remote use, and a documented tunnel requirement until then.

## Resolved

| Finding | Closed by |
|---|---|
| OSF-3 — CPU feature mismatch was warned about, not rejected, and the two implementations compared different leaves against unmasked host features | `Migrate device state by PCI identity over one driver` |
| Hyper-V enlightenment state was not migrated; `TIME_REF_COUNT` restarted from a process-local clock and ran backwards for the guest | `Migrate the Hyper-V enlightenment` |
| PS/2 controller RAM out-of-bounds index reachable from guest port I/O | `fix(ps2): guest-reachable out-of-bounds index in controller RAM` |
| Virtio indirect descriptor table unbounded against queue size | `fix(virtio): bound indirect descriptor tables to the queue size` |
| Migration `PageBatch` slice index abort and discarded guest-memory errors | `fix(migrate): validate and bound peer-controlled page transfers` |
| Wide guest access to an 8-bit port aborts the VMM; unbounded MMIO and PIO widths | `fix: wide guest accesses to narrow ports and buses abort the VMM` |
| virtio-blk accumulated requests without bound across the EVENT_IDX retry loop | `fix(virtio-blk): bound requests collected per notification` |
| virtio EVENT_IDX re-check loop spins forever holding the device lock when the available-ring entry is unmapped | `fix(virtio): guest-triggerable hang in the EVENT_IDX re-check loop` |
| NVMe queue-size arithmetic overflowed on guest input | `fix(nvme): compute guest-supplied queue sizes in u32` |
| Control socket unbounded requests, no deadlines, no peer authorization, serial accept | `fix: restrict and authorize every Unix socket the VMM exposes` |
| VNC and mdata sockets bound with no permission protection | same commit |
