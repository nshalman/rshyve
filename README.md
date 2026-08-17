# vmm - Rust bhyve VMM for Triton/SmartOS

A Rust userspace VMM for illumos bhyve, CLI-compatible with the C
`bhyve` command so it can be dropped into a Triton/SmartOS compute node
in its place. Designed with live migration as a first-class feature.

**Experimental.** This is not production-supported software. Major
architectural state is still missing (see
[docs/status.md](docs/status.md)), live migration is unauthenticated and
does not transfer FPU/XSAVE state, and no release has been published.
Read [SECURITY.md](SECURITY.md) before running an untrusted guest on it.

## Built on Propolis

This project stands on [Propolis](https://github.com/oxidecomputer/propolis),
Oxide Computer Company's bhyve VMM, and would not exist without it. The
bhyve and viona kernel ioctl bindings, the address space manager, and
the PS/2 device models are taken from Propolis largely unchanged; parts
of the PCI, UART, and VM-exit layers derive from it too. Both projects
are MPL-2.0.

Nineteen files, about 3,500 significant lines. Each carries a header
naming its upstream counterpart, [THIRD_PARTY.md](THIRD_PARTY.md) lists
them with measured similarity, and `tools/propolis-derivation.py`
regenerates that list against a Propolis checkout so the claim can be
checked rather than trusted.

## Quick Start

```bash
vmm -c 2 -m 2G \
  -s 4,nvme,/dev/zvol/dsk/zones/disk0 \
  -s 6,virtio-net-viona,net0 \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,stdio \
  myvm
```

## Documentation

- [Architecture](docs/architecture.md) - Crate structure and design
- [CLI Reference](docs/cli.md) - All flags and options
- [Devices](docs/devices.md) - Supported PCI and LPC devices
- [Live Migration](docs/migration.md) - Cross-host VM migration
- [Metadata Agent](docs/mdata.md) - Guest configuration via COM2
- [Platform Integration](docs/platform.md) - vmadm/boot.c integration

## Building and testing

The `vmm` binary builds only on illumos: `vmm-tpm-sys` runs an autotools
build of vendored libtpms, `bhyve-api` binds SunOS ioctls, and
`viona-api` needs libdladm. Everything else in the workspace builds and
tests on any host.

```bash
cargo build --workspace --locked --release   # on an illumos host
cargo test --workspace --locked
```

Build prerequisites on the illumos host: a Rust toolchain, autoconf,
automake, libtool, pkg-config, gmake, gcc, and OpenSSL development
files. DTrace is not required to build; USDT probes are compiled in
either way and simply do not register without it.

[docs/testing.md](docs/testing.md) covers the lab procedures: booting a
test VM, guest networking via the metadata agent, fio benchmarks, the
cross-host migration test, and the Windows boot harness.

To exercise it as a platform component, build a SmartOS platform image
with this binary in place of `/usr/sbin/bhyve`, install it with
`sdcadm platform install`, and assign it to a compute node. Every bhyve
VM on that node will then use this VMM. No prebuilt image is published;
one would need checksums, provenance, and a signing story before it
could be offered, since it replaces `/usr/sbin/bhyve` wholesale.

## Status

Tested on SmartOS with bhyve API v18. See [docs/status.md](docs/status.md)
for feature matrix.

## Security

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).
Known unresolved issues are tracked in
[docs/security/open-findings.md](docs/security/open-findings.md).

## License

MPL-2.0. Full text in [LICENSE](LICENSE).

Portions are copied from or derived from
[Propolis](https://github.com/oxidecomputer/propolis), Copyright Oxide
Computer Company, used under MPL-2.0. libtpms is vendored under the
3-clause BSD license. See [NOTICE](NOTICE) for attribution and
[THIRD_PARTY.md](THIRD_PARTY.md) for exact provenance.
