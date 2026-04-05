# vmm - Rust bhyve VMM for Triton/SmartOS

A drop-in replacement for the C `bhyve` hypervisor command, written in Rust.
Designed for Triton/SmartOS with live migration as a first-class feature.

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

## Testing

A SmartOS platform image with this VMM replacing `/usr/sbin/bhyve` is
available for testing:

```
https://$MANTA_URL/nick.wilkens@mnxsolutions.com/public/platforms/platform-20260402T214615Z.tgz
```

Install via `sdcadm platform install <url>` and assign to a compute node.
All bhyve VMs on that CN will use the Rust VMM.

## Status

Tested on SmartOS with bhyve API v18. See [docs/status.md](docs/status.md)
for feature matrix.

## License

MPL-2.0
