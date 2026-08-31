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

`0` when the VMM exited cleanly, `1` on an fhrun error or a VMM killed
by a signal, `2` on a usage error.

The payload's own exit status does **not** propagate. `fhrun-init`
`execve`s the payload and never reaps it; when the payload exits, init
falls through to ACPI poweroff and the VMM reports a clean shutdown.
Recovering the payload's status needs a return channel (vsock, or a
writable virtio-fs) that does not exist yet.

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
