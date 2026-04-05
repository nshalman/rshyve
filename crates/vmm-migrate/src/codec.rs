// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration codec for WebSocket-based data transfer.
//!
//! Message types for the migration protocol, using RON serialization
//! for structured data and raw binary for memory pages.
//!
//! # Wire format
//!
//! Each WebSocket binary frame has the payload bytes followed by a
//! single tag byte identifying the [`MessageType`]. The tag is at the
//! *end* so that variable-length payloads can be read without a
//! separate length prefix.

use serde::{Deserialize, Serialize};

/// Message types in the migration protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Acknowledgement / success.
    Okay = 0,
    /// Error with description.
    Error = 1,
    /// RON-serialized structured data.
    Serialized = 2,
    /// Raw binary blob.
    Blob = 3,
    /// Memory page data.
    Page = 4,
    /// Memory region query.
    MemQuery = 5,
    /// Memory region offer (dirty bitmap).
    MemOffer = 6,
    /// Request specific pages.
    MemFetch = 7,
    /// Page data transfer.
    MemXfer = 8,
    /// End of memory transfer phase.
    MemEnd = 9,
    /// Memory transfer complete acknowledgement.
    MemDone = 10,
}

impl MessageType {
    /// Convert a raw tag byte to a MessageType.
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Okay),
            1 => Some(Self::Error),
            2 => Some(Self::Serialized),
            3 => Some(Self::Blob),
            4 => Some(Self::Page),
            5 => Some(Self::MemQuery),
            6 => Some(Self::MemOffer),
            7 => Some(Self::MemFetch),
            8 => Some(Self::MemXfer),
            9 => Some(Self::MemEnd),
            10 => Some(Self::MemDone),
            _ => None,
        }
    }
}

/// Errors that can occur during migration.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum MigrateError {
    #[error("protocol negotiation failed: {0}")]
    ProtocolMismatch(String),
    #[error("preamble mismatch: {0}")]
    PreambleMismatch(String),
    #[error("unexpected message: expected {expected}, got {got}")]
    UnexpectedMessage { expected: String, got: String },
    #[error("codec error: {0}")]
    Codec(String),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("vmm-data error: {0}")]
    VmmData(String),
    #[error("state import/export error: {0}")]
    State(String),
    #[error("remote error: {0}")]
    RemoteError(String),
    #[error("phase error: {0}")]
    Phase(String),
}

impl From<std::io::Error> for MigrateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for MigrateError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(e.to_string())
    }
}

impl From<ron::Error> for MigrateError {
    fn from(e: ron::Error) -> Self {
        Self::Codec(format!("RON deserialize: {e}"))
    }
}

impl From<ron::error::SpannedError> for MigrateError {
    fn from(e: ron::error::SpannedError) -> Self {
        Self::Codec(format!("RON deserialize: {e}"))
    }
}

/// Time synchronization data for TSC and boot hrtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeData {
    /// Raw bytes of vdi_time_info_v1.
    pub vmm_time: Vec<u8>,
}

/// Aggregated device state for a single Serialized message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    /// Per-vCPU state payloads, indexed by vCPU ID.
    pub vcpus: Vec<VcpuStatePayload>,
    /// System device state payloads (IOAPIC, ATPIT, etc.).
    pub devices: Vec<DevicePayload>,
}

/// State data for a single vCPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuStatePayload {
    pub vcpuid: i32,
    /// VDC_REGISTER data.
    pub registers: Vec<u8>,
    /// VDC_MSR data.
    pub msrs: Vec<u8>,
    /// VDC_FPU data.
    pub fpu: Vec<u8>,
    /// VDC_LAPIC data.
    pub lapic: Vec<u8>,
    /// VDC_VMM_ARCH data.
    pub vmm_arch: Vec<u8>,
    /// Run state (VRS_HALT, VRS_INIT, VRS_RUN, etc.)
    #[serde(default = "default_run_state")]
    pub run_state: u32,
    /// SIPI vector (for APs in INIT/SIPI wait)
    #[serde(default)]
    pub sipi_vector: u8,
}

fn default_run_state() -> u32 {
    bhyve_api::VRS_RUN
}

/// A named system device state blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePayload {
    /// Device identifier (e.g., "ioapic", "atpit").
    pub name: String,
    /// VDC class constant.
    pub class: u16,
    /// VDC version used for the data.
    pub version: u16,
    /// Raw device state bytes.
    pub data: Vec<u8>,
}

