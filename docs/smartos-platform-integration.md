# Shipping firehyve in the SmartOS platform image

Investigated 2026-09-15 on the vnext build host. Everything below
was run or read there, not inferred. The headline: firehyve builds
clean on the platform toolchain in 42 seconds, the platform already
ships a Rust project by the same mechanism, and the platform already
has a supported hook for running an alternative VMM per zone.

## 1. How the vnext platform is built today

A smartos-live checkout at branch `vnext`, configured with a
`configure-projects` file that adds the vnext-specific projects, built
with `gmake iso usb tests-tar`. A nightly wrapper clones a fresh tree,
builds it, and copies `output/platform-<stamp>.tgz` and
`output-iso/platform-<stamp>.iso` on success.

`configure-projects` for vnext today:

```
illumos-extra: master: origin
illumos: vnext: <local illumos-joyent checkout>
local/kbmd: master: origin
local/kvm-cmd: master: origin
local/kvm: master: origin
local/mdata-client: master: origin
local/proteus: main: https://github.com/TritonDataCenter/proteus
local/ur-agent: master: origin
```

## 2. The mechanism, and the precedent

**proteus is a Rust project that already ships in the platform image.**
That settles the hard questions before they are asked: Rust in the
platform build works, the toolchain fetch works, and a Rust binary in
the proto area links correctly.

The wiring is entirely convention:

- `LOCAL_SUBDIRS := $(shell ls projects/local)` in the top Makefile, so
  anything `configure` clones into `projects/local/` is built.
- `0-subdir-%-stamp` runs `gmake $(SUBDIR_DEFS) DESTDIR=$(PROTO) world
  install` in the project directory.
- `SUBDIR_MANIFESTS` collects a `manifest` file from each project.
- Incremental rebuild is `rm 0-subdir-<name>-stamp && gmake
  0-subdir-<name>-stamp`.

A project therefore owes four things: a `Makefile` with `world`,
`install`, `manifest`, `clean`, `test` and `update` targets; a
`manifest` file; a `rust-toolchain.toml`; and a line in
`configure-projects`.

proteus gets its toolchain from smartos-live's own `deps/eng`:

```make
RUST_TOOLCHAIN_DIR=rust
RUST_USE_BOOTSTRAP=false
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
include ../../../deps/eng/tools/mk/Makefile.rust.defs
include ../../../deps/eng/tools/mk/Makefile.rust.targ

all: rust-fetch
	(. rust/cargo/env && $(CARGO) build --release --workspace)
```

Its `manifest` is a plain mode-and-owner table:

```
d usr/sbin 0755 root sys
f usr/sbin/proteusadm 0555 root sys
```

## 3. What rust-bhyve is missing

Three files and one line. Nothing structural.

| Needed | Status in rust-bhyve |
|---|---|
| `rust-toolchain.toml` | **absent** |
| `Makefile` (smartos-live project contract) | **absent** |
| `manifest` | **absent** |
| Line in `configure-projects` | **absent** |

The Makefile is deliberately separate from the normal cargo workflow
and is used only when the repo is pulled into a platform build, exactly
as proteus documents for itself.

**Install path.** The platform convention is `/usr/sbin`, not
`/usr/bin`. C bhyve installs to `proto/usr/sbin/bhyve` and proteus puts
`proteusadm` in `/usr/sbin`. Shipping firehyve as `/usr/sbin/firehyve`
keeps it beside `bhyve` and `bhyvectl` and avoids colliding with the
platform bhyve that the brand still expects to find.

## 4. It already builds

Cloned `TritonDataCenter/rust-bhyve` at `53a384b` onto a vnext build
host and built it with the stock 1.97.1 illumos toolchain:

```
cargo build --release -p firehyve
Finished `release` profile [optimized] target(s) in 42.39s
```

```
$ ./target/release/firehyve --version
firehyve 0.1.0 (53a384b)
```

No patches, no vendoring, no workarounds. The binary is 3.7 MB and
links only platform libraries plus `libgcc_s.so.1`, which the platform
ships at `/usr/lib/amd64/libgcc_s.so.1` via illumos-extra, so the
pkgsrc path seen in `ldd` on the build host is a build-host artifact
and not a packaging problem.

**The kernel interfaces line up.** The `vnext` illumos branch in the
build tree carries:

