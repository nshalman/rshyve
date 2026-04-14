# Vulnerability Analysis: `mdata.rs` — SmartOS Metadata Agent

## Finding 1: V2 CRC32 and Length Validation Completely Missing (High)

**Affected function:** `process_v2_frame()` (lines ~136-145)

**Root cause:** The CRC32 checksum and body length fields are parsed but **explicitly discarded** — the variables are prefixed with `_` to suppress unused warnings:

```rust
let _len_str = parts.next();   // never validated
let _crc_str = parts.next();   // never validated
```

Meanwhile, the **response builder** correctly computes CRC and length:

```rust
fn build_v2_response(...) -> String {
    let crc = crc32(&body);
    format!("V2 {} {:08x} {}", body.len(), crc, body)
}
```

This asymmetry means incoming requests have zero integrity protection while responses are properly signed. An attacker (or MITM on the UART bus) can inject or modify any V2 request with arbitrary garbage in the CRC/length fields.

**Exploitation:**
```
V2 0 00000000 deadbeef GET cm9vdF9wdw==
```
This request claims body length 0 and CRC 0x00000000 — both obviously wrong — but the server processes it happily and returns the `root_pw` value.

---

## Finding 2: V2→V1 Protocol Downgrade (Medium)

**Affected function:** `mdata_loop()` (lines ~88-95)

**Root cause:** After V2 is negotiated (`v2_mode = true`), any line that doesn't start with `"V2 "` falls through to V1 processing:

```rust
} else if v2_mode && line.starts_with("V2 ") {
    process_v2_frame(&line, store, log)
} else {
    // V1 or unknown — try V1
    process_v1_command(&line, store, log)    // ← always reachable!
}
```

V1 takes raw key names (no base64, no CRC, no request IDs). If V2 was intended to enforce any additional security properties, they're trivially bypassed.

**Exploitation:**
```
NEGOTIATE V2       →  V2_OK
GET root_pw        →  SUCCESS\nhunter2\n.
```
The second line doesn't start with `"V2 "`, so V1 handles it — even though V2 is negotiated.

---

## Finding 3: Integer Overflow in Base64 Decoder — DoS in Debug Builds (Medium)

**Affected function:** `base64_decode_bytes()` (lines ~224-226)

**Root cause:** The decoder performs bit shifts on `u8` values without masking:

```rust
out.push((a << 2) | (b >> 4));    // OK: max 252 | 3 = 255
out.push((b << 4) | (c >> 2));    // BUG: b=16 → 16<<4 = 256, overflows u8!
out.push((c << 6) | d);           // BUG: c=4  → 4<<6  = 256, overflows u8!
```

Base64 values range 0–63. `b << 4` overflows for b >= 16 (base64 chars `Q` through `/`). `c << 6` overflows for c >= 4 (base64 chars `E` through `/`).

- **Debug build:** Rust panics on integer overflow → **crashes the mdata agent thread**, killing metadata service for the guest.
- **Release build:** Wrapping produces correct results (mathematically equivalent to masking), so functionally safe but relying on implicit overflow semantics.

**Exploitation (debug build):**
```
V2 99 00000000 aabb GET aGVsbG8=
```
Decoding `aGVsbG8=` ("hello") requires `b << 4` with b=29, which is > 16. **Panics in debug mode**, killing the mdata agent.

Compare to the **encoder** which correctly masks:
```rust
self.buf.push(CHARS[((b & 0x0F) << 2 | c >> 6) as usize]);  // masked ✓
```

---

## Finding 4: Request ID Not Validated — Response Frame Confusion (Low)

**Affected function:** `process_v2_frame()` / `build_v2_response()`

**Root cause:** The `reqid` is taken verbatim from untrusted input and interpolated into the response body:

```rust
let reqid = body_parts.next().unwrap_or("00000000");  // no validation
// ...
format!("{} {} {}", reqid, status, b64_encode(d))      // into response
```

A reqid containing spaces breaks the space-delimited response framing. While the CRC/length in the **response** are computed correctly (so a proper client would detect the mismatch), a naive client that splits on spaces before validating CRC would misparse the response.

---

## Summary

| # | Vulnerability | Severity | Impact |
|---|---|---|---|
| 1 | CRC32 + length validation skipped on V2 requests | **High** | Request tampering/injection undetected |
| 2 | V1 commands accepted after V2 negotiation | **Medium** | Protocol downgrade bypasses V2 framing |
| 3 | u8 overflow in base64 decoder shifts | **Medium** | DoS (panic) in debug builds |
| 4 | Unsanitized reqid in response framing | **Low** | Response parsing confusion |

**Most likely CTF flag:** Finding 1 (CRC bypass) combined with Finding 2 (downgrade) — the server advertises V2 integrity but enforces none of it. The `_crc_str` variable name is practically a signpost.