/// High-level migration protocol message.
///
/// Each variant corresponds to a [`MessageType`] and carries the
/// associated payload. Use [`Message::encode`] and [`Message::decode`]
/// to convert to/from WebSocket binary frames.
#[derive(Debug, Clone)]
pub enum Message {
    /// Acknowledgement / success (no payload).
    Okay,
    /// Error with a RON-serialized MigrateError.
    Error(MigrateError),
    /// RON-serialized structured data (protocol offers, preamble,
    /// time data, device state, etc.).
    Serialized(Vec<u8>),
    /// Raw binary blob.
    Blob(Vec<u8>),
    /// A single 4 KiB memory page.
    Page(Vec<u8>),
    /// Memory region query.
    MemQuery(Vec<MemRegionDesc>),
    /// Memory region offer with dirty bitmap.
    MemOffer {
        region: MemRegionDesc,
        bitmap: Vec<u8>,
    },
    /// Request specific pages by GPA list.
    MemFetch(Vec<u64>),
    /// Page data transfer header (GPA + count).
    MemXfer {
        gpa: u64,
        count: u32,
    },
    /// End of memory transfer phase.
    MemEnd,
    /// Memory transfer complete acknowledgement.
    MemDone,
}

impl Message {
    /// Encode this message into a WebSocket binary frame.
    ///
    /// Format: `[payload bytes...][tag byte]`
    pub fn encode(&self) -> tokio_tungstenite::tungstenite::Message {
        let (tag, payload) = match self {
            Message::Okay => (MessageType::Okay, Vec::new()),
            Message::Error(e) => {
                let data = ron::ser::to_string(e)
                    .unwrap_or_else(|_| "serialization_failed".to_string());
                (MessageType::Error, data.into_bytes())
            }
            Message::Serialized(data) => {
                (MessageType::Serialized, data.clone())
            }
            Message::Blob(data) => (MessageType::Blob, data.clone()),
            Message::Page(data) => (MessageType::Page, data.clone()),
            Message::MemQuery(regions) => {
                let data = ron::ser::to_string(regions)
                    .unwrap_or_else(|_| "[]".to_string());
                (MessageType::MemQuery, data.into_bytes())
            }
            Message::MemOffer { region, bitmap } => {
                // RON-serialize the region, then append bitmap as raw
                // bytes after a length separator.
                let region_data = ron::ser::to_string(region)
                    .unwrap_or_else(|_| "()".to_string());
                let region_bytes = region_data.as_bytes();
                let len_bytes = (region_bytes.len() as u32).to_le_bytes();
                let mut payload = Vec::with_capacity(
                    4 + region_bytes.len() + bitmap.len(),
                );
                payload.extend_from_slice(&len_bytes);
                payload.extend_from_slice(region_bytes);
                payload.extend_from_slice(bitmap);
                (MessageType::MemOffer, payload)
            }
            Message::MemFetch(gpas) => {
                let data = ron::ser::to_string(gpas)
                    .unwrap_or_else(|_| "[]".to_string());
                (MessageType::MemFetch, data.into_bytes())
            }
            Message::MemXfer { gpa, count } => {
                let mut payload = Vec::with_capacity(12);
                payload.extend_from_slice(&gpa.to_le_bytes());
                payload.extend_from_slice(&count.to_le_bytes());
                (MessageType::MemXfer, payload)
            }
            Message::MemEnd => (MessageType::MemEnd, Vec::new()),
            Message::MemDone => (MessageType::MemDone, Vec::new()),
        };

        let mut frame = payload;
        frame.push(tag as u8);
        tokio_tungstenite::tungstenite::Message::Binary(frame)
    }

