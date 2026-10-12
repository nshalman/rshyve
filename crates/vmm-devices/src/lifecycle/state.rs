// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The state a migration carries for one device.
//!
//! These types are the wire format. A device exports one of them, the
//! migration layer ships it under the device's PCI address, or under
//! its registry id for a device with none, and the destination hands it
//! to the device with that same name. A field added here is a
//! wire-format change and needs `PROTOCOL_RON` in `vmm-migrate` bumped
//! with it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::pci::msix::MsixMigrateState;
use crate::pci::Bdf;

mod platform;
mod xhci;

pub use platform::*;
pub use xhci::*;

/// A device's PCI address on the migration wire.
///
/// The destination matches a payload to a device by this alone. Two
/// devices of one kind and one queue count are otherwise identical, so
/// anything weaker restores one device's rings onto another.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
)]
pub struct WireBdf {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl From<Bdf> for WireBdf {
    fn from(bdf: Bdf) -> Self {
        Self {
            bus: bdf.bus(),
            dev: bdf.dev(),
            func: bdf.func(),
        }
    }
}

impl fmt::Display for WireBdf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.bus, self.dev, self.func)
    }
}

/// Why a device could not export or restore its migration state.
///
/// Every variant fails the migration. The source keeps running on an
/// export failure; the destination is discarded on a restore failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceStateError {
    #[error("export failed: {0}")]
    Export(String),
    #[error("payload is {got} state, this device is {want}")]
    WrongKind {
        want: &'static str,
        got: &'static str,
    },
    #[error("invalid state: {0}")]
    Invalid(String),
}

impl DeviceStateError {
    pub fn invalid(what: impl fmt::Display) -> Self {
        Self::Invalid(what.to_string())
    }
}

/// The state a migration carries for one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceMigrateState {
    Virtio(VirtioMigrateState),
    Nvme(NvmeMigrateState),
    Fbuf(FbufMigrateState),
    Xhci(XhciMigrateState),
    Uart(UartMigrateState),
    Ps2(Ps2MigrateState),
    AcpiPm(AcpiPmMigrateState),
    AcpiGpe(AcpiGpeMigrateState),
    Chipset(ChipsetMigrateState),
    FwCfg(FwCfgMigrateState),
    CpuHotplug(CpuHotplugMigrateState),
    MemHotplug(MemHotplugMigrateState),
    TestFault(TestFaultMigrateState),
}

impl DeviceMigrateState {
    /// The kind name the topology check and the error messages use.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Virtio(_) => "virtio",
            Self::Nvme(_) => "nvme",
            Self::Fbuf(_) => "fbuf",
            Self::Xhci(_) => "xhci",
            Self::Uart(_) => "uart",
            Self::Ps2(_) => "ps2",
            Self::AcpiPm(_) => "acpi-pm",
            Self::AcpiGpe(_) => "acpi-gpe",
            Self::Chipset(_) => "chipset",
            Self::FwCfg(_) => "fwcfg",
            Self::CpuHotplug(_) => "cpu-hotplug",
            Self::MemHotplug(_) => "mem-hotplug",
            Self::TestFault(_) => crate::testdev::KIND,
        }
    }
}

/// One device's state under the address it had on the source.
///
/// Every device with an address is on the wire, `None` state included,
/// so the destination can tell "the source had nothing to carry" from
/// "the source has a device this VM does not".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatePayload {
    pub bdf: WireBdf,
    pub state: Option<DeviceMigrateState>,
}

/// The Hyper-V enlightenment's guest-visible state.
///
/// Not a PCI device: the enlightenment belongs to the VM, so it travels
/// beside the per-address payloads rather than under one of them.
///
/// The two overlay pages are not carried. They are derived from the
/// MSR values, so replaying the MSR writes on the destination
/// reinstalls them, which is what Propolis does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HypervMigrateState {
    pub guest_os_id: u64,
    pub hypercall: u64,
    pub reference_tsc: u64,
    pub crash_p: [u64; 5],
    /// `HV_X64_MSR_TIME_REF_COUNT` at export, in 100 ns units.
    ///
    /// The destination continues the counter from here. A fresh
    /// process-local clock makes a counter the guest is told is
    /// monotonic run backwards.
    pub time_ref_count: u64,
}

/// PCI header state a transport replays before its queues.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigratePciState {
    /// PCI command register bits that gate BAR decoding and bus mastering.
    pub command: u16,
    /// BAR base addresses indexed by BAR number (BAR0..BAR5).
    pub bar_addrs: [u64; 6],
    /// The Interrupt Line the firmware wrote. `None` from a v8 or older
    /// source.
    #[serde(default)]
    pub intr_line: Option<u8>,
}

