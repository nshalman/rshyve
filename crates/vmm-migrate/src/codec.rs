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

use crate::limits;

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
    /// Batch of memory pages (optionally zstd-compressed).
    ///
    /// Wire format: `[base_gpa: u64][page_count: u32][flags: u32][data...]`
    /// flags bit 0: ZSTD_COMPRESSED (data is zstd-compressed)
    /// data: page_count * 4096 bytes (uncompressed) or zstd stream
    PageBatch = 11,
    /// Source has paused; next RAM pass is the final one.
    PauseSignal = 12,
}

/// PageBatch flag: data payload is zstd-compressed.
pub const PAGE_BATCH_FLAG_ZSTD: u32 = 1 << 0;

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
            11 => Some(Self::PageBatch),
            12 => Some(Self::PauseSignal),
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

impl From<tungstenite::Error> for MigrateError {
    fn from(e: tungstenite::Error) -> Self {
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
    /// Virtio queue state for viona ring reprogramming on destination.
    #[serde(default)]
    pub virtio_queues: Vec<VirtioQueueState>,
    /// NVMe controller state for disk device restoration on destination.
    #[serde(default)]
    pub nvme_controllers: Vec<NvmeControllerState>,
}

/// Virtio PCI transport state needed to restore BAR decoding after migration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VirtioMigratePciState {
    /// PCI command register bits that gate BAR decoding and bus mastering.
    pub command: u16,
    /// BAR base addresses indexed by BAR number (BAR0..BAR5).
    pub bar_addrs: [u64; 6],
}

/// Virtio queue ring addresses for post-migration restoration.
///
/// After migration, the destination's viona kernel driver needs
/// the ring GPAs to be reprogrammed. These addresses were configured
/// by the guest on the source and must be transferred explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtioQueueState {
    /// PCI BDF slot of the virtio device (to match on destination).
    pub pci_slot: u8,
    /// Queue index (0=RX, 1=TX for viona).
    pub queue_idx: u16,
    /// Queue size (number of descriptors).
    pub queue_size: u16,
    /// Guest physical address of descriptor table.
    pub desc_addr: u64,
    /// Guest physical address of available ring.
    pub avail_addr: u64,
    /// Guest physical address of used ring.
    pub used_addr: u64,
    /// Whether this queue is enabled.
    pub enabled: bool,
    /// Negotiated features for the device.
    pub features: u64,
    /// Current available ring index (device-side).
    #[serde(default)]
    pub avail_idx: u16,
    /// Current used ring index (device-side shadow).
    #[serde(default)]
    pub used_idx: u16,
    /// MSI-X table entry address for this queue's interrupt vector.
    #[serde(default)]
    pub msix_addr: u64,
    /// MSI-X table entry data for this queue's interrupt vector.
    #[serde(default)]
    pub msix_data: u32,
    /// MSI-X vector number for this queue (0xFFFF = no vector).
    #[serde(default = "default_msix_no_vector")]
    pub msix_vector: u16,
    /// PCI BAR addresses and command register bits for the device.
    #[serde(default)]
    pub pci_state: Option<VirtioMigratePciState>,
}

fn default_msix_no_vector() -> u16 {
    0xFFFF
}

/// NVMe controller state for migration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NvmeControllerState {
    /// Controller Configuration register.
    pub cc: u32,
    /// Controller Status register.
    pub csts: u32,
    /// Admin Queue Attributes register.
    pub aqa: u32,
    /// Admin Submission Queue base address.
    pub asq_base: u64,
    /// Admin Completion Queue base address.
    pub acq_base: u64,
    /// Admin submission queue state.
    pub admin_sq: Option<NvmeSqState>,
    /// Admin completion queue state.
    pub admin_cq: Option<NvmeCqState>,
    /// I/O submission queue states (index 0 = qid 1).
    pub io_sqs: Vec<Option<NvmeSqState>>,
    /// I/O completion queue states (index 0 = qid 1).
    pub io_cqs: Vec<Option<NvmeCqState>>,
    /// Number of I/O queue pairs negotiated with guest.
    pub num_io_queues: u16,
    /// PCI command register.
    pub pci_command: u16,
    /// PCI BAR base addresses (BAR0..BAR5).
    pub pci_bar_addrs: [u64; 6],
    /// MSI-X table entries: (addr, data) per vector.
    pub msix_entries: Vec<(u64, u64)>,
    /// Whether MSI-X is globally enabled.
    pub msix_enabled: bool,
}