    /// Decode a WebSocket binary frame into a Message.
    pub fn decode(
        ws_msg: tokio_tungstenite::tungstenite::Message,
    ) -> Result<Self, MigrateError> {
        let data = match ws_msg {
            tokio_tungstenite::tungstenite::Message::Binary(d) => d,
            tokio_tungstenite::tungstenite::Message::Close(_) => {
                return Err(MigrateError::WebSocket(
                    "connection closed".to_string(),
                ));
            }
            other => {
                return Err(MigrateError::Codec(format!(
                    "expected binary frame, got {:?}",
                    other,
                )));
            }
        };

        if data.is_empty() {
            return Err(MigrateError::Codec(
                "empty frame (no tag byte)".to_string(),
            ));
        }

        let tag_byte = data[data.len() - 1];
        let payload = &data[..data.len() - 1];

        let tag = MessageType::from_u8(tag_byte).ok_or_else(|| {
            MigrateError::Codec(format!("unknown message tag: {tag_byte}"))
        })?;

        match tag {
            MessageType::Okay => Ok(Message::Okay),
            MessageType::Error => {
                let text = std::str::from_utf8(payload).map_err(|e| {
                    MigrateError::Codec(format!("invalid UTF-8 in error: {e}"))
                })?;
                let err: MigrateError = ron::from_str(text)?;
                Ok(Message::Error(err))
            }
            MessageType::Serialized => {
                Ok(Message::Serialized(payload.to_vec()))
            }
            MessageType::Blob => Ok(Message::Blob(payload.to_vec())),
            MessageType::Page => Ok(Message::Page(payload.to_vec())),
            MessageType::MemQuery => {
                let text = std::str::from_utf8(payload).map_err(|e| {
                    MigrateError::Codec(format!("invalid UTF-8: {e}"))
                })?;
                let regions: Vec<MemRegionDesc> = ron::from_str(text)?;
                Ok(Message::MemQuery(regions))
            }
            MessageType::MemOffer => {
                if payload.len() < 4 {
                    return Err(MigrateError::Codec(
                        "MemOffer too short for length prefix".to_string(),
                    ));
                }
                let region_len = u32::from_le_bytes(
                    payload[..4].try_into().map_err(|_| {
                        MigrateError::Codec(
                            "MemOffer length prefix".to_string(),
                        )
                    })?,
                ) as usize;
                if payload.len() < 4 + region_len {
                    return Err(MigrateError::Codec(
                        "MemOffer truncated region data".to_string(),
                    ));
                }
                let region_text =
                    std::str::from_utf8(&payload[4..4 + region_len])
                        .map_err(|e| {
                            MigrateError::Codec(format!(
                                "invalid UTF-8 in MemOffer region: {e}"
                            ))
                        })?;
                let region: MemRegionDesc = ron::from_str(region_text)?;
                let bitmap = payload[4 + region_len..].to_vec();
                Ok(Message::MemOffer { region, bitmap })
            }
            MessageType::MemFetch => {
                let text = std::str::from_utf8(payload).map_err(|e| {
                    MigrateError::Codec(format!("invalid UTF-8: {e}"))
                })?;
                let gpas: Vec<u64> = ron::from_str(text)?;
                Ok(Message::MemFetch(gpas))
            }
            MessageType::MemXfer => {
                if payload.len() < 12 {
                    return Err(MigrateError::Codec(
                        "MemXfer too short".to_string(),
                    ));
                }
                let gpa = u64::from_le_bytes(
                    payload[..8].try_into().map_err(|_| {
                        MigrateError::Codec("MemXfer gpa".to_string())
                    })?,
                );
                let count = u32::from_le_bytes(
                    payload[8..12].try_into().map_err(|_| {
                        MigrateError::Codec("MemXfer count".to_string())
                    })?,
                );
                Ok(Message::MemXfer { gpa, count })
            }
            MessageType::MemEnd => Ok(Message::MemEnd),
            MessageType::MemDone => Ok(Message::MemDone),
        }
    }

    /// Encode a serializable value as a Serialized message.
    pub fn serialized<T: Serialize>(val: &T) -> Result<Self, MigrateError> {
        let data = ron::ser::to_string(val).map_err(|e| {
            MigrateError::Codec(format!("RON serialize: {e}"))
        })?;
        Ok(Message::Serialized(data.into_bytes()))
    }

    /// Decode the payload of a Serialized message.
    pub fn deserialize<T: for<'de> Deserialize<'de>>(
        data: &[u8],
    ) -> Result<T, MigrateError> {
        let text = std::str::from_utf8(data).map_err(|e| {
            MigrateError::Codec(format!("invalid UTF-8: {e}"))
        })?;
        let val: T = ron::from_str(text)?;
        Ok(val)
    }
}

/// Protocol negotiation offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolOffer {
    /// List of supported protocol strings (e.g., "vmm-migrate-ron/0").
    pub protocols: Vec<String>,
}

/// Protocol negotiation selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolSelect {
    /// Selected protocol string.
    pub protocol: String,
}

/// Memory region descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemRegionDesc {
    /// Guest physical address.
    pub gpa: u64,
    /// Length in bytes.
    pub len: u64,
}

/// Preamble exchanged during the Sync phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPreamble {
    /// VM configuration for compatibility check.
    pub num_cpus: u32,
    pub mem_size: u64,
    /// Device list for compatibility.
    pub devices: Vec<String>,
    /// CPU feature bits from the source host for compatibility check.
    /// The destination rejects migration if it lacks any of these features.
    #[serde(default)]
    pub cpu_features: Option<CpuFeatures>,
}

