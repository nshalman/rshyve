# vmm-mantablock

The NVMe backend for a MantaBlock volume. It sends guest I/O to the
MantaBlock engine on the same CN over a shared-memory ring, as MantaBlock
spec 37 (the ring spec) defines. The engine is in the manta-storage
repository.

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
  known) and reports the pass to the controller in one call, so the
  controller raises one MSI for the batch.
- Runs one control thread: Ping every 100 ms, and the engine counts as
  lost with no Pong in 1 s or a request on the ring for longer than the
  I/O limit (30 s by default).
- Reports a durable namespace (VWC 0) only when AttachOk says every write
  is durable. It completes Flush itself then, as the spec says. An engine
  whose writes are not durable is refused unless `allow_volatile` is set,
  for tests and measurement; Flush and FUA then go to the engine.

## What S2 leaves out

- **The remote path.** When the engine is lost, every request held fails
  with an internal error. Spec 37 wants them kept and replayed over
  NVMe-oF to another engine (S5). For that reason write data is not kept
  past the copy into the ring.
- **Reattach.** Each attach asks for a new generation.
- **rshyve wiring.** The crate is outside the root workspace (see below),
  so no binary uses it yet.

## The ring crate dependency

`manta-block-ring` is a git dependency pinned to a commit, so the two
sides of the ring cannot drift. The repository is private, which is why
this crate is its own Cargo workspace: as a member of the root one it
would put that source in the root lockfile, and every CI job and
`cargo deny` would need access to it.

Cargo fetches the dependency over HTTPS. With git credentials that reach
the repository:

```bash
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test
```

Where GitHub is not reachable, patch the dependency to a local copy of
the same commit. Do not commit the patch:

```bash
cargo test --config \
  'patch."https://github.com/TritonDataCenter/manta-storage.git".manta-block-ring.path="<manta-storage>/crates/manta-block-ring"'
```

Cargo then rewrites `Cargo.lock` locally, so `--locked` cannot be used
with the patch.

## Tests

The unit tests play the engine in-process over a real shared mapping and
real pipes, so they can send completions the real engine never would.

`tests/engine.rs` runs the real `mantablock-engine` with a file volume
and a null volume. Build the engine from the pinned commit and name it:

```bash
cargo build -p manta-block-engine        # in manta-storage
MANTABLOCK_ENGINE=<manta-storage>/target/debug/mantablock-engine cargo test
```

These tests fail without `MANTABLOCK_ENGINE`, rather than pass without
running.
