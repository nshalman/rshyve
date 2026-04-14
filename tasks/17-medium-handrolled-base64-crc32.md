# 17 — Hand-rolled base64 and CRC32 instead of crates

**Priority:** MEDIUM
**File:** `bin/vmm/src/mdata.rs:249-393`
**Category:** Code quality — unnecessary custom crypto/encoding

## Problem

~115 lines of custom base64 encode/decode with manual pending-byte buffer management, plus a 15-line hand-rolled CRC32 table lookup. Only one roundtrip test exists for the base64 implementation.

The base64 encoder manually manages 3-to-4 byte group conversion with a `pending` array and `pending_len` counter. Edge cases (padding, partial groups, invalid characters in decode) have minimal test coverage.

The CRC32 implementation builds a lookup table at runtime on every call.

## Fix

Add well-tested crates:

```toml
# bin/vmm/Cargo.toml
base64 = "0.22"
crc32fast = "1.4"
```

Replace the custom implementations:

```rust
// base64 encode
use base64::Engine;
let encoded = base64::engine::general_purpose::STANDARD.encode(data);

// base64 decode
let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;

// CRC32
let crc = crc32fast::hash(data);
```

Delete:
- `fn base64_encode` (~50 lines)
- `fn base64_decode` (~50 lines)
- `fn crc32` (~15 lines)
- The CRC32 lookup table constant

Keep the existing test but convert it to verify the crate-based implementation produces the same output.

## Testing

- Verify mdata V2 protocol still works end-to-end (base64-encoded responses with CRC32 trailer)
- Run existing `test_base64_roundtrip` against the new implementation
- Add edge-case tests: empty input, single byte, padding-required lengths
