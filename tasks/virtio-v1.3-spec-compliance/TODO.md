# Virtio v1.3 Spec Compliance -- Task Tracker

This document tracks work to bring the VMM's virtio implementation into
compliance with the virtio v1.3 specification (`docs/virtio-v1.3-csd01.html`).
Tasks are ordered by phase, with earlier phases being lower-risk, self-contained
fixes and later phases being larger architectural changes. Each task has a
corresponding `NN-*.md` file in this directory with full implementation details.

---

## Phase 1: Quick fixes (no API changes, no new features)

- [x] **01** -- Fix avail_end off-by-2 in queue address calculation | _1 line_ | No deps
- [x] **02** -- Mask guest features to offered subset | _3 lines_ | No deps
- [x] **03** -- Add flags-based notification suppression (non-EVENT_IDX) | _~20 lines_ | No deps
- [x] **04** -- Move EVENT_IDX from transport to device backends | _~15 lines_ | No deps

## Phase 2: Feature negotiation and status correctness

- [x] **05** -- Add FEATURES_OK validation on device status write | _~15 lines_ | After 02, 04
- [x] **06** -- Gate indirect descriptor rejection on feature negotiation | _~10 lines_ | No deps
- [x] **07** -- Add DEVICE_NEEDS_RESET signaling mechanism | _~25 lines_ | After 12

## Phase 3: Block device spec gaps

- [x] **08** -- Expose seg_max and size_max in block config space | _~30 lines_ | No deps
- [x] **09** -- Add GET_ID command support | _~20 lines_ | No deps
- [x] **10** -- Use fdatasync instead of fsync for FLUSH | _2 lines_ | No deps
- [x] **11** -- Implement DISCARD and WRITE_ZEROES commands | _~80 lines_ | After 08

## Phase 4: Interrupt and notification improvements

- [x] **12** -- Add config change interrupt support (ISR_CFG_CHANGE) | _~15 lines_ | No deps
- [x] **13** -- Implement indirect descriptor support | _~60 lines_ | After 06

## Phase 5: Architecture -- widen feature trait to u64

- [x] **14** -- Widen VirtioDevice trait features to u64 | _~40 lines_ | After 02, 04

## Phase 6: Major features

- [ ] **15** -- Modern PCI transport (capability structures) | _~500 lines_ | After 14, 16 _(deferred: legacy+MSI-X works for all current guests)_
- [x] **16** -- MSI-X interrupt support | _~150 lines_ | No deps
- [x] **17** -- Block device multi-queue (MQ) | _~100 lines_ | After 15
- [x] **18** -- Packed virtqueues | _~400 lines_ | After 14, 15

---

## Dependency Graph

```
01 (avail_end fix)          standalone
02 (mask features)          standalone
03 (flags notification)     standalone
04 (EVENT_IDX per-device)   standalone
                              |
                  +-----------+-----------+
                  |                       |
                  v                       v
       05 (FEATURES_OK)          14 (u64 features)
          needs 02, 04               needs 02, 04
                                          |
                                          v
                                 15 (modern PCI transport)
                                     needs 14, 16
                                          |
                              +-----------+-----------+
                              |                       |
                              v                       v
                    17 (block MQ)            18 (packed virtqueues)

06 (indirect gating)     standalone
        |
        v
13 (indirect descriptors)
        needs 06

07 (DEVICE_NEEDS_RESET)  --> needs 12 (config change interrupt)

08 (seg_max/size_max)    standalone
        |
        v
11 (DISCARD/WRITE_ZEROES)
        needs 08

09 (GET_ID)              standalone
10 (fdatasync)           standalone
12 (config change ISR)   standalone
16 (MSI-X)               standalone
```

### Execution order (suggested)

1. 01, 02, 03, 04, 06, 08, 09, 10, 12 -- all independent, can be done in any order
2. 05 -- after 02 and 04
3. 07 -- after 12
4. 11 -- after 08
5. 13 -- after 06
6. 14 -- after 02 and 04
7. 16 -- independent, but needed before 15
8. 15 -- after 14 and 16
9. 17, 18 -- after 15

---

## Quick Wins (3 lines or fewer)

These tasks can each be completed with minimal code change and near-zero risk:

| Task | Description | Lines | Risk |
|------|-------------|-------|------|
| 01 | Fix `avail_end` constant from `4` to `6` | 1 | None (page alignment absorbs error) |
| 10 | Replace `fsync` with `fdatasync` (2 call sites) | 2 | None (fdatasync is subset of fsync) |
| 02 | Mask `guest_features &= offered` on feature write | 3 | Low (strictly more restrictive) |

---

## Notes

- Spec reference: `docs/virtio-v1.3-csd01.html`
- All 18 tasks have detail files with implementation guidance.
- Test commands assume the workspace layout: `cargo test -p vmm-devices`
- Platform: illumos (some syscalls differ from Linux; see task 11 for `fallocate` vs `fcntl(F_FREESP)` notes).
