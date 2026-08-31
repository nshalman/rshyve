// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SmartOS metadata agent for guest configuration.
//!
//! Implements the mdata V2 protocol over a UART (COM2), providing
//! network config, hostname, SSH keys, and user-script to the guest.
//!
//! # Protocol (V2)
//!
//! Request:  `V2 <body_len> <crc32_hex> <reqid> <command> [<b64_arg>]\n`
//! Response: `V2 <body_len> <crc32_hex> <reqid> <status> [<b64_data>]\n`
//!
//! Negotiation: client sends `NEGOTIATE V2\n`, server responds `V2_OK\n`

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use slog::{info, warn, Logger};
use vmm_devices::uart::lpc::LpcUart;

/// Metadata store — a simple key/value map.
pub struct MdataStore {
    entries: BTreeMap<String, String>,
}

impl MdataStore {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.entries.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|s| s.as_str())
    }

    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(|k| k.as_str()).collect()
    }
}

/// Start the metadata agent on COM2.
pub fn start_mdata_agent(
    uart: Arc<LpcUart>,
    store: Arc<MdataStore>,
    log: Logger,
) {
    thread::Builder::new()
        .name("mdata-agent".into())
        .spawn(move || {
            mdata_loop(&uart, &store, &log);
        })
        .expect("failed to spawn mdata agent thread");
}

/// Main protocol loop.
fn mdata_loop(uart: &LpcUart, store: &MdataStore, log: &Logger) {
    let mut line_buf = Vec::with_capacity(256);
    let mut v2_mode = false;

    info!(
        log,
        "mdata agent loop started, waiting for guest requests on COM2"
    );

    loop {
        match uart.output_byte() {
            Some(b'\n') => {
                let line = String::from_utf8_lossy(&line_buf).to_string();
                line_buf.clear();

                if line.is_empty() {
                    // Bare newline = serial reset. Respond with
                    // "invalid command" to confirm port is alive.
                    send_line(uart, "invalid command");
                    continue;
                }

                info!(log, "mdata: line received"; "line" => &line, "v2" => v2_mode, "len" => line.len());

                let response = if line == "NEGOTIATE V2" {
                    v2_mode = true;
                    info!(log, "mdata: V2 negotiated");
                    "V2_OK".to_string()
                } else if v2_mode && line.starts_with("V2 ") {
                    process_v2_frame(&line, store, log)
                } else {
                    // V1 or unknown — try V1
                    process_v1_command(&line, store, log)
                };

                info!(log, "mdata: sending response"; "response_len" => response.len(), "response" => &response);
                send_line(uart, &response);
            }
            Some(b'\r') => {
                // Ignore CR
            }
            Some(b) => {
                // Only buffer printable ASCII. Drop control chars
                // (ESC sequences from UEFI firmware probing COM2).
                if (0x20..0x7F).contains(&b) && line_buf.len() < 4096 {
                    line_buf.push(b);
                }
            }
            None => {
                thread::sleep(std::time::Duration::from_micros(100));
            }
        }
    }
}

/// Send a line (with \n terminator) to the guest via COM2 RX.
///
/// The UART RX FIFO is 16 bytes. We must pace writes to avoid
/// overflowing — wait for the guest to drain each byte before
/// pushing the next.
fn send_line(uart: &LpcUart, line: &str) {
    for b in line.as_bytes() {
        send_byte_paced(uart, *b);
    }
    send_byte_paced(uart, b'\n');
}

/// Send a single byte, retrying until the RX FIFO has space.
fn send_byte_paced(uart: &LpcUart, b: u8) {
    // Try up to ~100ms (1000 * 100us) for the guest to drain the FIFO
    for _ in 0..1000 {
        if uart.input_byte(b) {
            return;
        }
        thread::sleep(std::time::Duration::from_micros(100));
    }
    // Last resort — drop the byte (guest isn't reading)
    eprintln!("[mdata] dropped byte: retry limit exceeded");
}

/// Process a V2 framed request.
///
/// Frame: `V2 <body_len> <crc32_hex> <reqid> <command> [<b64_arg>]`
fn process_v2_frame(line: &str, store: &MdataStore, log: &Logger) -> String {
    // Parse: "V2 <len> <crc> <body>"
    let mut parts = line.splitn(4, ' ');
    let _v2 = parts.next(); // "V2"
    let _len_str = parts.next();
    let _crc_str = parts.next();
    let body = match parts.next() {
        Some(b) => b,
        None => return build_v2_error("00000000", "FAILURE"),
    };

    // Parse body: "<reqid> <command> [<b64_arg>]"
    let mut body_parts = body.splitn(3, ' ');
    let reqid = body_parts.next().unwrap_or("00000000");
    let command = body_parts.next().unwrap_or("");
    let b64_arg = body_parts.next();

    match command {
        "GET" => {
            let key = match b64_arg.and_then(b64_decode) {
                Some(k) => k,
                None => return build_v2_response(reqid, "FAILURE", None),
            };
            info!(log, "mdata: V2 GET"; "key" => &key);
            match store.get(&key) {
                Some(value) => build_v2_response(reqid, "SUCCESS", Some(value)),
                None => build_v2_response(reqid, "NOTFOUND", None),
            }
        }
        "KEYS" => {
            info!(log, "mdata: V2 KEYS");
            let keys_str = store.keys().join("\n");
            build_v2_response(reqid, "SUCCESS", Some(&keys_str))
        }
        "PUT" | "DELETE" => {
            // Read-only store
            build_v2_response(reqid, "FAILURE", None)
        }
        _ => {
            warn!(log, "mdata: unknown V2 command"; "command" => command);
            build_v2_response(reqid, "FAILURE", None)
        }
    }
}

