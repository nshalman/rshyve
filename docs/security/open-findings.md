# Open security findings

The authoritative register of known, unresolved security issues. Each
entry states the boundary it crosses, the evidence, and what closing it
requires. Resolved entries move to the "Resolved" section with the
commit that closed them.

## Open

### OSF-1 — Live migration has no peer authentication or encryption

**Boundary:** migration peer to VMM. **Severity:** high in any
deployment where the migration path is not already a trusted channel.

The source connects to `ws://migration/live` in the clear
(`crates/vmm-migrate/src/source.rs`), the destination accepts a raw
WebSocket (`crates/vmm-migrate/src/destination.rs`), and the sync paths
accept TCP or Unix streams with no protocol-level peer identity.
`vmm-migrate-agent` binds `0.0.0.0` and accepts the first connection.

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

### OSF-3 — CPU feature mismatch is warned about, not rejected

**Boundary:** correctness, guest-visible. **Severity:** medium.

Both destinations log a CPU feature mismatch and continue. The two
implementations also compare differently: the sync path, which is the
one production uses, checks only CPUID leaf 7 and has no ignore masks
for SGX/MPX, while the async path checks leaf 1, leaf 7, and xcr0.

The comparison itself is also wrong in a way that produces false
negatives: the source sends baseline-masked guest features while the
destination compares against raw host features, so a migration from
`--cpu-baseline host` to a narrower baseline reports a passing check and
then hands a running guest a feature table with bits cleared.

**To close:** one shared comparison over the full effective guest CPUID
contract, then a policy decision on reject-by-default with an explicit,
auditable override.

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

### OSF-8 — Hyper-V enlightenment state does not cross a migration

**Boundary:** correctness, guest-visible. **Severity:** high for a
`--hyperv` Windows guest. **Status:** pending verification.

`crates/vmm-hyperv/src/lib.rs` keeps every piece of enlightenment state
in one `Mutex<Inner>` built zeroed, and derives reference time from an
`Instant` taken when the instance was constructed.
`crates/vmm-migrate/src/codec.rs` carries no Hyper-V payload, and
`bin/rshyve/src/main.rs` builds a fresh instance on the destination. So a
nominally successful migration restarts `TIME_REF_COUNT` at zero,
violating the monotonicity the guest is promised, and loses
`HV_X64_MSR_GUEST_OS_ID`, `HV_X64_MSR_HYPERCALL`,
`HV_X64_MSR_REFERENCE_TSC` and the five guest-readable crash parameters.
The overlay pages themselves travel inside guest RAM, so the guest keeps
pages whose describing MSRs the destination has forgotten.

`--hyperv` is not refused for migration, so nothing warns.

**To close:** export and import the three configuration MSRs, the crash
parameters, and a reference-time offset rather than a raw `Instant`;
remove the overlays before the final RAM copy and reinstall them after
import, propagating any failure; then migrate a live Windows guest under
a continuous time-reading workload and confirm `TIME_REF_COUNT` does not
go backwards. Until then, refuse migration for a VM started with
`--hyperv`.

**Mitigation today:** do not migrate a `--hyperv` guest.

### OSF-9 — A guest can flood the host log on several device paths

**Boundary:** guest to host. **Severity:** medium (host disk and log
pipeline, and evidence destruction). **Status:** pending verification.

Several device paths write one log record per guest action with no rate
limit and no latch, so a guest that repeats the action writes to the host
log as fast as it can execute: pvpanic writes, COM2 bytes the guest does
not drain, and refused viona kicks and ring inits are the ones found.
Each is being fixed where it was found. This entry records the class,
because the tree has no shared answer to it: no rate-limiting drain, no
"log once per state change" helper, and no test convention, so the next
device to log on a guest-driven path reintroduces it.

The logger's async drain is a 128-entry channel with `DropAndReport`, so
a flood also evicts the records that would explain an unrelated failure.

