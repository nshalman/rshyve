# vmmnew TODO

Prioritized task list. See `tasks/` for detailed descriptions.

## Duplicates (removed)

- ~~05~~ = 101 (push_used barrier — same issue)
- ~~21~~ = 106 (EVENT_IDX disabled — 106 is more detailed)
- ~~105~~ consequence of 106 (test expects EVENT_IDX bit that's commented out)

## Not Implementing

| # | Task | Reason |
|---|------|--------|
| 07 | Migration TLS | Admin VLAN only. Migration still in dev. Premature. |
| 08 | Control socket auth | Unix socket uses filesystem perms. Standard for VMMs. |
| 09 | VNC XOR auth | Accessed through Triton proxy. Minimal impl, likely replaced. |
| 17 | Replace hand-rolled base64/crc32 | Working code. Crate deps not worth it for ~130 lines. |
| 18 | DSDT override path | Standard debugging mechanism. Root-only writable. |
| 26 | Magic numbers in direct_boot | Well-known x86 constants with inline comments. |
| 27 | Box::leak vm_name | Intentional `'static` pattern. Process-scoped. |
| 31 | strip=true | Intentional for production. Debug profile available. |
| 35 | Hardcoded DNS defaults | Triton injects DNS via mdata. Fallback only. |
| 208 | virtio-vsock | Future feature. Not needed for current work. |
| 209 | pvpanic | Nice to have. Not needed now. |

## Tier 1 -- Critical correctness/security (fix now)

- [x] **01** NVMe integer overflow — checked_mul/checked_add for sector arithmetic
- [x] **02** Virtio-blk sector overflow — checked_mul in do_pread/do_pwrite
- [x] **03** SubMapping alignment UB — alignment check in read<T>/write<T>
- [x] **100** pop_avail() missing Acquire fence — fence(Ordering::Acquire) after avail_idx read
- [x] **101** push_used() missing Release fence — fence(Ordering::Release) before used_idx write
- [x] **102** Reject indirect descriptors — return None in collect_chain for INDIRECT flag
- [x] **04** NVMe mem::forget fd leak — store File in struct, drop on controller drop
- [x] **11** getrandom() unchecked — assert on return value, panic on failure

## Tier 2 -- High-impact perf & correctness (fix soon)

- [x] **106** Enable EVENT_IDX (fix the race) — SeqCst fences + re-check loop per VirtIO 1.3 / QEMU pattern
- [x] **103** Completion len always 1 — do_pread/do_pwrite return (status, written_bytes)
- [x] **104** Sector bounds check incomplete — compute sectors_needed from chain data
- [x] **10** Spawn failure -> zombie VM — error handling, state restore on spawn failure
- [x] **14** AHCI silently becomes virtio-blk — bail with clear error message
- [x] **24** API version falls back to v18 — hard error instead of silent fallback

## Tier 3 -- Architecture & features (planned work)

- [ ] **201** MSI-X for virtio — biggest perf gap vs Propolis, per-queue interrupts
- [ ] **200** Lifecycle trait on all devices — required for clean pause/resume/migration
- [ ] **202** VirtioDevice trait update — 64-bit features, error returns, VqChange
- [ ] **12** Decompose 656-line run() — untestable monolith
- [ ] **203** Chain descriptor abstraction — enables indirect descriptors, cleaner devices
- [ ] **204** Block backend trait — extensibility for ramdisk/zvol/network block

## Tier 4 -- Medium improvements (when convenient)

- [ ] **16** Fix 27 silent error swallows — real failures masked in critical paths
- [ ] **20** Mutex held during I/O — serialization bottleneck for multi-vCPU
- [ ] **107** preadv/pwritev scatter-gather — syscall reduction
- [ ] **108** Eliminate Vec alloc for iodepth=1 — allocation churn on hot path
- [ ] **19** Volatile semantics inconsistency — compiler could optimize away guest writes
- [ ] **15** ACPI tables generated twice — wasted computation, easy fix
- [ ] **206** Viona ring state machine — kicks to unconfigured rings
- [ ] **205** Discard/write-zeroes — guest fstrim, space reclamation
- [ ] **207** Viona multiqueue — multi-core network perf
- [ ] **25** eprintln -> slog in migration — lost errors in production
- [ ] **13** Tests for critical files — risk exposure, heavy lift

## Tier 5 -- Low priority (backlog)

- [ ] **06** Unsafe ptr laundering in control.rs — code smell, not a real bug
- [ ] **22** Duplicate cache_api_version — DRY violation, trivial
- [ ] **23** VmRunState unknown -> Stopped — masks state bugs, low likelihood
- [ ] **29** Duplicate log message — cosmetic
- [ ] **30** slog-bunyan unused dep — build time only
- [ ] **32** Unused _log param — cosmetic
- [ ] **33** Serial stdout interleave — edge case
- [ ] **34** mdata silent byte drop — 1000-retry exhaustion is rare
- [ ] **28** VNC input silently dropped — known incomplete feature
- [ ] **109** Per-worker channels — optimization, measure first
- [ ] **110** Batch completions — optimization, measure first
- [ ] **111** Raise inline threshold — micro-optimization