/// NVMe submission queue state for migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeSqState {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub cq_id: u16,
}

/// NVMe completion queue state for migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeCqState {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub phase: bool,
    pub iv: u16,
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
    MemXfer { gpa: u64, count: u32 },
    /// End of memory transfer phase.
    MemEnd,
    /// Memory transfer complete acknowledgement.
    MemDone,
    /// Batch of memory pages, optionally zstd-compressed.
    PageBatch {
        /// GPA of the first page in the batch.
        base_gpa: u64,
        /// Number of pages (each 4 KiB).
        page_count: u32,
        /// Flags (bit 0 = ZSTD_COMPRESSED).
        flags: u32,
        /// Page data (compressed or raw).
        data: Vec<u8>,
    },
    /// Source has paused the VM; next RAM pass is final.
    PauseSignal,
}

impl Message {
    /// Encode this message into a WebSocket binary frame.
    ///
    /// Format: `[payload bytes...][tag byte]`
    pub fn encode(&self) -> tungstenite::Message {
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
                let mut payload =
                    Vec::with_capacity(4 + region_bytes.len() + bitmap.len());
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
            Message::PageBatch {
                base_gpa,
                page_count,
                flags,
                ref data,
            } => {
                let mut payload = Vec::with_capacity(16 + data.len());
                payload.extend_from_slice(&base_gpa.to_le_bytes());
                payload.extend_from_slice(&page_count.to_le_bytes());
                payload.extend_from_slice(&flags.to_le_bytes());
                payload.extend_from_slice(data);
                (MessageType::PageBatch, payload)
            }
            Message::PauseSignal => (MessageType::PauseSignal, Vec::new()),
        };

