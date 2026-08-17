# vmmnew — Rust bhyve VMM for Triton/SmartOS

## What it is

A new Rust-based bhyve userspace VMM, replacing the existing C `bhyve` command. Designed for live migration as a first-class feature, with CLI compatibility for vmadm/boot.c integration. Built on [Oxide's Propolis](https://github.com/oxidecomputer/propolis) (MPL-2.0) and purpose-built for Triton: the bhyve and viona kernel bindings, the address space manager, and the PS/2 models come from there largely unchanged. See NOTICE and THIRD_PARTY.md.

## What works today

**Full Ubuntu 24.04 guest** — Boots from ZFS zvol with UEFI, NVMe disk, virtio-net networking, cloud-init SmartOS datasource, SSH access via mdata-delivered keys. Tested with 2 vCPUs, 1 GB RAM.

**Performance (NVMe, dd O_DIRECT):**

| Test | Rust VMM | C bhyve | Notes |
|------|----------|---------|-------|
| Sequential Read 1M | **2.8 GB/s** | 2.8 GB/s | Parity (ARC-cached) |

**Devices:** i440fx chipset, NVMe controller (async I/O, zero-copy preadv/pwritev, 8 worker threads), virtio-blk, virtio-net (viona kernel-accelerated with interrupt polling), virtio-rng, framebuffer/VNC (Unix socket), xHCI USB tablet, PCI passthrough (Intel VT-d + AMD IOMMU), dual 16550 UARTs (256-byte FIFO), ACPI tables (table-loader + legacy), SMBIOS 2.8 (SmartDC HVM), fw_cfg, E820, bhyve kernel devices (LAPIC, IOAPIC, ATPIC, ATPIT, HPET, RTC, PM timer), ACPI PM1 registers.

**Live migration** — Experimental. WebSocket protocol with iterative dirty page transfer. Works same-host and cross-host with TSC frequency scaling. CPUID masking (`--cpu-baseline avx2`) for cross-generation migration. The destination compares CPU features at the preamble but **warns and continues** on a mismatch rather than rejecting it, FPU/XSAVE state is not transferred, and the transport is neither authenticated nor encrypted. See [SECURITY.md](SECURITY.md).

**Metadata agent** — Built-in SmartOS mdata V2 protocol on COM2 for testing. Serves sdc:nics, sdc:hostname, sdc:resolvers, SSH keys. Cloud-init SmartOS datasource detects via SMBIOS and configures network automatically. In production, the external metadata agent is used instead.

**Platform integration** — All boot.c CLI flags supported. SMBIOS defaults to "SmartDC HVM" / "Joyent" for cloud-init detection. SIGTERM triggers ACPI poweroff. Serial console supports stdio and device paths (-l com1,/dev/zconsole).

## Crate structure

```
vmmnew/
  bin/vmm/                  Main binary + mdata agent + control socket
  crates/bhyve-api/         Kernel VMM ioctl FFI (forked from propolis)
  crates/viona-api/         viona NIC driver FFI (forked from propolis)
  crates/vmm-core/          VmmHdl, PhysMap, MemCtx, Vcpu, CPUID masking
  crates/vmm-devices/       PCI, chipset, virtio, NVMe, UART, fbuf, xHCI, passthru
  crates/vmm-migrate/       Migration protocol, codec, state export/import
  crates/vmm-config/        CLI parsing (clap, bhyve flag-compatible)
```

## Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| **Phase 1: Foundation** | VmmHdl, PhysMap, MemCtx, Builder, Vcpu | Complete |
| **Phase 2: Essential devices** | All PCI/LPC devices, virtio, ACPI, SMBIOS | Complete |
| **Phase 3: Control + state** | Unix socket, JSON protocol, pause/resume, shutdown | Complete |
| **Phase 4: Live migration** | WebSocket, dirty pages, state export/import, time sync | Complete |
| **Phase 5: Additional devices** | NVMe, framebuffer/VNC, xHCI, PCI passthrough | Complete |
| **Phase 6: Production** | boot.c integration, mdata, CPUID masking, networking | In progress |

## Remaining work for platform image

- Build integration (Cargo release build into platform image at /usr/sbin/vmm)
- boot.c modification to exec `vmm` instead of `bhyve`
- AHCI native implementation (currently falls back to virtio-blk)
- FPU/XSAVE state transfer during migration
- Large VM testing (>32 GB RAM, >16 vCPUs)

## Build & test

```bash
# Local (macOS cross-check)
cargo test

# SmartOS headnode (actual VM execution)
ssh root@<headnode>
export PATH=/opt/tools/bin:$PATH
cd /opt/vmmnew && cargo build --release

# Boot Ubuntu 24.04 from ZFS zvol with networking
./target/release/vmm -H -c 2 -m 1G \
  -s 0,hostbridge \
  -s 4,nvme,/dev/zvol/dsk/zones/vmm-test-disk \
  -s 6,virtio-net-viona,vmm_test0 \
  -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,stdio \
  --mdata-nics '[{"interface":"net0","mac":"...","ips":["10.0.0.5/24"],"gateways":["10.0.0.1"],"primary":true}]' \
  --mdata-ssh-keys "ssh-rsa AAAA..." \
  testvm

# Live migration (source)
./target/release/vmm ... --track-dirty --control-socket /tmp/vm.sock testvm
echo '{"command":"migrate-source","target_addr":"dest:4567"}' | socat - UNIX-CONNECT:/tmp/vm.sock

# Live migration (destination)
./target/release/vmm ... --track-dirty --migrate-listen 0.0.0.0:4567 testvm
```

## Documentation

See [docs/](docs/) for detailed documentation:
- [Architecture](docs/architecture.md) — Crate structure, key types, data flow
- [CLI Reference](docs/cli.md) — All flags and options
- [Devices](docs/devices.md) — Supported PCI and LPC devices
- [Live Migration](docs/migration.md) — Protocol, CPU baseline, usage
- [Metadata Agent](docs/mdata.md) — V2 protocol, NIC JSON format
- [Platform Integration](docs/platform.md) — boot.c integration guide
- [Feature Status](docs/status.md) — Detailed feature matrix