/// CPU feature summary for migration compatibility checking.
///
/// Contains the key CPUID leaf values that affect guest instruction
/// compatibility. The destination must support all features the source
/// exposes to the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuFeatures {
    /// CPUID leaf 1 ECX (SSE3, SSE4.1, SSE4.2, AVX, FMA, etc.)
    pub leaf1_ecx: u32,
    /// CPUID leaf 1 EDX (SSE, SSE2, etc.)
    pub leaf1_edx: u32,
    /// CPUID leaf 7 subleaf 0 EBX (AVX2, AVX-512F, etc.)
    pub leaf7_ebx: u32,
    /// CPUID leaf 7 subleaf 0 ECX (AVX-512VBMI, etc.)
    pub leaf7_ecx: u32,
    /// CPUID leaf 7 subleaf 0 EDX (AVX-512 4VNNIW, etc.)
    pub leaf7_edx: u32,
    /// XCR0 supported bits (XSAVE components)
    pub xcr0: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode, then decode, and verify the round-tripped message
    /// matches the original.
    fn roundtrip(msg: &Message) -> Message {
        let ws_msg = msg.encode();
        Message::decode(ws_msg).expect("decode should succeed")
    }

    #[test]
    fn roundtrip_okay() {
        let out = roundtrip(&Message::Okay);
        assert!(matches!(out, Message::Okay));
    }

    #[test]
    fn roundtrip_mem_end() {
        let out = roundtrip(&Message::MemEnd);
        assert!(matches!(out, Message::MemEnd));
    }

    #[test]
    fn roundtrip_mem_done() {
        let out = roundtrip(&Message::MemDone);
        assert!(matches!(out, Message::MemDone));
    }

    #[test]
    fn roundtrip_error() {
        let err = MigrateError::ProtocolMismatch("test error".to_string());
        let out = roundtrip(&Message::Error(err));
        match out {
            Message::Error(MigrateError::ProtocolMismatch(s)) => {
                assert_eq!(s, "test error");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_serialized() {
        let offer = ProtocolOffer {
            protocols: vec!["vmm-migrate-ron/0".to_string()],
        };
        let msg = Message::serialized(&offer).unwrap();
        let out = roundtrip(&msg);
        match out {
            Message::Serialized(data) => {
                let decoded: ProtocolOffer =
                    Message::deserialize(&data).unwrap();
                assert_eq!(decoded.protocols, offer.protocols);
            }
            other => panic!("expected Serialized, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_page() {
        let page = vec![0xABu8; 4096];
        let out = roundtrip(&Message::Page(page.clone()));
        match out {
            Message::Page(data) => assert_eq!(data, page),
            other => panic!("expected Page, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_mem_offer() {
        let region = MemRegionDesc { gpa: 0x1000, len: 0x8000 };
        let bitmap = vec![0xFF, 0x01];
        let msg = Message::MemOffer {
            region: region.clone(),
            bitmap: bitmap.clone(),
        };
        let out = roundtrip(&msg);
        match out {
            Message::MemOffer {
                region: r,
                bitmap: b,
            } => {
                assert_eq!(r.gpa, 0x1000);
                assert_eq!(r.len, 0x8000);
                assert_eq!(b, bitmap);
            }
            other => panic!("expected MemOffer, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_mem_fetch() {
        let gpas = vec![0x1000u64, 0x2000, 0x5000];
        let out = roundtrip(&Message::MemFetch(gpas.clone()));
        match out {
            Message::MemFetch(g) => assert_eq!(g, gpas),
            other => panic!("expected MemFetch, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_mem_xfer() {
        let msg = Message::MemXfer { gpa: 0xDEAD_0000, count: 42 };
        let out = roundtrip(&msg);
        match out {
            Message::MemXfer { gpa, count } => {
                assert_eq!(gpa, 0xDEAD_0000);
                assert_eq!(count, 42);
            }
            other => panic!("expected MemXfer, got {other:?}"),
        }
    }

    #[test]
    fn decode_empty_frame_fails() {
        let ws =
            tokio_tungstenite::tungstenite::Message::Binary(Vec::new());
        let result = Message::decode(ws);
        assert!(result.is_err());
    }

    #[test]
    fn decode_unknown_tag_fails() {
        let ws =
            tokio_tungstenite::tungstenite::Message::Binary(vec![0xFF]);
        let result = Message::decode(ws);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_preamble() {
        let preamble = MigrationPreamble {
            num_cpus: 4,
            mem_size: 1024 * 1024 * 1024,
            devices: vec!["virtio-blk".to_string()],
            cpu_features: None,
        };
        let msg = Message::serialized(&preamble).unwrap();
        let out = roundtrip(&msg);
        match out {
            Message::Serialized(data) => {
                let decoded: MigrationPreamble =
                    Message::deserialize(&data).unwrap();
                assert_eq!(decoded.num_cpus, 4);
                assert_eq!(decoded.mem_size, 1024 * 1024 * 1024);
                assert_eq!(decoded.devices, vec!["virtio-blk"]);
            }
            other => panic!("expected Serialized, got {other:?}"),
        }
    }
}