/// Build a V2 response frame.
fn build_v2_response(reqid: &str, status: &str, data: Option<&str>) -> String {
    let body = match data {
        Some(d) if !d.is_empty() => {
            format!("{} {} {}", reqid, status, b64_encode(d))
        }
        _ => format!("{} {}", reqid, status),
    };
    let crc = crc32(&body);
    format!("V2 {} {:08x} {}", body.len(), crc, body)
}

fn build_v2_error(reqid: &str, status: &str) -> String {
    build_v2_response(reqid, status, None)
}

/// Process a V1 command (fallback).
fn process_v1_command(line: &str, store: &MdataStore, log: &Logger) -> String {
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts.get(1).copied().unwrap_or("");

    match cmd {
        "GET" => {
            let key = arg.trim();
            if key.is_empty() {
                return "invalid command".to_string();
            }
            match store.get(key) {
                Some(value) => {
                    info!(log, "mdata: V1 GET"; "key" => key);
                    let mut resp = String::from("SUCCESS\n");
                    for line in value.lines() {
                        if line.starts_with('.') {
                            resp.push('.');
                        }
                        resp.push_str(line);
                        resp.push('\n');
                    }
                    resp.push('.');
                    resp
                }
                None => "NOTFOUND".to_string(),
            }
        }
        "KEYS" => {
            let keys = store.keys();
            let mut resp = String::from("SUCCESS\n");
            for key in &keys {
                resp.push_str(key);
                resp.push('\n');
            }
            resp.push('.');
            resp
        }
        _ => "invalid command".to_string(),
    }
}

// -- Base64 and CRC32 helpers --

fn b64_encode(s: &str) -> String {
    use std::io::Write;
    let mut buf = Vec::with_capacity(s.len() * 4 / 3 + 4);
    {
        let mut enc = Base64Encoder::new(&mut buf);
        enc.write_all(s.as_bytes()).unwrap();
        enc.finish().unwrap();
    }
    String::from_utf8(buf).unwrap()
}

fn b64_decode(s: &str) -> Option<String> {
    let bytes = base64_decode_bytes(s)?;
    String::from_utf8(bytes).ok()
}

fn base64_decode_bytes(input: &str) -> Option<Vec<u8>> {
    const TABLE: [u8; 128] = {
        let mut t = [0xFFu8; 128];
        let mut i = 0u8;
        while i < 26 {
            t[(b'A' + i) as usize] = i;
            i += 1;
        }
        i = 0;
        while i < 26 {
            t[(b'a' + i) as usize] = 26 + i;
            i += 1;
        }
        i = 0;
        while i < 10 {
            t[(b'0' + i) as usize] = 52 + i;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let (a, b, c, d) = (
            *TABLE.get(*bytes.get(i)? as usize)?,
            *TABLE.get(*bytes.get(i + 1)? as usize)?,
            *TABLE.get(*bytes.get(i + 2)? as usize)?,
            *TABLE.get(*bytes.get(i + 3)? as usize)?,
        );
        if a == 0xFF || b == 0xFF || c == 0xFF || d == 0xFF {
            return None;
        }
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        i += 4;
    }
    let rem = bytes.len() - i;
    if rem >= 2 {
        let a = *TABLE.get(*bytes.get(i)? as usize)?;
        let b = *TABLE.get(*bytes.get(i + 1)? as usize)?;
        if a == 0xFF || b == 0xFF {
            return None;
        }
        out.push((a << 2) | (b >> 4));
        if rem >= 3 {
            let c = *TABLE.get(*bytes.get(i + 2)? as usize)?;
            if c == 0xFF {
                return None;
            }
            out.push((b << 4) | (c >> 2));
        }
    }
    Some(out)
}

struct Base64Encoder<'a> {
    buf: &'a mut Vec<u8>,
    pending: [u8; 3],
    pending_len: usize,
}

impl<'a> Base64Encoder<'a> {
    fn new(buf: &'a mut Vec<u8>) -> Self {
        Self {
            buf,
            pending: [0; 3],
            pending_len: 0,
        }
    }
    fn finish(mut self) -> Result<(), std::io::Error> {
        const CHARS: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        if self.pending_len == 1 {
            let a = self.pending[0];
            self.buf.push(CHARS[(a >> 2) as usize]);
            self.buf.push(CHARS[((a & 0x03) << 4) as usize]);
            self.buf.push(b'=');
            self.buf.push(b'=');
        } else if self.pending_len == 2 {
            let (a, b) = (self.pending[0], self.pending[1]);
            self.buf.push(CHARS[(a >> 2) as usize]);
            self.buf.push(CHARS[((a & 0x03) << 4 | b >> 4) as usize]);
            self.buf.push(CHARS[((b & 0x0F) << 2) as usize]);
            self.buf.push(b'=');
        }
        self.pending_len = 0;
        Ok(())
    }
}

