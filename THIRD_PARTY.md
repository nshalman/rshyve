# Third-party provenance

Every copied, forked, vendored, or redistributed component in this
repository, with its upstream revision, license, and local changes.
`NOTICE` carries the attribution text required for redistribution; this
file carries the detail needed to audit or refresh a component.

## Derived source: Propolis

| | |
|---|---|
| Upstream | https://github.com/oxidecomputer/propolis |
| License | MPL-2.0 |
| Copyright | Oxide Computer Company |
| Form | Source copied and adapted into this tree |

**This project would not exist in its current form without Propolis.**
The entire bhyve and viona kernel interface, the address-space manager,
the PS/2 device models, and parts of the PCI, UART, and VM-exit layers
originate there. About 3,500 significant lines are copied or derived, across 19 files.

Similarity below is measured with ordered sequence comparison over
non-trivial lines, so it reflects real derivation rather than shared
Rust boilerplate. Every file listed carries an attribution header naming
its upstream counterpart.

Copied essentially verbatim:

| File | Upstream |
|---|---|
| `crates/bhyve-api/sys/src/vmm_data.rs` | `crates/bhyve-api/sys/src/vmm_data.rs` |
| `crates/bhyve-api/sys/src/enums.rs` | `crates/bhyve-api/sys/src/enums.rs` |
| `crates/bhyve-api/sys/src/ioctls.rs` | `crates/bhyve-api/sys/src/ioctls.rs` |
| `crates/bhyve-api/src/lib.rs` | `crates/bhyve-api/src/lib.rs` |
| `crates/viona-api/src/lib.rs` | `crates/viona-api/src/lib.rs` |
| `crates/viona-api/src/ffi.rs` | `crates/viona-api/src/ffi.rs` |
| `crates/dladm/src/sys.rs` | `crates/dladm/src/sys.rs` |
| `crates/vmm-core/src/aspace.rs` | `lib/propolis/src/util/aspace.rs` |
| `crates/vmm-hid/src/ps2/keysym.rs` | `crates/rfb/src/keysym.rs` |
| `crates/vmm-hid/src/ps2/keyboard/mod.rs` | `lib/propolis/src/hw/ps2/keyboard/mod.rs` |
| `crates/vmm-hid/src/ps2/keyboard/scan_code_1.rs` | same path upstream |
| `crates/vmm-hid/src/ps2/keyboard/scan_code_2.rs` | same path upstream |

Substantially derived:

| File | Similarity | Upstream |
|---|---|---|
| `crates/bhyve-api/sys/src/structs.rs` | 93% | `crates/bhyve-api/sys/src/structs.rs` |
| `crates/vmm-devices/src/migrate.rs` | 79% | `lib/propolis/src/migrate.rs` |

Partly derived:

| File | Similarity | Upstream |
|---|---|---|
| `crates/vmm-devices/src/pci/bits.rs` | 53% | `lib/propolis/src/hw/pci/bits.rs` |
| `crates/vmm-core/src/exits.rs` | 48% | `lib/propolis/src/exits.rs` |
| `crates/vmm-hid/src/ps2/ctrl.rs` | 46% | `lib/propolis/src/hw/ps2/ctrl.rs` |
| `crates/vmm-hid/src/ps2/kbd.rs` | split | `lib/propolis/src/hw/ps2/ctrl.rs` (`PS2Kbd`) |
| `crates/vmm-hid/src/ps2/mouse.rs` | split | `lib/propolis/src/hw/ps2/ctrl.rs` (`PS2Mouse`) |
| `crates/vmm-devices/src/pci/bar.rs` | 41% | `lib/propolis/src/hw/pci/bar.rs` |
| `crates/vmm-core/src/intr_pins.rs` | 39% | `lib/propolis/src/intr_pins.rs` |
| `crates/vmm-devices/src/uart/uart16550.rs` | 36% | `lib/propolis/src/hw/uart/uart16550.rs` |

`kbd.rs` and `mouse.rs` hold the `PS2Kbd` and `PS2Mouse` models, which
upstream keeps inside `ctrl.rs`. Splitting them across three files puts
each below the 30% per-file reporting threshold, so the tool does not
list them separately. They are derived all the same.

Reproduce the whole table with:

```sh
tools/propolis-derivation.py /path/to/propolis
```

Scores move as upstream moves, so record the Propolis revision measured
whenever this table is regenerated.

### Licensing

Both projects are MPL-2.0, so the licenses are compatible and the copy
is licensed. MPL-2.0 section 3.4 notice preservation is satisfied: every
derived file retains its MPL Exhibit A header, and now also names its
upstream source.

Only `keysym.rs` carries an Oxide copyright line, because it is the only
one that had one upstream. Propolis states a copyright on 13 of its 317
Rust files; the others we copied had none to preserve. Nothing was
stripped.

### Shared defects

Three defects fixed in this tree exist verbatim in upstream Propolis, in
`lib/propolis/src/hw/ps2/ctrl.rs`:

- controller RAM sized from an exclusive subtraction over an inclusive
  command range (upstream `:294-295`),
- the RAM write path taking its offset from the data byte instead of the
  command prefix (upstream `:436-438`),
- `assert_eq!(rwo.len(), 1)` in the port I/O handler, which a wide guest
  access trips (upstream `:402`).