**To close:** one rate-limited or latched logging seam for guest-driven
events, used by every device that logs on such a path, and a
`CountingDrain` test per device asserting that N guest actions produce a
bounded number of records. Then re-audit every `warn!` and `info!`
reachable from a guest action.

### OSF-10 — virtio-fs runs in the VMM process, not a sandbox

**Boundary:** guest to host filesystem. **Severity:** medium.
**Status:** pending verification.

`crates/vmm-virtio/src/fs/passthrough.rs` serves FUSE requests from the
VMM process itself. The isolation is the shared root fd, `*at()` calls
with `O_NOFOLLOW`, name validation at LOOKUP, and refusal of device,
FIFO and socket nodes. Caps on cached inodes and open handles are being
added so a guest cannot pin host descriptors without bound.

Those caps are a mitigation, not the boundary. virtiofsd runs the same
work in a separate process under a namespace or `pivot_root`, so a defect
in path handling costs an attacker a sandbox rather than the VMM's whole
address space and every fd in it. illumos has no seccomp and no mount
namespaces, so the equivalent needs its own design.

**To close:** decide whether virtio-fs ships in the VMM process at all.
If it does, the argument for it needs writing down, together with a
negative test set for every path-handling rule the module claims. If it
does not, a helper process with a reduced privilege set and its own
`root_fd`, talking to the device over a socket.

**Mitigation today:** export only directories whose contents the guest
may read, and never a read-write share of anything the host trusts.

### OSF-11 — The PCI passthrough audit has not run on real hardware

**Boundary:** guest to host. **Severity:** unknown.
**Status:** pending verification.

OSF-5 records that passthrough was unaudited. A source audit of the
guest-offset arithmetic and the BAR mapping paths closes the reading part
of that. What no audit can supply is a device: every conclusion about
passthrough in this tree has been reached by reading code and the PPT
ioctl contract, never by assigning a physical function to a guest.

Specifically unproven: that the config-space windows the guest sees match
what the audit concluded, that BAR mapping and unmapping survive a guest
reset with DMA in flight, that the MSI-X refusal covers every device that
advertises it, and that illumos VT-d remapping behaves as the module
header assumes. There is no AMD IOMMU backend at all.

**To close:** run a real assigned device: a NIC and a storage controller,
each through guest boot, reset, reboot and VM destroy, with DMA active,
on an Intel VT-d host. Record what was assigned and what was observed in
`docs/hardware-run.md`. Until then treat `passthru` as experimental
whatever the source audit says.

**Mitigation today:** do not offer `-s N,passthru` to an untrusted guest.

## Resolved

| Finding | Closed by |
|---|---|
| PS/2 controller RAM out-of-bounds index reachable from guest port I/O | `fix(ps2): guest-reachable out-of-bounds index in controller RAM` |
| Virtio indirect descriptor table unbounded against queue size | `fix(virtio): bound indirect descriptor tables to the queue size` |
| Migration `PageBatch` slice index abort and discarded guest-memory errors | `fix(migrate): validate and bound peer-controlled page transfers` |
| Wide guest access to an 8-bit port aborts the VMM; unbounded MMIO and PIO widths | `fix: wide guest accesses to narrow ports and buses abort the VMM` |
| virtio-blk accumulated requests without bound across the EVENT_IDX retry loop | `fix(virtio-blk): bound requests collected per notification` |
| virtio EVENT_IDX re-check loop spins forever holding the device lock when the available-ring entry is unmapped | `fix(virtio): guest-triggerable hang in the EVENT_IDX re-check loop` |
| NVMe queue-size arithmetic overflowed on guest input | `fix(nvme): compute guest-supplied queue sizes in u32` |
| Control socket unbounded requests, no deadlines, no peer authorization, serial accept | `fix: restrict and authorize every Unix socket the VMM exposes` |
| VNC and mdata sockets bound with no permission protection | same commit |