| Interface | Platform | rust-bhyve needs |
|---|---|---|
| `VMM_CURRENT_INTERFACE_VERSION` | 18 | >= 17 for dirty tracking |
| `VIONA_CURRENT_INTERFACE_VERSION` | 6 | 6 |

## 5. How a zone would actually run it

The platform already has the hook, and it is hardened.

`boot.c` in the bhyve brand writes the VM's argv as a packed nvlist
(`bhyve_args`) into `/var/run/bhyve/zhyve.cmd` inside the zone, mode
0600. In the zone, `zhyve` unpacks it and then calls
`try_bhyve_override()`, which is worth quoting because it is the whole
integration story:

> If `BHYVE_ZONE_PATH` exists, exec it in place of the platform bhyve.

`BHYVE_ZONE_PATH` is `/bhyve.zone`. The override is `fexecve`d on the
already-open fd so a concurrent rename or symlink swap cannot change
what runs, symlinks are followed at open time so an operator can
repoint the link to swap candidates, and a present-but-invalid override
is fatal rather than silently falling back to the platform bhyve.

Two consequences.

**The argv contract is already satisfied.** zhyve does the nvlist
parsing and hands the override a plain argv, so the "NVList/zhyve.cmd
reader not implemented" line in `docs/features.md` is not a blocker for
this path. firehyve is already built for this exact argv: it accepts
and ignores the `bootrom`, `fbuf` and `xhci` specs that `boot.c` emits
on every zone.

**There are two levels of integration, and the cheap one is real.**

- **Ship the binary, opt in per zone.** Add rust-bhyve as a project so
  `/usr/sbin/firehyve` is in the image, then a zone opts in by pointing
  `/bhyve.zone` at it. This is the mechanism already used to test
  firehyve on the lab node, and it needs no brand change at all.
- **Select the VMM from the brand.** Extend the bhyve brand so a zone
  attribute chooses the VMM instead of an in-zone file. This is the
  productized version and matches the standing decision to extend the
  bhyve brand rather than add a new one.

## 6. What else the platform needs for sandbox lifecycle

Present already:

- **Memory reservoir.** `/usr/lib/rsrvrctl` is in the proto area. This
  is the eleven-fold VM-create speedup and it is the single biggest
  lever on create latency.
- **proteus.** VPC, distributed firewall, NAT, DHCP synthesis and IMDS
  all ship. Inbound is default-deny and the `egress-guard` clamp is
  built.
- **ZFS.** Clone-per-sandbox needs nothing built.

Gaps, in rough order of how much they block a sandbox product:

1. **Guest kernel delivery.** Linux kernels deliberately do not ship in
   the platform image, and each firehyve instance selects a kernel by
   version. The platform needs a versioned fetch-and-resolve path and
   a cache, and that mechanism does not exist. This blocks everything
   else, because without it a sandbox has nothing to boot.
2. **`fhrun-init` has the same problem.** It is a Linux musl binary, so
   it is guest content, not platform content, and it needs to travel
   with the kernel through the same channel.
3. **The guest agent.** Nothing in the platform terminates the vsock
   control channel. This is the exec, file and port surface, and it is
   the largest single piece of missing work.
4. **Lifetime and reaping.** No idle detector, no max-runtime, no
   sweeper. Lab VMs already leak across sessions and are cleaned by
   hand. At fleet scale this needs to be a platform service.
5. **Bound the `vmm_drv` lease wait.** A stuck viona lease parks the
   VMM process past `SIGKILL`. On a host cycling thousands of
   short-lived sandboxes this leaks until reboot, and it is an illumos
   change, not a rust-bhyve one.
6. **Snapshot to file.** Needed for pause billing and fork. illumos has
   no `userfaultfd`, so lazy restore needs a kernel change; eager
   restore does not and should be measured first.
7. **Privilege reduction.** The VMM runs as root in the zone with no
   `setppriv`, no privsep and no chroot. illumos has no seccomp, so
   this needs its own design.
8. **Roll out the egress posture.** Built and host-tested, not enabled.

## 7. Suggested first step

Add the three files, add the `configure-projects` line, and take
one nightly. That produces a platform image with `/usr/sbin/firehyve`
in it, which can be exercised immediately through the existing
`/bhyve.zone` override without touching the brand. It is a small,
reversible change that converts firehyve from something staged by hand
onto a lab node into something the nightly produces.