Upstream also builds release with `panic = "abort"`, so the impact there
is the same: an unprivileged guest can abort the VMM. Coordinate
disclosure with Oxide before publishing this repository.

## Submodule: libtpms

| | |
|---|---|
| Upstream | https://github.com/stefanberger/libtpms |
| Revision | `521c51073fe6f7c56023db78e56961fcaf7906e8` (untagged `master`, 2026-03-13) |
| License | BSD-3-Clause |
| Copyright | IBM Corporation 2006-2011; Microsoft Corporation 2010-2022; Trusted Computing Group and contributors 2022-2025 |
| Location | `third_party/libtpms` (submodule, pinned to the revision above) |
| License text | `LICENSES/BSD-3-Clause-libtpms.txt` (source not redistributed here) |
| Local changes | `third_party/libtpms-illumos/patches/`, applied at build time |
| Linkage | Static, via `crates/vmm-tpm-sys/build.rs` |

The submodule is pristine upstream, so there is no vendored copy to
audit. Confirm the pin and that the patches still apply:

```sh
git -C third_party/libtpms rev-parse HEAD    # must match the revision above
git submodule status third_party/libtpms     # a leading '+' means drift
```

`crates/vmm-tpm-sys/build.rs` copies the submodule into `OUT_DIR` and
applies every patch there, failing the build if one does not apply. The
submodule is never modified, so the patch set below is the complete and
mechanically enforced statement of what this project changes.

`PORTING_NOTES.md` previously recorded the source as "v0.11.0". No such
upstream tag exists; 0.11.0 is the in-development version string in
`configure.ac`. The revision above was recovered by exhaustive diff and
is authoritative.

### Local patches

Both live in `third_party/libtpms-illumos/patches/` and are applied to
the `OUT_DIR` copy by `build.rs`:

1. `0001-illumos-strchr.patch` — `src/tpm2/RuntimeProfile.c` uses
   `strchr()` instead of the legacy `index()`, which illumos does not
   declare under the default feature-test macros.
2. `0002-illumos-endian.patch` — adds a `__sun` / `__illumos__` arm to
   the endianness cascade in `TpmProfile_Common.h`.

### License exception inside the vendored tree

`m4/ax_check_linker_flag.m4` inside the libtpms submodule is
GPL-2.0-or-later
with the Autoconf Macro exception (Copyright 2008 Guido U. Draheim,
2011 Maarten Bosmans). The exception means it does not affect the
license of the build output. It is the only GPL-licensed file in the
tree and is called out here so a license scan of this repository has an
explanation to match against. It reaches a build host through the
submodule, not through this repository's own tracked files.

### Outstanding security exposure

The pinned revision predates two upstream CVE fixes. Both are present
and unpatched in the vendored tree:

| CVE | Upstream fix | Vendored state |
|---|---|---|
| CVE-2026-6726 (CVSS 8.5) | `ef2762830558` "Sync: Clear OBJECT on ObjectFlush" | `src/tpm2/TPMCmd/tpm/src/subsystem/Object.c` `ObjectFlush()` clears only the `occupied` attribute and never zeroes the freed OBJECT, leaving key material in the slot |
| CVE-2026-6727 (CVSS 8.3) | `0356d4339bcb` "tpm2: Improve OaepDecode constant-timeness" | `src/tpm2/TPMCmd/tpm/src/crypt/CryptRsa.c` `OaepDecode()` retains the early-exit comparisons, reachable from guest-issued `TPM2_RSA_Decrypt` |

See CERT VU#431093. Both are tracked in `docs/security/open-findings.md`
and must be resolved before the vTPM is presented as production-ready.

## Referenced specification: VIRTIO 1.3

| | |
|---|---|
| Upstream | https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html |
| Copyright | OASIS Open 2023. All Rights Reserved. |

Not redistributed. The virtio device models are written against this
specification and cite it by section number.

## Fetched at build time: edk2-bhyve firmware

| | |
|---|---|
| Upstream | FreeBSD edk2-bhyve package |
| License | BSD-3-Clause, text at `LICENSES/BSD-3-Clause-edk2.txt` |
| Fetcher | `tools/fetch-edk2-bhyve.sh` |

Not vendored. The fetch script pins the package URL and verifies
SHA-256 and size for both the package and the extracted `CODE`/`VARS`
images before installing them. `docs/firmware-edk2-bhyve.md` records the
provenance table.

## Cargo dependencies

The dependency graph is pinned by `Cargo.lock`. Every package in it
declares an SPDX license expression; there is no GPL or AGPL. The spread
is predominantly MIT/Apache-2.0, with Unicode-3.0, BSD-2-Clause, Zlib,
and this project's own MPL-2.0 crates.

MIT, Apache-2.0, BSD, and Zlib all require their notices to accompany
binary redistribution. Any published binary artifact must ship a
generated third-party license bundle alongside it.

Not yet in place, and required before broad distribution: a `deny.toml`
license and source policy, and a generated SBOM.

## Unpinned inputs

`kernel/build.sh` downloads a Linux kernel tarball from `cdn.kernel.org`
with no checksum verification. It is a development convenience for the
fast-boot experiment, not part of the VMM build, but it should either
pin a digest or be removed before publication.
