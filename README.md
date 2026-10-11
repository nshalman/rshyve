# rshyve - Rust bhyve VMM for Triton/SmartOS

A Rust userspace VMM for illumos bhyve. It takes the command line the
bhyve brand builds, so a Triton/SmartOS zone can run it in place of the
platform bhyve through the brand's `/bhyve.zone` override. Live
migration is a first-class feature and C bhyve has none.

It is not all of bhyve. `ahci-hd` is refused at startup, `-D`, `-k` and
`--json-config` are refused rather than ignored, an unknown `-s` driver
name is logged and skipped, and a number of device options parse and
then do nothing. Each one is listed in
[docs/features.md](docs/features.md#accepted-by-the-cli-and-then-ignored).

**Experimental.** This is not production-supported software. Major
architectural state is still missing (see
[docs/status.md](docs/status.md)), live migration is unauthenticated and
does not transfer FPU/XSAVE state, and no release has been published.
Read [SECURITY.md](SECURITY.md) before running an untrusted guest on it.

## Built on Propolis

This project stands on [Propolis](https://github.com/oxidecomputer/propolis),
Oxide Computer Company's bhyve VMM. The bhyve and viona kernel ioctl bindings,
the address space manager, and the PS/2 device models are from Propolis 
largely unchanged; parts of the PCI, UART, and VM-exit layers derive from it
too. Both projects are MPL-2.0.

Nineteen files, 3300 significant lines, measured against Propolis
`b428c40e`. Each carries a header naming its upstream counterpart,
[THIRD_PARTY.md](THIRD_PARTY.md) lists them with measured similarity, and
`tools/propolis-derivation.py` regenerates that list against a Propolis
checkout so the claim can be checked rather than trusted. The tool
prints the revision it measured.

## Quick Start

```bash
rshyve -c 2 -m 2G \
  -s 4,nvme,/dev/zvol/rdsk/zones/disk0 \
  -s 6,virtio-net-viona,net0 \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,stdio \
  myvm
```

## What the workspace builds

`rshyve` is the larger binary: the brand's flag grammar, UEFI boot,
migration, the control socket and passthrough. `firehyve` is a
microVM over the same crates: direct kernel boot, virtio devices, and a
stdio serial console, with no UEFI, migration, control socket or
passthrough. It shares the flag grammar and rejects what it cannot
honor at startup. See [docs/cli.md](docs/cli.md#firehyve).

One smaller binary goes with them: `fhrun` runs a Linux binary inside a
firehyve microVM like a child process (see
[docs/fhrun.md](docs/fhrun.md)).

Migration orchestration is not in this repository. The VMM implements
the wire protocol and the control-socket verbs; replicating the disk and
sequencing the two hosts belong to whatever drives it. The contract an
orchestrator has to meet is in
[docs/migration.md](docs/migration.md#the-zfs-barrier).

## Documentation

- [Architecture](docs/architecture.md) - Workspace layout, key types, data flow
- [CLI Reference](docs/cli.md) - All flags and options
- [Feature status](docs/status.md) - What is tested and what is not
- [Feature reference](docs/features.md) - What each binary supports, with limits and caveats
- [Backlog](FEATURES_TODO.md) - What is not built yet, and why
- [Devices](docs/devices.md) - Supported PCI and LPC devices
- [Live Migration](docs/migration.md) - Cross-host VM migration
- [Metadata Agent](docs/mdata.md) - Guest configuration via COM2
- [Platform Integration](docs/platform.md) - vmadm/boot.c integration
- [Hardware run](docs/hardware-run.md) - what a live guest established, and what it did not

## Building and testing

The `rshyve` binary targets illumos: `bhyve-api` binds SunOS ioctls and
`viona-api` needs libdladm. Everything else in the workspace builds and
tests on any host.

`vmm-tpm-sys` is one of those. It drives a real autotools build of the
vendored libtpms on macOS and Linux as well as on illumos, and the 12
vTPM tests run on each. Do not replace it with a stub build script. A
stub hides the OpenSSL link path, and that path is what once produced a
binary no platform image could load
([docs/platform.md](docs/platform.md#openssl-and-the-vtpm)).

libtpms is a submodule, so clone with it:

```bash
git clone --recurse-submodules https://github.com/TritonDataCenter/rust-bhyve
# already cloned:  git submodule update --init third_party/libtpms
```

```bash
cargo build --workspace --locked --release   # on an illumos host
cargo test --workspace --locked --no-fail-fast
```

Pass `--no-fail-fast`. Without it the run stops at the first crate that
fails, and the partial output still reads like a completed pass.

Build prerequisites on the illumos host: a Rust toolchain, autoconf,
automake, libtool, pkg-config, gmake, gcc, and OpenSSL development
files. A macOS or Linux host needs the same set, with `make` in place
of `gmake`. DTrace is not required to build; USDT probes are compiled in
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
for the feature matrix and
[docs/hardware-run.md](docs/hardware-run.md) for what a live guest
showed, including the one platform limit it found: a device reset has
no bounded duration on illumos.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) covers the build, what CI does and
does not prove, the repository gates, and commit style.
[CHANGELOG.md](CHANGELOG.md) records what an operator would notice.

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
