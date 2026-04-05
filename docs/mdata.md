# Metadata Agent (mdata)

## Overview

The VMM includes a built-in SmartOS metadata agent for testing and
standalone use. In production, the external metadata agent (running in
the global zone) is used instead — COM2 is connected to the zone's
metadata socket via `-l com2,/.zonecontrol/metadata.sock`.

The built-in agent activates only when no `-l com2,<path>` is specified.

## Protocol

The agent implements the Joyent Metadata Protocol V2:

1. Guest opens `/dev/ttyS1` (Linux) or `/dev/term/b` (SmartOS)
2. Guest sends `\n` (reset), agent responds `invalid command\n`
3. Guest sends `NEGOTIATE V2\n`, agent responds `V2_OK\n`
4. All subsequent messages use V2 framing

### V2 Frame Format

```
V2 <body_len> <crc32_hex> <reqid> <command> [<b64_arg>]\n
```

- `body_len` - decimal length of body after the third space
- `crc32_hex` - CRC32 of body (8 hex digits)
- `reqid` - 8 hex digit request ID
- `command` - GET, KEYS, PUT, DELETE
- `b64_arg` - base64-encoded argument

### Supported Commands

| Command | Description |
|---------|-------------|
| `GET <key>` | Retrieve metadata value |
| `KEYS` | List all available keys |

PUT and DELETE return FAILURE (read-only store).

## Metadata Keys

### Populated via CLI flags

| Key | CLI Flag | Description |
|-----|----------|-------------|
| `sdc:hostname` | positional `<VM_NAME>` | VM hostname |
| `sdc:uuid` | `-U` | Instance UUID |
| `sdc:nics` | `--mdata-nics` | Network configuration (JSON) |
| `sdc:resolvers` | `--mdata-resolvers` | DNS resolvers (JSON array) |
| `root_authorized_keys` | `--mdata-ssh-keys` | SSH public keys |
| `root_pw` | `--mdata-root-pw` | Root password |

### Default values

| Key | Default |
|-----|---------|
| `sdc:resolvers` | `["8.8.8.8","1.1.1.1"]` |
| `sdc:maintain_resolvers` | `true` |
| `sdc:routes` | `[]` |
| `sdc:dns_domain` | (empty) |
| `user-script` | (empty) |
| `user-data` | (empty) |
| `sdc:vendor-data` | (empty) |
| `sdc:operator-script` | (empty) |

## NIC JSON Format

The `--mdata-nics` value must be a JSON array matching the SmartOS
`sdc:nics` schema. Cloud-init's SmartOS datasource uses the `ips` and
`gateways` array fields for network configuration:

```json
[{
  "interface": "net0",
  "mac": "02:08:20:aa:bb:cc",
  "ips": ["10.0.0.5/24"],
  "gateways": ["10.0.0.1"],
  "primary": true
}]
```

## Guest Compatibility

The metadata agent is compatible with:

- **cloud-init SmartOS datasource** - Detects SmartOS via SMBIOS
  product name `SmartDC*`, negotiates V2, fetches all `sdc:*` keys
- **mdata-client** (Rust, from monitor-reef) - V2 protocol over serial
- **mdata-client** (C, from smartos-live) - V1/V2 protocol over serial

### SMBIOS Requirement

Cloud-init's `ds-identify` checks DMI product name for `SmartDC*`.
The VMM defaults SMBIOS Type 1 product to `SmartDC HVM` and
manufacturer to `Joyent`. These can be overridden with `-B`.

## UART Implementation Notes

The UART uses 256-byte FIFOs (oversized vs hardware 16550A's 16 bytes)
to buffer complete V2 protocol frames. Response bytes are paced to
avoid overflowing the RX FIFO while the guest drains it.

Non-printable characters (ESC sequences from UEFI firmware probing COM2
during boot) are silently discarded.