impl<'a> std::io::Write for Base64Encoder<'a> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        const CHARS: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        // First, fill pending buffer
        while self.pending_len < 3 && i < data.len() {
            self.pending[self.pending_len] = data[i];
            self.pending_len += 1;
            i += 1;
        }
        if self.pending_len == 3 {
            let (a, b, c) = (self.pending[0], self.pending[1], self.pending[2]);
            self.buf.push(CHARS[(a >> 2) as usize]);
            self.buf.push(CHARS[((a & 0x03) << 4 | b >> 4) as usize]);
            self.buf.push(CHARS[((b & 0x0F) << 2 | c >> 6) as usize]);
            self.buf.push(CHARS[(c & 0x3F) as usize]);
            self.pending_len = 0;
        }
        // Process remaining full triplets
        while i + 2 < data.len() {
            let (a, b, c) = (data[i], data[i + 1], data[i + 2]);
            self.buf.push(CHARS[(a >> 2) as usize]);
            self.buf.push(CHARS[((a & 0x03) << 4 | b >> 4) as usize]);
            self.buf.push(CHARS[((b & 0x0F) << 2 | c >> 6) as usize]);
            self.buf.push(CHARS[(c & 0x3F) as usize]);
            i += 3;
        }
        // Buffer remaining
        while i < data.len() {
            self.pending[self.pending_len] = data[i];
            self.pending_len += 1;
            i += 1;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// CRC32 (ISO 3309 / ITU-T V.42, same as zlib).
fn crc32(data: &str) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data.as_bytes() {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Build the metadata store from zone config / CLI flags.
pub fn build_store_from_zone_config(
    hostname: &str,
    uuid: Option<&str>,
    nics_json: Option<&str>,
    resolvers_json: Option<&str>,
    ssh_keys: Option<&str>,
    root_pw: Option<&str>,
) -> MdataStore {
    let mut store = MdataStore::new();

    store.set("sdc:hostname", hostname);

    // sdc:uuid is required by cloud-init's SmartOS datasource for
    // instance identification. Without it, _get_data() fails and
    // the datasource is not persisted.
    store.set("sdc:uuid", uuid.unwrap_or(hostname));

    if let Some(nics) = nics_json {
        store.set("sdc:nics", nics);
    }

    if let Some(resolvers) = resolvers_json {
        store.set("sdc:resolvers", resolvers);
    } else {
        // Default public resolvers used when no --resolvers flag is
        // passed.  Acceptable for dev/test; production zones should
        // always supply explicit resolvers via the CLI.
        store.set("sdc:resolvers", r#"["8.8.8.8","1.1.1.1"]"#);
    }

    if let Some(keys) = ssh_keys {
        store.set("root_authorized_keys", keys);
    }

    if let Some(pw) = root_pw {
        store.set("root_pw", pw);
    }

    // Keys that cloud-init's SmartOS datasource expects to exist
    store.set("sdc:maintain_resolvers", "true");
    store.set("sdc:routes", "[]");
    store.set("sdc:dns_domain", "");
    store.set("user-script", "");
    store.set("user-data", "");
    store.set("sdc:vendor-data", "");
    store.set("sdc:operator-script", "");

    store
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_value() {
        // "123456789" → 0xCBF43926
        assert_eq!(crc32("123456789"), 0xCBF4_3926);
    }

    #[test]
    fn b64_roundtrip() {
        let orig = "hello world";
        let encoded = b64_encode(orig);
        let decoded = b64_decode(&encoded).unwrap();
        assert_eq!(decoded, orig);
    }

    #[test]
    fn v2_response_format() {
        let resp = build_v2_response("aabbccdd", "SUCCESS", Some("test"));
        assert!(resp.starts_with("V2 "));
        // Parse it back
        let mut parts = resp.splitn(4, ' ');
        assert_eq!(parts.next(), Some("V2"));
        let len: usize = parts.next().unwrap().parse().unwrap();
        let crc_hex = parts.next().unwrap();
        let body = parts.next().unwrap();
        assert_eq!(body.len(), len);
        let crc_val = u32::from_str_radix(crc_hex, 16).unwrap();
        assert_eq!(crc32(body), crc_val);
    }

    #[test]
    fn v1_get_existing() {
        let mut store = MdataStore::new();
        store.set("test-key", "test-value");
        let log = Logger::root(slog::Discard, slog::o!());
        let resp = process_v1_command("GET test-key", &store, &log);
        assert_eq!(resp, "SUCCESS\ntest-value\n.");
    }

    #[test]
    fn v1_get_missing() {
        let store = MdataStore::new();
        let log = Logger::root(slog::Discard, slog::o!());
        let resp = process_v1_command("GET missing", &store, &log);
        assert_eq!(resp, "NOTFOUND");
    }
}
