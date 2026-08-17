# Security policy

## Status

This is experimental software. It is not production-supported, and it
has not completed a security qualification. Do not run untrusted guests
on it.

## What this project defends

`vmm` is a hypervisor. The security boundary that matters is **guest to
host**: a guest must not be able to read or write host memory outside
its own address space, execute code in the VMM process, escape its zone,
or crash the VMM.

The release profile sets `panic = "abort"`, so **any panic reachable
from guest-controlled input is a denial of service against the guest and
is treated as a security bug**, not merely a robustness bug. Guest input
includes port I/O, MMIO, descriptor rings, and every device command
queue.

Secondary boundaries, in decreasing order of hardening:

| Boundary | Current state |
|---|---|
| Guest to host | The primary boundary. Report anything that crosses it. |
| Control socket to VMM | Unix socket, owner-only, with peer credential authorization, bounded requests, and deadlines. Local root is already trusted. |
| Migration peer to VMM | **Unauthenticated and unencrypted.** See below. |
| VNC client to VMM | Local Unix socket, owner-only. Legacy DES challenge auth, no transport encryption. |

## Known gaps

These are documented rather than hidden. They are not eligible for
disclosure credit, but reports that deepen them are welcome.

- **Live migration has no peer authentication or transport encryption.**
  Guest RAM, CPU state, and device state cross the wire in the clear,
  and the destination accepts the first connection it receives. Migrate
  only over a trusted, separately authenticated channel.
- **Live migration does not transfer FPU/XSAVE state.** A guest using
  floating point or vector state across a migration may observe
  corruption.
- **CPU feature compatibility is warned about, not enforced.** A
  migration to a destination lacking a feature the guest is using will
  be attempted.
- **The vendored libtpms predates the fixes for CVE-2026-6726 and
  CVE-2026-6727.** See `THIRD_PARTY.md`.
- **PCI passthrough** exposes physical device configuration space to
  guest-controlled offsets and has not been fully audited.

`docs/security/open-findings.md` tracks the current register.

## Reporting a vulnerability

Report privately. Do not open a public issue for a suspected
vulnerability.

Use GitHub's private vulnerability reporting on this repository
(Security tab, "Report a vulnerability"). It creates a private advisory
visible only to the maintainers.

Please include:

- the affected revision,
- the guest or peer input that triggers it,
- what boundary you believe it crosses,
- a reproducer if you have one.

## Response

- Acknowledgement within 5 business days.
- An initial assessment, with a severity and a fix target, within 15
  business days.
- Default embargo of 90 days from acknowledgement, extendable by
  agreement if a fix needs coordination with upstream projects.

Some of this code is derived from
[Propolis](https://github.com/oxidecomputer/propolis). A defect that
also exists upstream will be coordinated with Oxide Computer Company
before public disclosure.

## Supported versions

No release has been published. Until one is, only the `master` branch
receives fixes.