        let mut frame = payload;
        frame.push(tag as u8);
        tungstenite::Message::Binary(frame)
    }

    /// Decode a WebSocket binary frame into a Message.
    pub fn decode(ws_msg: tungstenite::Message) -> Result<Self, MigrateError> {
        let data = match ws_msg {
            tungstenite::Message::Binary(d) => d,
            tungstenite::Message::Close(_) => {
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
                let region_text = std::str::from_utf8(
                    &payload[4..4 + region_len],
                )
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
                if gpas.len() > limits::MAX_SPARSE_GPAS {
                    return Err(MigrateError::Codec(format!(
                        "MemFetch has too many GPAs: {}",
                        gpas.len(),
                    )));
                }
                Ok(Message::MemFetch(gpas))
            }
            MessageType::MemXfer => {
                if payload.len() < 12 {
                    return Err(MigrateError::Codec(
                        "MemXfer too short".to_string(),
                    ));
                }
                let gpa = u64::from_le_bytes(payload[..8].try_into().map_err(
                    |_| MigrateError::Codec("MemXfer gpa".to_string()),
                )?);
                let count =
                    u32::from_le_bytes(payload[8..12].try_into().map_err(
                        |_| MigrateError::Codec("MemXfer count".to_string()),
                    )?);
                Ok(Message::MemXfer { gpa, count })
            }
            MessageType::MemEnd => Ok(Message::MemEnd),
            MessageType::MemDone => Ok(Message::MemDone),
            MessageType::PageBatch => {
                if payload.len() < 16 {
                    return Err(MigrateError::Codec(
                        "PageBatch too short for header".to_string(),
                    ));
                }
                let base_gpa =
                    u64::from_le_bytes(payload[..8].try_into().unwrap());
                let page_count =
                    u32::from_le_bytes(payload[8..12].try_into().unwrap());
                let flags =
                    u32::from_le_bytes(payload[12..16].try_into().unwrap());
                if page_count == 0 || page_count > limits::MAX_PAGES_PER_BATCH {
                    return Err(MigrateError::Codec(format!(
                        "invalid PageBatch page count: {page_count}",
                    )));
                }
                if flags & !PAGE_BATCH_FLAG_ZSTD != 0 {
                    return Err(MigrateError::Codec(format!(
                        "unknown PageBatch flags: {flags:#x}",
                    )));
                }
                let data = payload[16..].to_vec();
                Ok(Message::PageBatch {
                    base_gpa,
                    page_count,
                    flags,
                    data,
                })
            }
            MessageType::PauseSignal => Ok(Message::PauseSignal),
        }
    }

    /// Encode a serializable value as a Serialized message.
    pub fn serialized<T: Serialize>(val: &T) -> Result<Self, MigrateError> {
        let data = ron::ser::to_string(val)
            .map_err(|e| MigrateError::Codec(format!("RON serialize: {e}")))?;
        Ok(Message::Serialized(data.into_bytes()))
    }

    /// Decode the payload of a Serialized message.
    pub fn deserialize<T: for<'de> Deserialize<'de>>(
        data: &[u8],
    ) -> Result<T, MigrateError> {
        let text = std::str::from_utf8(data)
            .map_err(|e| MigrateError::Codec(format!("invalid UTF-8: {e}")))?;
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
        let region = MemRegionDesc {
            gpa: 0x1000,
            len: 0x8000,
        };
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
    fn decode_mem_fetch_rejects_too_many_gpas() {
        let msg = Message::MemFetch(vec![0; limits::MAX_SPARSE_GPAS + 1]);
        assert!(Message::decode(msg.encode()).is_err());
    }

    #[test]
    fn roundtrip_mem_xfer() {
        let msg = Message::MemXfer {
            gpa: 0xDEAD_0000,
            count: 42,
        };
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
        let ws = tungstenite::Message::Binary(Vec::new());
        let result = Message::decode(ws);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_page_batch() {
        let data = vec![0xABu8; 4096 * 4]; // 4 pages
        let msg = Message::PageBatch {
            base_gpa: 0x10_0000,
            page_count: 4,
            flags: PAGE_BATCH_FLAG_ZSTD,
            data: data.clone(),
        };
        let out = roundtrip(&msg);
        match out {
            Message::PageBatch {
                base_gpa,
                page_count,
                flags,
                data: d,
            } => {
                assert_eq!(base_gpa, 0x10_0000);
                assert_eq!(page_count, 4);
                assert_eq!(flags, PAGE_BATCH_FLAG_ZSTD);
                assert_eq!(d, data);
            }
            other => panic!("expected PageBatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_page_batch_rejects_out_of_range_page_count() {
        let msg = Message::PageBatch {
            base_gpa: 0,
            page_count: limits::MAX_PAGES_PER_BATCH + 1,
            flags: 0,
            data: Vec::new(),
        };
        assert!(Message::decode(msg.encode()).is_err());
    }

    #[test]
    fn decode_page_batch_rejects_unknown_flags() {
        let msg = Message::PageBatch {
            base_gpa: 0,
            page_count: 1,
            flags: 1 << 1,
            data: vec![0; 4096],
        };
        assert!(Message::decode(msg.encode()).is_err());
    }

    #[test]
    fn roundtrip_pause_signal() {
        let out = roundtrip(&Message::PauseSignal);
        assert!(matches!(out, Message::PauseSignal));
    }

    #[test]
    fn decode_unknown_tag_fails() {
        let ws = tungstenite::Message::Binary(vec![0xFF]);
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

    #[test]
    fn virtio_queue_state_defaults_missing_pci_state() {
        let encoded = r#"(
            pci_slot: 0,
            queue_idx: 0,
            queue_size: 256,
            desc_addr: 4096,
            avail_addr: 8192,
            used_addr: 12288,
            enabled: true,
            features: 4294967296,
            avail_idx: 7,
            used_idx: 5,
            msix_addr: 0,
            msix_data: 0,
        )"#;

        let decoded: VirtioQueueState = ron::from_str(encoded).unwrap();
        assert!(decoded.pci_state.is_none());
    }
}
