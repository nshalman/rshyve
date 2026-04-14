# Vulnerability Report: `metrics.rs` — JSON/Prometheus Injection

## Severity: **Medium** (High if metrics feed into an orchestrator or alerting pipeline)

## Vulnerability Type
**Injection via unsanitized string interpolation** — the `device` parameter in `BlockMetrics::to_json()`, `BlockMetrics::to_prometheus()`, `NetMetrics::to_json()`, and `NetMetrics::to_prometheus()` is interpolated directly into structured output without escaping.

## Affected Functions

| Function | Line (approx) | Format |
|---|---|---|
| `BlockMetrics::to_json()` | ~157 | JSON injection |
| `BlockMetrics::to_prometheus()` | ~171 | Prometheus label injection |
| `NetMetrics::to_json()` | ~218 | JSON injection |
| `NetMetrics::to_prometheus()` | ~230 | Prometheus label injection |

## Root Cause

The `device` name (a `&str`) is passed into `format!()` with `{}` — Rust's `Display` trait, which performs **no JSON or Prometheus escaping**. Characters like `"`, `\`, `}`, and newlines pass through verbatim, allowing an attacker who controls the device name to break out of the string context.

The device name originates from `VmmMetrics::add_block_device(name)` / `add_net_device(name)`, which accept an arbitrary `String`. If that string comes from a VM creation API, CLI argument, or configuration file controlled by a tenant, it's attacker-controlled input.

## Exploitation Strategy

### 1. JSON Structure Injection

A device name like:

```
x"}],"pwned":true,"block_devices":[{"device":"y
```

When rendered through `VmmMetrics::to_json()`, produces:

```json
{"uptime_secs":42,"vcpus":[],"block_devices":[{"device":"x"}],"pwned":true,"block_devices":[{"device":"y","reads":0,"writes":0,...}],"net_devices":[]}
```

This injects an arbitrary top-level key (`pwned`) and creates a duplicate `block_devices` key. JSON parsers handle duplicates differently — some take the first, some take the last — enabling key shadowing.

### 2. Prometheus Metric Injection

A device name like:

```
x"} 0
vmm_admin_override{user="attacker"} 1
vmm_block_reads_total{device="y
```

Produces in the Prometheus text output:

```
vmm_block_reads_total{device="x"} 0
vmm_admin_override{user="attacker"} 1
vmm_block_reads_total{device="y"} 0
...
```

This injects an entirely fabricated metric (`vmm_admin_override`) that a Prometheus scraper will ingest as legitimate. Downstream impact depends on what alerting rules and dashboards consume these metrics.

### 3. CTF Exploitation Sketch

```rust
// Attacker-controlled device name via VM creation API or CLI
let malicious_name = r#"x"}],"flag":"CTF{m3trics_1nj3cti0n}","block_devices":[{"device":"y"#;
metrics.add_block_device(malicious_name.to_string());

// When metrics are queried via control socket:
let json = metrics.to_json();
// json now contains "flag":"CTF{m3trics_1nj3cti0n}" at the top level
```

## Impact

- **Metrics poisoning**: Fake values for real metrics, hiding actual I/O errors or injecting phantom load
- **Alert manipulation**: Trigger or suppress alerts in monitoring systems (Amon, Grafana, PagerDuty)
- **Key shadowing in JSON consumers**: If an orchestrator trusts the metrics JSON to make scaling/migration decisions, injected keys could influence those decisions
- **Information disclosure**: If the JSON output is reflected to other tenants in a multi-tenant environment

## Fix

Escape the `device` string before interpolation. For JSON, escape `"` and `\`. For Prometheus, escape `"`, `\`, and `\n` in label values per the [Prometheus exposition format spec](https://prometheus.io/docs/instrumenting/exposition_formats/):

```rust
fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_prometheus_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}
```

Or better: use `serde_json` for JSON output, which handles escaping correctly.

## Other Notes

- The `VcpuMetrics` methods use `vcpu_id: i32`, a numeric type — not injectable.
- The `Relaxed` atomic ordering means `to_json()`/`to_prometheus()` can produce inconsistent snapshots across counters (e.g., `reads` from time T1, `read_bytes` from time T2), but this is a correctness issue, not a security vulnerability.
- No `unsafe` code, no raw pointers, no memory corruption vectors — Rust's type system handles the rest well.
