# vmm-mantablock

The NVMe backend for a MantaBlock volume. It sends guest I/O to the
MantaBlock engine on the same CN over a shared-memory ring, as the
`manta-block-ring` crate defines it. The engine is in the manta-storage
repository.

rshyve uses it for `-s <slot>,nvme,mantablock=<socket>,volume=<uuid>`.
See [docs/cli.md](../../docs/cli.md#nvme-on-a-mantablock-volume).

## What it does

- Connects to the engine's Unix socket for the volume, does Hello and
  Attach, and takes the region and the wake-up pipe ends with
  `SCM_RIGHTS`. It checks the grant against what it asked, checks each
  descriptor's type and direction, checks the region file's length before
  it maps it, and checks the region header against AttachOk.
- Gives each guest I/O queue one submission ring and one completion ring.
  `RequestId::tag` is not the ring tag: each ring tag owns a fixed buffer
  slot, and a request larger than one slot goes as several parts with
  their own tags. NVMe MDTS is 2 MiB and a backend cannot lower it, so
  this is how a large request fits.
- Copies write data from the guest into the buffer area before it
  publishes the entry, and read data out to the guest after it takes the
  completion. The engine never maps guest memory.
- Never blocks in `submit`. A part that has no free tag waits in a
  per-queue list and goes when a completion frees one.
- Runs one completion thread per queue. It takes every completion on the
  ring in one pass, checks each one (tag in flight, op id matches, status
  known, reserved bytes zero) and reports the pass to the controller in
  one call, so the controller raises one MSI for the batch. A pass with
  more completions than tags means the engine is lost. A run of wake-ups
  that find no completion makes the thread sleep 1 ms between wakes.
- Runs one control thread: Ping every 100 ms, and the engine counts as
  lost with no Pong in 1 s or a request on the ring for longer than the
  I/O limit (30 s by default). It takes at most 64 frames per tick, so a
  flood cannot keep it from its timers. On halt it waits for Detached at
  most the control deadline (5 s).
- Reports a durable namespace (VWC 0) only when AttachOk says every write
  is durable. It completes Flush itself then, as the spec says. An engine
  whose writes are not durable is refused unless `allow_volatile` is set,
  for tests and measurement; Flush and FUA then go to the engine.

## Trust

The engine runs in the global zone and already holds all of the
volume's data, so it is trusted. The checks above defend against an
engine bug, not a hostile engine: a buggy engine must not corrupt guest
memory or hang the VMM.

One thing is not defended: the region is a shared file mapping, and a
fault on it raises SIGBUS in rshyve. The engine must not shrink the
region file while it is attached, and must not leave it sparse on a
tmpfs that can run out of space. rshyve checks the file's length once,
before it maps it, and does nothing about SIGBUS after that.

## Not done yet

- **The remote path.** When the engine is lost, every request held fails
  with an internal error. The plan is to keep them and replay them over
  NVMe-oF to an engine on another CN. Until then write data is not kept
  past the copy into the ring.
- **Reattach.** Each attach asks for a new generation.
- **Migration.** rshyve refuses a ring disk on a migration destination.

## The ring crate dependency

`manta-block-ring` comes from its public repository, pinned to a commit
in the root `Cargo.toml`, so the two sides of the ring cannot drift. The
engine pins the same commit. Moving the pin changes the ring contract:
move both sides together.

To try an engine-side change to the ring crate before it has a commit,
patch the dependency to a local checkout. Do not commit the patch:

```bash
cargo test -p vmm_mantablock --config \
  'patch."https://github.com/TritonDataCenter/manta-block-ring".manta-block-ring.path="<manta-block-ring>"'
```

Cargo then rewrites `Cargo.lock` locally, so `--locked` cannot be used
with the patch.

## Tests

The unit tests play the engine in-process over a real shared mapping and
real pipes, so they can send completions the real engine never would.

`tests/engine.rs` runs the real `mantablock-engine` with a file volume
and a null volume. The engine is in manta-storage, which is a private
repository, so CI cannot build it and these tests are ignored by
default. Run them by hand before a change to this crate or to the ring
pin merges. Check out the manta-storage commit whose `Cargo.toml` pins
the same `manta-block-ring` rev as the root `Cargo.toml` here, then:

```bash
cargo build -p manta-block-engine        # in manta-storage
MANTABLOCK_ENGINE=<manta-storage>/target/debug/mantablock-engine \
  cargo test -p vmm_mantablock -- --include-ignored
```

Run that way, these tests fail without `MANTABLOCK_ENGINE`
rather than pass without the engine.

The `controller::` tests in `tests/engine/controller.rs` build the NVMe
controller on the ring with `NvmeController::with_backend`, as rshyve
does, and drive it through its registers. That constructor takes a VM
handle, so they make a VM and need `/dev/vmm` and the privilege to
make one: run them on the illumos build host.
