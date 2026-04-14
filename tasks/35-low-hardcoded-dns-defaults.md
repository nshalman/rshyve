# 35 — Hardcoded Google/Cloudflare DNS defaults

**Priority:** LOW
**File:** `bin/vmm/src/mdata.rs:420`
**Category:** Configuration — inappropriate defaults for Triton/SmartOS

## Problem

```rust
store.set("sdc:resolvers", r#"["8.8.8.8","1.1.1.1"]"#);
```

Google DNS (8.8.8.8) and Cloudflare DNS (1.1.1.1) are hardcoded as default resolvers. In a Triton/SmartOS datacenter environment:

1. VMs should use the datacenter's internal DNS resolvers
2. External DNS may not be reachable from all networks
3. DNS queries to external resolvers leak internal hostnames

The defaults should come from zone configuration or the `--mdata-resolvers` CLI flag.

## Fix

Only set default resolvers if none are provided via CLI:

```rust
if let Some(resolvers) = &cli.mdata_resolvers {
    store.set("sdc:resolvers", resolvers);
}
// No default — if no resolvers are configured, the guest gets no
// resolver configuration via mdata, which is correct behavior.
// The guest can still configure DNS via DHCP or manual config.
```

If a default is truly needed, use a less opinionated value or make it configurable at the platform level (e.g., read from zone metadata).
