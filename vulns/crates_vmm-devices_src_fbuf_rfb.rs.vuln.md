# RFB Protocol Vulnerability Report

## Vulnerability 1: Broken VNC Authentication — XOR Substituted for DES (Critical)

**Affected function:** `vnc_des_encrypt` (lines ~234-249)

### Root Cause

The VNC authentication "encrypts" the challenge using **XOR instead of DES**:

```rust
for i in 0..16 {
    result[i] = challenge[i] ^ key[i % 8];
}
```

The 16-byte challenge is sent to the client **in cleartext**. The expected response is `challenge XOR key`. Any network observer (or MITM) who captures one successful auth exchange can recover the password:

```
key[i] = challenge[i] XOR response[i]    (for i in 0..8)
password[i] = reverse_bits(key[i])
```

### Exploitation Strategy

1. Sniff a single VNC authentication exchange (challenge + response).
2. XOR them together to get the 8 bit-reversed key bytes.
3. Reverse bits on each byte to recover the plaintext password (up to 8 chars).

### Proof of Concept

```python
def recover_password(challenge: bytes, response: bytes) -> str:
    """Recover VNC password from a single captured auth exchange."""
    def reverse_bits(b):
        r = 0
        for _ in range(8):
            r = (r << 1) | (b & 1)
            b >>= 1
        return r

    key = bytes(c ^ r for c, r in zip(challenge[:8], response[:8]))
    password = bytes(reverse_bits(b) for b in key)
    return password.rstrip(b'\x00').decode('ascii', errors='replace')

# Example: challenge and response captured from the wire
challenge = bytes.fromhex("aabbccdd11223344aabbccdd11223344")
response  = bytes.fromhex("...")  # captured from client
print(recover_password(challenge, response))
```

**Severity:** Critical — full auth bypass from passive observation of one session.

---

## Vulnerability 2: Predictable Challenge PRNG (High)

**Affected function:** `vnc_auth_challenge` (lines ~198-213)

### Root Cause

The 16-byte challenge is generated from a **nanosecond timestamp seeded xorshift64**:

```rust
let seed = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_nanos();
let mut rng = seed as u64;
```

An attacker who knows the server time within ~1 second can enumerate ~10^9 candidate seeds, predict the challenge, and precompute expected responses for candidate passwords — enabling an **offline brute-force without even observing a real session**.

Combined with Vuln 1 (XOR not DES), a single password guess per seed candidate is enough.

**Severity:** High — enables offline attack without traffic capture.

---

## Vulnerability 3: Unbounded Read in CutText Drain Loop (Medium)

**Affected function:** `read_client_message`, `CLIENT_CUT_TEXT` arm (lines ~327-343)

### Root Cause

A malicious client sends `len = 0xFFFFFFFF` (4 GiB) in the CutText header. The buffer is capped at 1 MiB, but the **drain loop reads the remaining ~4 GiB** from the socket in 4 KB chunks:

```rust
let mut remaining = len - capped;  // ~4 GiB
while remaining > 0 {
    let chunk = remaining.min(drain.len());
    r.read_exact(&mut drain[..chunk])?;  // blocks until data arrives
    remaining -= chunk;
}
```

The server thread is pinned reading data for the duration. The client can trickle data slowly (slowloris-style) to hold the connection indefinitely.

**Severity:** Medium — single-threaded DoS; the VNC connection handler is blocked.

---

## Vulnerability 4: SetEncodings Uncapped Allocation (Low)

**Affected function:** `read_client_message`, `CLIENT_SET_ENCODINGS` arm (lines ~302-310)

The initial `Vec::with_capacity(count.min(64))` caps preallocation, but the loop pushes up to `u16::MAX` (65535) entries = 256 KB per message. Repeated messages can pressure the allocator. Minor DoS only.

**Severity:** Low.

---

## Summary

| # | Vulnerability | Severity | Impact |
|---|---|---|---|
| 1 | XOR instead of DES in VNC auth | **Critical** | Password recovery from single packet capture |
| 2 | Predictable PRNG for challenge | **High** | Offline brute-force without traffic capture |
| 3 | Unbounded drain loop in CutText | **Medium** | Connection-tying DoS |
| 4 | Uncapped encoding list allocation | **Low** | Minor memory pressure |

**The CTF flag is almost certainly in Vuln 1** — the XOR "encryption" is the kind of deliberate-looking crypto flaw CTFs love. Capture a challenge/response pair, XOR them, reverse the bits, and you have the password.
