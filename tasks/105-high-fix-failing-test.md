# BUG: device_features_read test fails

**Priority:** High (test suite broken)
**File:** `crates/vmm-devices/src/virtio/pci.rs` line 575

## Problem

Test `device_features_read` asserts:
```rust
assert_eq!(val, 0x42 | bits::VIRTIO_F_RING_EVENT_IDX as u32);
```

But `bar_read()` at line 298-300 has EVENT_IDX commented out -- it only returns
`self.device.device_features()` = 0x42.

**Confirmed:** `cargo test -p vmm_devices -- virtio::pci::tests::device_features_read` FAILS:
```
left: 66
right: 536870978
```

## Fix

**If implementing before task 106 (EVENT_IDX):** Change the test assertion to match
current behavior:
```rust
assert_eq!(val, 0x42);
```

**If implementing together with task 106:** Leave assertion as-is; uncommenting EVENT_IDX
in pci.rs:298 makes it correct.

## Verification

- `cargo test -p vmm_devices -- virtio::pci::tests` all 11 tests pass