/// The framebuffer: its header, the mode the guest set, and its VRAM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FbufMigrateState {
    pub pci: MigratePciState,
    pub width: u16,
    pub height: u16,
    /// `None` when the source chose not to carry it. The destination
    /// then shows black until the guest draws again.
    pub vram: Option<FbufMigrateVram>,
}

/// The start of the VRAM, LZ4-compressed (block format). VRAM past
/// `len` is zero.
///
/// Builds before v12 sent v8 to v10 with only `zstd` (a zstd frame of
/// the same bytes) or only `lz4`, under the same version string. A
/// source sends both to a peer before v12, so either build reads it,
/// and a destination takes either. `vmm-migrate` does the zstd side, so
/// the device crates stay free of a C codec.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FbufMigrateVram {
    pub len: u32,
    // RON writes a plain Vec<u8> as a list of decimal numbers, about
    // four times the bytes. As bytes it is base64.
    #[serde(default, with = "wire_bytes")]
    pub lz4: Vec<u8>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "wire_bytes"
    )]
    pub zstd: Vec<u8>,
}

impl FbufMigrateVram {
    /// The size of the VRAM segment, which bounds `len`.
    pub const MAX_LEN: u32 = 16 * 1024 * 1024;

    /// The largest compressed VRAM the device state carries, per field.
    ///
    /// The whole device state is one message of at most 8 MiB, and RON
    /// writes these bytes as base64, so two fields at this cap still
    /// fit.
    pub const WIRE_CAP: usize = 2 * 1024 * 1024;
}

impl fmt::Debug for FbufMigrateVram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FbufMigrateVram")
            .field("len", &self.len)
            .field("lz4_len", &self.lz4.len())
            .field("zstd_len", &self.zstd.len())
            .finish()
    }
}

pub(crate) mod wire_bytes {
    use serde::de::{Deserializer, Error, SeqAccess, Visitor};
    use serde::Serializer;
    use std::fmt;

    pub fn serialize<S: Serializer>(
        bytes: &[u8],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        deserializer.deserialize_byte_buf(BytesVisitor)
    }

    struct BytesVisitor;

    impl<'de> Visitor<'de> for BytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bytes")
        }

        fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }

        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::new();
            while let Some(b) = seq.next_element()? {
                out.push(b);
            }
            Ok(out)
        }
    }
}

/// One virtio PCI transport: the register file the driver programmed
/// and every queue it configured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMigrateState {
    /// The device status byte as the driver left it. Restored as it
    /// was: a destination that synthesises DRIVER_OK tells the guest
    /// driver a device is ready that the driver never finished.
    pub status: u8,
    /// Features the driver accepted.
    pub features: u64,
    /// MSI-X vector for configuration changes, `0xFFFF` for none.
    pub config_msix_vector: u16,
    pub pci: MigratePciState,
    /// The MSI-X table, when the transport has one. `None` keeps the
    /// destination on INTx, which is where an INTx guest has to stay.
    pub msix: Option<MsixMigrateState>,
    /// Every configured queue, in queue order.
    pub queues: Vec<VirtioMigrateQueue>,
    /// The state of the device behind the transport, for a device that
    /// keeps any beyond its rings. Left off the wire when there is none,
    /// so the payload is a v12 peer's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<VirtioDeviceMigrateState>,
}

/// The state a virtio device keeps behind its transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VirtioDeviceMigrateState {
    Sound(SoundMigrateState),
    Gpu(GpuMigrateState),
}

impl VirtioDeviceMigrateState {
    /// The kind name the error messages use.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Sound(_) => "virtio-snd",
            Self::Gpu(_) => "virtio-gpu",
        }
    }
}

/// A virtio-snd device's one output stream, as the driver set it up.
///
/// No message is in flight: a paused device has handed back every one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoundMigrateState {
    /// The hardware buffer size SET_PARAMS accepted, if it has run.
    pub buffer_bytes: Option<u32>,
    /// Between PREPARE and RELEASE.
    pub prepared: bool,
    /// Between START and STOP.
    pub running: bool,
}

/// A virtio-gpu device's resources, scanout and cursor.
///
/// The pixels are not carried: every resource the guest draws into is
/// backed by guest pages, which the migration carries, and the
/// destination reads each resource back from them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMigrateState {
    pub resources: Vec<GpuMigrateResource>,
    pub scanout: Option<GpuMigrateScanout>,
    pub cursor: Option<GpuMigrateCursor>,
    /// The mode the device offers the guest for scanout 0.
    pub preferred: (u32, u32),
    /// Config `events_read`: events the driver has not cleared.
    pub events: u32,
    /// Blobs of guest memory; none when absent.
    #[serde(default)]
    pub blobs: Vec<GpuMigrateBlob>,
}

