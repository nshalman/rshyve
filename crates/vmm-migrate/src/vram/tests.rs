// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::Deserialize;
use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStatePayload, FbufMigrateState, FbufMigrateVram,
    WireBdf,
};

use super::*;
use crate::protocol::{
    PROTOCOL_RON, PROTOCOL_RON_V10, PROTOCOL_RON_V8, PROTOCOL_RON_V9,
};

const PAGE: usize = 4096;

fn log() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

/// One page, as the device cuts it: a few bytes, then zeros.
fn raw() -> Vec<u8> {
    let mut raw = vec![0u8; PAGE];
    raw[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    raw
}

fn payload(vram: Option<FbufMigrateVram>) -> Vec<DeviceStatePayload> {
    vec![DeviceStatePayload {
        bdf: WireBdf {
            bus: 0,
            dev: 30,
            func: 0,
        },
        state: Some(DeviceMigrateState::Fbuf(FbufMigrateState {
            width: 1024,
            height: 768,
            vram,
            ..Default::default()
        })),
    }]
}

/// What the device exports.
fn exported() -> Vec<DeviceStatePayload> {
    let raw = raw();
    payload(Some(FbufMigrateVram {
        len: raw.len() as u32,
        lz4: lz4_flex::block::compress(&raw),
        zstd: Vec::new(),
    }))
}

fn vram_of(p: &[DeviceStatePayload]) -> &FbufMigrateVram {
    match &p[0].state {
        Some(DeviceMigrateState::Fbuf(f)) => f.vram.as_ref().expect("vram"),
        other => panic!("not fbuf: {other:?}"),
    }
}

fn send(protocol: &str) -> String {
    let mut p = exported();
    fit_for_peer(protocol, &mut p, &log()).expect("fit");
    ron::ser::to_string(&p).expect("serialize")
}

fn receive(text: &str) -> Vec<u8> {
    let mut p: Vec<DeviceStatePayload> =
        ron::de::from_str(text).expect("deserialize");
    accept(&mut p).expect("accept");
    let v = vram_of(&p);
    assert!(v.zstd.is_empty());
    lz4_flex::block::decompress(&v.lz4, v.len as usize).expect("lz4")
}

/// The VRAM as each v10 build declared it. Neither denies unknown
/// fields, which is what lets one payload serve both.
mod old {
    use serde::de::{Deserializer, Visitor};
    use serde::Deserialize;
    use std::fmt;

    #[derive(Deserialize)]
    pub struct ZstdBuild {
        pub len: u32,
        #[serde(deserialize_with = "bytes")]
        pub zstd: Vec<u8>,
    }

    #[derive(Deserialize)]
    pub struct Lz4Build {
        pub len: u32,
        #[serde(deserialize_with = "bytes")]
        pub lz4: Vec<u8>,
    }

    fn bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bytes")
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
        }
        d.deserialize_byte_buf(V)
    }
}

/// The fbuf payload around one VRAM, as any v10 build reads it.
#[derive(Deserialize)]
struct OldPayload<V> {
    state: Option<OldState<V>>,
}

#[derive(Deserialize)]
enum OldState<V> {
    Fbuf(OldFbuf<V>),
}

#[derive(Deserialize)]
struct OldFbuf<V> {
    vram: Option<V>,
}

fn old_vram<V: for<'de> Deserialize<'de>>(text: &str) -> V {
    let p: Vec<OldPayload<V>> = ron::de::from_str(text).expect("old build");
    match p.into_iter().next().and_then(|p| p.state) {
        Some(OldState::Fbuf(f)) => f.vram.expect("vram"),
        None => panic!("no state"),
    }
}

// Golden v10 payloads, one per build, for the page `raw()` gives.

/// A zstd build (before #43 moved the VRAM to LZ4).
const V10_ZSTD: &str = concat!(
    "[(bdf:(bus:0,dev:30,func:0),state:Some(Fbuf((pci:(command:0,bar_addrs:(0,0,0,0,0,0),intr_line:None),width:1024,height:768,",
    "vram:Some((len:4096,zstd:\"KLUv/WAAD4UAAEgBAgMEBQYHCAABAPT3wRE=\"))))))]"
);

/// An LZ4 build (#43 to this change).
const V10_LZ4: &str = concat!(
    "[(bdf:(bus:0,dev:30,func:0),state:Some(Fbuf((pci:(command:0,bar_addrs:(0,0,0,0,0,0),intr_line:None),width:1024,height:768,",
    "vram:Some((len:4096,lz4:\"nwECAwQFBgcIAAEA////////////////////7WAAAAAAAAA=\"))))))]"
);

#[test]
fn both_v10_builds_decode_on_this_build() {
    assert_eq!(receive(V10_ZSTD), raw());
    assert_eq!(receive(V10_LZ4), raw());
}

#[test]
fn an_older_peer_gets_a_payload_both_v10_builds_read() {
    for peer in [PROTOCOL_RON_V10, PROTOCOL_RON_V9, PROTOCOL_RON_V8] {
        let text = send(peer);
        let z: old::ZstdBuild = old_vram(&text);
        let out = zstd::bulk::decompress(&z.zstd, z.len as usize).expect("z");
        assert_eq!(out, raw(), "{peer}");
        let l: old::Lz4Build = old_vram(&text);
        let out =
            lz4_flex::block::decompress(&l.lz4, l.len as usize).expect("lz4");
        assert_eq!(out, raw(), "{peer}");
        assert_eq!(receive(&text), raw(), "{peer}");
    }
}

#[test]
fn a_v12_peer_gets_lz4_only() {
    let text = send(PROTOCOL_RON);
    assert!(!text.contains("zstd"), "{text}");
    assert!(text.contains("lz4:\""), "{text}");
    assert_eq!(receive(&text), raw());
}

/// Pins the field names and order this build sends to a v10 peer.
#[test]
fn the_v10_vram_shape_is_pinned() {
    let text = send(PROTOCOL_RON_V10);
    let at = text.find("vram:Some((").expect("vram") + "vram:Some((".len();
    let vram = &text[at..];
    assert!(vram.starts_with("len:4096,lz4:\""), "{vram}");
    assert!(vram.contains("\",zstd:\""), "{vram}");
}

#[test]
fn a_vram_too_big_for_an_older_peer_is_left_behind() {
    // Noise does not compress, so its zstd form is over the cap.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let raw: Vec<u8> = (0..FbufMigrateVram::WIRE_CAP + PAGE)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let mut p = payload(Some(FbufMigrateVram {
        len: raw.len() as u32,
        lz4: lz4_flex::block::compress(&raw),
        zstd: Vec::new(),
    }));
    fit_for_peer(PROTOCOL_RON_V10, &mut p, &log()).expect("fit");
    match &p[0].state {
        Some(DeviceMigrateState::Fbuf(f)) => assert!(f.vram.is_none()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_hostile_zstd_vram_is_refused() {
    let raw = raw();
    let frame = zstd::bulk::compress(&raw, 1).expect("z");
    for (len, zstd) in [
        (FbufMigrateVram::MAX_LEN + 1, frame.clone()),
        // The header says more than the frame holds.
        (2 * PAGE as u32, frame.clone()),
        // The header says less.
        (16, frame),
        (PAGE as u32, vec![1, 2, 3]),
    ] {
        let mut p = payload(Some(FbufMigrateVram {
            len,
            lz4: Vec::new(),
            zstd,
        }));
        assert!(accept(&mut p).is_err(), "len {len}");
    }
}
