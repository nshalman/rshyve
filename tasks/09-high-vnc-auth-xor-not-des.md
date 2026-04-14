# 09 — VNC auth uses XOR instead of DES

**Priority:** HIGH
**File:** `crates/vmm-devices/src/fbuf/rfb.rs:271-291`
**Category:** Security — broken authentication

## Problem

The `vnc_des_encrypt` function claims to implement VNC DES authentication but actually does XOR:

```rust
fn vnc_des_encrypt(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        key[i] = reverse_bits(b);
    }
    // "simple XOR-based transform"
    let mut result = [0u8; 16];
    for i in 0..16 {
        result[i] = challenge[i] ^ key[i % 8];
    }
    result
}
```

XOR "encryption" is trivially reversible. An attacker who observes the challenge (sent in cleartext on the wire) can recover the password by XORing the response with the challenge. Even a passive network observer can extract the VNC password.

The comment says `TODO: Replace with a proper DES implementation`. This is shipping broken crypto behind a TODO.

## Fix

Option A — Use the `des` crate for proper VNC DES:

```toml
# Cargo.toml
des = "0.8"
```

```rust
use des::cipher::{BlockEncrypt, KeyInit};
use des::Des;

fn vnc_des_encrypt(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        key[i] = reverse_bits(b);
    }

    let cipher = Des::new_from_slice(&key).expect("DES key is always 8 bytes");
    let mut result = [0u8; 16];
    result.copy_from_slice(challenge);

    // Encrypt each 8-byte half separately (VNC protocol)
    let (first, second) = result.split_at_mut(8);
    cipher.encrypt_block(first.into());
    cipher.encrypt_block(second.into());

    result
}
```

Option B — Remove VNC auth entirely and rely on network-level access control (the VNC socket is Unix domain, so only local access is possible). Set `security_type = SECURITY_NONE` always.

## Also fix: VNC challenge PRNG (related)

The challenge at line 227-238 uses `xorshift64` seeded from `SystemTime`. Replace with `getrandom()`:

```rust
let mut challenge = [0u8; 16];
super::super::virtio::rng::getrandom(&mut challenge);
// Or: libc::getrandom(challenge.as_mut_ptr().cast(), 16, 0);
```