/// One blob of guest memory. Like a 2D resource's pixels, its contents
/// are the guest pages the migration carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMigrateBlob {
    pub id: u32,
    pub size: u64,
    /// The guest pages it is, in order, as `(address, length)`.
    pub backing: Vec<(u64, u32)>,
}

/// How a blob holds the image the scanout shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMigrateBlobImage {
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub stride: u32,
    /// Where the first row starts in the blob.
    pub offset: u32,
}

/// One 2D resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMigrateResource {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// The guest pages behind it, in order, as `(address, length)`.
    pub backing: Option<Vec<(u64, u32)>>,
}

/// Which rectangle of which resource scanout 0 shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMigrateScanout {
    pub resource_id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The image, when the resource is a blob; none when absent.
    #[serde(default)]
    pub blob: Option<GpuMigrateBlobImage>,
}

/// The cursor image and where it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMigrateCursor {
    pub x: u32,
    pub y: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// 64x64 pixels, B8G8R8A8.
    pub pixels: Vec<u8>,
}

/// One configured virtqueue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMigrateQueue {
    pub queue_idx: u16,
    pub queue_size: u16,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    /// The queue is live: enabled through the modern register, or
    /// programmed through the legacy PFN register, which has no
    /// enable bit.
    pub live: bool,
    /// The device's consumption cursor into the available ring.
    pub avail_idx: u16,
    /// The device's completion cursor into the used ring.
    pub used_idx: u16,
    /// MSI-X vector for this queue, `0xFFFF` for none.
    pub msix_vector: u16,
}

/// One NVMe controller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateState {
    pub cc: u32,
    pub csts: u32,
    pub aqa: u32,
    pub asq_base: u64,
    pub acq_base: u64,
    pub admin_sq: Option<NvmeMigrateSq>,
    pub admin_cq: Option<NvmeMigrateCq>,
    pub io_sqs: Vec<Option<NvmeMigrateSq>>,
    pub io_cqs: Vec<Option<NvmeMigrateCq>>,
    /// The larger of the two granted counts. A peer that keeps one
    /// count for both reads only this, and accepts every live queue.
    pub num_io_queues: u16,
    /// `None` from a source that kept one count for SQs and CQs.
    #[serde(default)]
    pub io_queue_counts: Option<NvmeMigrateQueueCounts>,
    pub pci: MigratePciState,
    pub msix: MsixMigrateState,
    /// `None` from a v2 source, whose only shape was 512-byte blocks and
    /// a volatile write cache.
    #[serde(default)]
    pub namespace: Option<NvmeMigrateNamespace>,
    /// `None` from a v3 or older source, which had only host files.
    #[serde(default)]
    pub backend: Option<NvmeMigrateBackend>,
    /// Asynchronous Event Requests held without a completion. The guest
    /// counts them against AERL, so a destination that forgot them
    /// would take one more than the guest believes it may send.
    pub aers_held: u8,
}

/// The I/O SQs and CQs Set Features granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateQueueCounts {
    pub sqs: u16,
    pub cqs: u16,
}

/// What holds a namespace's blocks. Two volumes of one shape pass every
/// namespace check, so the destination also needs the same store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NvmeMigrateBackend {
    /// A host file or zvol. Paths differ between hosts, so none is kept.
    File,
    /// A MantaBlock volume. A UUID, so a payload naming anything else
    /// does not decode and the name reaches no log unescaped.
    MantaBlock { volume: uuid::Uuid },
}

impl fmt::Display for NvmeMigrateBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File => f.write_str("a host file"),
            Self::MantaBlock { volume } => {
                write!(f, "MantaBlock volume {volume}")
            }
        }
    }
}

/// A destination must show the same namespace: the guest's partition
/// tables, flush habits and mounts depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateNamespace {
    pub block_size: u32,
    pub volatile_write_cache: bool,
    /// NSZE, in blocks.
    pub total_blocks: u64,
    pub read_only: bool,
}

/// NVMe submission queue state for migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateSq {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub cq_id: u16,
}

/// NVMe completion queue state for migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateCq {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub phase: bool,
    pub iv: u16,
    /// Whether the driver asked for an interrupt on this queue. A Linux
    /// poll queue has none, and restoring one as interrupting sends the
    /// destination guest a message it never armed a handler for.
    pub ien: bool,
    /// Completions waiting for a free guest CQ slot, oldest first. Empty
    /// from a v2 source, which dropped them.
    #[serde(default)]
    pub held: Vec<NvmeMigrateCqe>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateCqe {
    pub sqid: u16,
    pub cid: u16,
    /// The status field without its phase bit.
    pub status: u16,
    pub cdw0: u32,
}
