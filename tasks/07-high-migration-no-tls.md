# 07 — Migration sends all guest RAM over unencrypted TCP

**Priority:** HIGH
**Files:** `crates/vmm-migrate/src/source.rs:53`, `crates/vmm-migrate/src/destination.rs:47`
**Category:** Security — confidentiality/integrity/authentication breach

## Problem

Live migration uses a plain TCP WebSocket (`tokio-tungstenite` with no TLS). All guest memory (RAM, CPU registers, device state) is sent in cleartext. A network attacker can:

1. **Intercept** all guest memory (passwords, keys, application data)
2. **Modify** guest memory/state in transit (inject code, corrupt data)
3. **Impersonate** the destination and receive the entire VM

The control socket `migrate-source` command takes an arbitrary `target_addr` — a local attacker who can connect to the control socket can redirect migration to any host they control.

## Fix

### Phase 1 — TLS (minimum viable)

Replace `tokio::net::TcpStream` with `tokio_rustls` (or `tokio-native-tls`):

**source.rs:**
```rust
let tcp = tokio::net::TcpStream::connect(&target_addr).await?;
let tls = tokio_rustls::TlsConnector::from(tls_config);
let stream = tls.connect(server_name, tcp).await?;
let ws = tokio_tungstenite::client_async("ws://migration", stream).await?.0;
```

**destination.rs:**
```rust
let tcp = listener.accept().await?.0;
let tls = tokio_rustls::TlsAcceptor::from(tls_config);
let stream = tls.accept(tcp).await?;
let ws = tokio_tungstenite::accept_async(stream).await?;
```

### Phase 2 — Pre-shared key

Add a migration token to the control socket command:

```json
{"command": "migrate-source", "target": "10.0.0.2:8080", "token": "..."}
```

The source sends the token in the WebSocket handshake. The destination validates it before accepting any data.

### Dependencies

Add to `Cargo.toml`:
```toml
tokio-rustls = "0.26"
rustls = "0.23"
```

Or use the simpler `tokio-native-tls` if the platform has OpenSSL.
