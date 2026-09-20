# fhrun

`fhrun` runs a Linux binary as if it were a host process, by wrapping it
in a microVM. It builds an initramfs holding `/init` (the `fhrun-init`
companion), the payload at `/app`, and a JSON spec at
`/firehyve-spec.json`, then starts the VMM with the shared `vmm-config`
argv grammar. One grammar means a manifest the VMM cannot honor fails at
parse time instead of at boot.

## Commands

```
fhrun <manifest.json>                        boot it
fhrun --check <manifest.json>                validate only
fhrun --print-argv <manifest.json>           print the VMM argv, one token per line
fhrun --emit-initramfs <manifest.json> <out.cpio>
```

## Manifest

| key | required | default | meaning |
|---|---|---|---|
| `name` | yes | | VM name, 1..=127 chars, no NUL |
| `bin` | yes | | host path to the payload binary |
| `kernel` | yes | | host path to the kernel; the boot protocol is read from the file |
| `init` | yes | | host path to the static `fhrun-init` |
| `args` | no | `[]` | argv[1..]; argv[0] is `bin`'s basename |
| `env` | no | `{}` | environment for the payload |
| `workdir` | no | `/` | working directory in the guest |
| `vcpus` | no | `1` | vCPU count |
| `memory` | no | `"128M"` | memory size, parsed by the VMM |
| `extra_files` | no | `{}` | relative in-guest path to host path |
| `net` | no | | single-NIC alias; always `eth0` |
| `nics` | no | `[]` | more NICs, `eth1..`; 4 in total at most |
| `consoles` | no | `[]` | virtio-console channels; 2 at most |
| `guest_metadata` | no | | opaque JSON forwarded to the guest spec |
| `vmm` | no | `"firehyve"` | VMM binary; the key `firehyve` is an alias |
| `kernel_extra_cmdline` | no | `""` | appended to the fhrun cmdline |

### consoles

Each entry is a generic virtio-console channel with no protocol
semantics:

| key | default |
|---|---|
| `socket_path` | `<runtime-dir>/console<N>.sock` |
| `guest_device` | `/dev/hvc<N>` |
| `role` | none; an opaque label |

`socket_path` is host-side and is stripped before the spec reaches the
guest. `guest_device` and `role` are forwarded. A caller that speaks its
own protocol over a channel tags it with `role` and puts its payload in
`guest_metadata`; fhrun forwards both without reading either.

There is no implicit console. An empty or absent `consoles` list means
the VM gets no virtio-console device.

## Emitted command line

```
-c <vcpus> -m <memory> --kernel <kernel> --initrd <initramfs>
--cmdline <cmdline> --no-track-dirty -l com1,stdio
-s <7+i>,virtio-net-viona,<vnic>       one per NIC, slots 7..=10
-s <15+i>,virtio-console,<socket>      one per console, slots 15..=16
<name>
```

The console device config is a bare socket path. A `key=` prefix would
become part of the path the device binds.

`--no-track-dirty` is unconditional: an fhrun VM never migrates, so it
should not pay for a dirty bitmap.

## Exit codes

fhrun exits with the payload's status, like a shell would:

| code | meaning |
|---|---|
| `0..=255` | the payload's own exit code |
| `128 + n` | the payload died from signal `n` |
| `1` | fhrun failed: a bad manifest, a VMM that would not start, or a VMM that ended before init reported the payload |
| `2` | usage error |

`fhrun-init` stays PID 1, forks the payload, reaps it and every orphan
it leaves, and writes one line to `/dev/ttyS1` (COM2):

```
fhrun-init: payload exit <code>
fhrun-init: payload signal <n>
```

firehyve mirrors COM2 to its stdout, which fhrun pipes rather than
inherits: it copies every byte to its own stdout and keeps the last
marker it sees. Init then powers the VM off. The last marker wins
because init writes after the payload is gone; a payload that prints a
marker of its own only spoofs its own exit status, which it could set
anyway.

When no marker arrives the VMM's outcome is all there is, and none of
its codes means the payload succeeded. A VMM exit `0` is a reset, which
under `panic=-1` is how a kernel panic ends; `1` is a poweroff that
init reached without a payload, such as a `bin` that cannot be
executed. fhrun reports both on stderr and exits `1`.

## Signals

SIGINT and SIGTERM are forwarded to the VMM as SIGTERM, which is its
ACPI power-button path, so the guest shuts down and the temporary
directory is removed. Both stay blocked from before the VMM is spawned
until fhrun knows its pid, so a Ctrl-C in that window is not lost.

SIGKILL has no such path. fhrun dies without removing
`$TMPDIR/fhrun-*/initramfs.cpio`, which holds the manifest `env`, and
the VMM keeps running: illumos has no parent-death signal and fhrun
records the child pid nowhere a sweeper could read it.

## fhrun-init

`tools/fhrun-init` is a separate Cargo workspace with its own lockfile,
named in the root manifest's `exclude` list. It cross-compiles to
`x86_64-unknown-linux-musl`:

```sh
rustup target add x86_64-unknown-linux-musl
cd tools/fhrun-init
cargo build --release --target x86_64-unknown-linux-musl
```

The resulting binary is what a manifest's `init` field points at. CI
formats, lints and tests it in the `guest init (musl)` job, because no
root-workspace job reaches an excluded package.

Its `GuestSpec` declares only the fields it acts on. Everything else the
host sends, `metadata` above all, is ignored by serde and reaches the
payload binary untouched. `guest_spec_json_matches_init_contract` in
`tools/fhrun/src/manifest.rs` locks the key names on the host side,
because a rename cannot fail at compile time across the workspace
boundary.
