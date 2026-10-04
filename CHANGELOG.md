# Changelog

Notable changes per release, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html) once there is
a version to follow.

No release has been published. Until one is, `Unreleased` is the whole
file and `git log` is the detailed record. Add an entry here for anything
a user or an operator would notice: a new device, a changed flag, a
changed default, a removed feature, or a security fix.

## Unreleased

### Added

- `firehyve`, a microVM binary over the same crates as `rshyve`: direct
  kernel boot, virtio devices, stdio serial console, no UEFI, migration,
  control socket or passthrough.
- `fhrun`, which runs a Linux binary inside a `firehyve` microVM like a
  child process.
- virtio-fs, virtio-vsock and virtio-console devices; PCI, CPU and memory
  hot-plug behind `--hotplug`.
- A vTPM 2.0 over libtpms, with a TCG PTP CRB, a TPM2 ACPI table and
  file-backed NV state.
- Repository gates: public-surface, unsafe-comment, CI-coverage,
  dependency-boundary and test-parity, each with a CI self-test.

### Changed

- `rust-bhyve` is the project name. Earlier documents called it `vmmnew`
  or `vmm`.
- Documentation reduced to one status file
  ([docs/status.md](docs/status.md)), one layout file
  ([docs/architecture.md](docs/architecture.md)), one feature reference
  ([docs/features.md](docs/features.md)) and one backlog
  ([FEATURES_TODO.md](FEATURES_TODO.md)).
- A writable block device node (`/dev/dsk/...`, `/dev/zvol/dsk/...`,
  `/dev/lofi/...`) is refused as a virtio-blk or NVMe backing disk,
  because specfs hides flush errors. Use the raw node or attach it
  read-only.
- A disk whose write cache is on but which cannot flush it is refused,
  and so is a disk that fails the flush probe at open. Only a zvol's
  write cache is changed; other disks are left as found.

### Security

- Unresolved findings are tracked in
  [docs/security/open-findings.md](docs/security/open-findings.md). The
  ones that gate production use today: live migration has no peer
  authentication or encryption (OSF-1), migration does not transfer
  FPU/XSAVE state (OSF-2), the vendored libtpms predates two CVE fixes
  (OSF-4), and PCI passthrough has not been audited (OSF-5).
