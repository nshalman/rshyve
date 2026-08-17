// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AML (ACPI Machine Language) bytecode builder.
//!
//! Provides a structured Rust API for emitting AML bytecode without
//! requiring an external ASL compiler (iasl). Each method corresponds
//! to an AML construct (Scope, Device, Method, Name, etc.) and emits
//! the correct opcodes and package-length encoding.
//!
//! # Example
//!
//! ```ignore
//! let mut aml = Aml::new();
//! aml.scope("\\_SB", |a| {
//!     a.device("PCI0", |d| {
//!         d.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0A03")));
//!     });
//! });
//! let bytecode = aml.into_bytes();
//! ```

// ── AML opcodes ─────────────────────────────────────────────────────

const AML_SCOPE_OP: u8 = 0x10;
const AML_NAME_OP: u8 = 0x08;
const AML_METHOD_OP: u8 = 0x14;
const AML_EXT_PREFIX: u8 = 0x5B;
const AML_DEVICE_OP: u8 = 0x82; // follows EXT_PREFIX
const AML_PACKAGE_OP: u8 = 0x12;
const AML_RETURN_OP: u8 = 0xA4;
const AML_STORE_OP: u8 = 0x70;
const AML_IF_OP: u8 = 0xA0;
const AML_LEQUAL_OP: u8 = 0x93;
const AML_ARG0: u8 = 0x68;
const AML_ZERO_OP: u8 = 0x00;
const AML_ONE_OP: u8 = 0x01;
const AML_BYTE_PREFIX: u8 = 0x0A;
const AML_WORD_PREFIX: u8 = 0x0B;
const AML_DWORD_PREFIX: u8 = 0x0C;
const AML_QWORD_PREFIX: u8 = 0x0E;
const AML_STRING_PREFIX: u8 = 0x0D;
const AML_BUFFER_OP: u8 = 0x11;
const AML_ROOT_PREFIX: u8 = 0x5C;
const AML_DUAL_NAME_PREFIX: u8 = 0x2E;
const AML_MULTI_NAME_PREFIX: u8 = 0x2F;

// Resource descriptor tags
const ACPI_RES_END_TAG: u8 = 0x79;

// ── AmlValue ────────────────────────────────────────────────────────

/// A typed AML data value.
#[derive(Clone, Debug)]
pub enum AmlValue {
    /// The integer zero (single byte: 0x00).
    Zero,
    /// The integer one (single byte: 0x01).
    One,
    /// An 8-bit integer (ByteConst: 0x0A + byte).
    Byte(u8),
    /// A 16-bit integer (WordConst: 0x0B + u16le).
    Word(u16),
    /// A 32-bit integer (DWordConst: 0x0C + u32le).
    DWord(u32),
    /// A 64-bit integer (QWordConst: 0x0E + u64le).
    QWord(u64),
    /// A NUL-terminated string (StringPrefix + bytes + 0x00).
    String(&'static str),
}

// ── Aml builder ─────────────────────────────────────────────────────

/// AML bytecode buffer builder.
///
/// Accumulates raw AML bytecode. Use the builder methods to emit
/// AML constructs, then call [`into_bytes`](Self::into_bytes) to
/// extract the bytecode.
pub struct Aml {
    buf: Vec<u8>,
}

impl Default for Aml {
    fn default() -> Self {
        Self::new()
    }
}

impl Aml {
    /// Create a new empty AML builder with pre-allocated capacity.
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(512),
        }
    }

    /// Consume the builder and return the accumulated bytecode.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Return the current length of accumulated bytecode.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Return whether the builder contains no AML bytecode.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    // ── Package length encoding ─────────────────────────────────

    /// Encode an AML PkgLength value.
    ///
    /// The PkgLength encodes the total number of bytes from the start
    /// of the PkgLength field itself to the end of the enclosed data.
    /// It uses variable-length encoding:
    /// - 1 byte for lengths 0..63
    /// - 2 bytes for lengths up to 0xFFF
    /// - 3 bytes for lengths up to 0xFFFFF
    /// - 4 bytes for lengths up to 0x0FFFFFFF
    fn encode_pkg_length(length: usize) -> Vec<u8> {
        if length < 63 {
            // Single byte: bits [5:0] = length
            vec![length as u8]
        } else if length < 0xFFF {
            // Two bytes: first byte bits [3:0] = length[3:0], bits [7:6] = 01
            // second byte = length[11:4]
            let lo = (length & 0x0F) as u8 | 0x40;
            let hi = ((length >> 4) & 0xFF) as u8;
            vec![lo, hi]
        } else if length < 0xFFFFF {
            // Three bytes: first byte bits [3:0] = length[3:0], bits [7:6] = 10
            let b0 = (length & 0x0F) as u8 | 0x80;
            let b1 = ((length >> 4) & 0xFF) as u8;
            let b2 = ((length >> 12) & 0xFF) as u8;
            vec![b0, b1, b2]
        } else {
            // Four bytes: first byte bits [3:0] = length[3:0], bits [7:6] = 11
            let b0 = (length & 0x0F) as u8 | 0xC0;
            let b1 = ((length >> 4) & 0xFF) as u8;
            let b2 = ((length >> 12) & 0xFF) as u8;
            let b3 = ((length >> 20) & 0xFF) as u8;
            vec![b0, b1, b2, b3]
        }
    }

    /// How many bytes the PkgLength field itself will occupy for a
    /// given total content length (NOT including the PkgLength field).
    fn pkg_length_size(content_len: usize) -> usize {
        // We need to find the smallest encoding that can hold
        // (content_len + pkg_length_field_size).
        // Try each size:
        if content_len + 1 < 63 {
            1
        } else if content_len + 2 < 0xFFF {
            2
        } else if content_len + 3 < 0xFFFFF {
            3
        } else {
            4
        }
    }

    // ── Name string encoding ────────────────────────────────────

    /// Encode a 4-character AML name segment (padded with '_').
    fn encode_name_seg(name: &str) -> [u8; 4] {
        let bytes = name.as_bytes();
        assert!(
            !bytes.is_empty() && bytes.len() <= 4,
            "AML name segment must be 1-4 chars: {:?}",
            name,
        );
        let mut seg = [b'_'; 4];
        seg[..bytes.len()].copy_from_slice(bytes);
        seg
    }

    /// Encode an AML NameString into bytecode.
    ///
    /// Handles root-prefixed paths (`\\_SB.PCI0`), simple 4-char
    /// names (`PCI0`), and multi-segment paths.
    fn encode_name_string(name: &str) -> Vec<u8> {
        let mut result = Vec::with_capacity(12);

        let name = if let Some(stripped) = name.strip_prefix('\\') {
            result.push(AML_ROOT_PREFIX);
            stripped
        } else {
            name
        };

        // Handle parent prefix(es)
        let name = {
            let mut n = name;
            while let Some(stripped) = n.strip_prefix('^') {
                result.push(0x5E); // ParentPrefixChar
                n = stripped;
            }
            n
        };

        if name.is_empty() {
            // Root or parent path with no trailing segment - null name
            result.push(0x00);
            return result;
        }

        let segments: Vec<&str> = name.split('.').collect();
        match segments.len() {
            1 => {
                result.extend_from_slice(&Self::encode_name_seg(segments[0]));
            }
            2 => {
                result.push(AML_DUAL_NAME_PREFIX);
                result.extend_from_slice(&Self::encode_name_seg(segments[0]));
                result.extend_from_slice(&Self::encode_name_seg(segments[1]));
            }
            n => {
                result.push(AML_MULTI_NAME_PREFIX);
                result.push(n as u8);
                for seg in &segments {
                    result.extend_from_slice(&Self::encode_name_seg(seg));
                }
            }
        }

        result
    }

    // ── Value encoding ──────────────────────────────────────────

    /// Encode an `AmlValue` into AML bytecode.
    fn encode_value(val: &AmlValue) -> Vec<u8> {
        match val {
            AmlValue::Zero => vec![AML_ZERO_OP],
            AmlValue::One => vec![AML_ONE_OP],
            AmlValue::Byte(v) => vec![AML_BYTE_PREFIX, *v],
            AmlValue::Word(v) => {
                let mut r = vec![AML_WORD_PREFIX];
                r.extend_from_slice(&v.to_le_bytes());
                r
            }
            AmlValue::DWord(v) => {
                let mut r = vec![AML_DWORD_PREFIX];
                r.extend_from_slice(&v.to_le_bytes());
                r
            }
            AmlValue::QWord(v) => {
                let mut r = vec![AML_QWORD_PREFIX];
                r.extend_from_slice(&v.to_le_bytes());
                r
            }
            AmlValue::String(s) => {
                let mut r = vec![AML_STRING_PREFIX];
                r.extend_from_slice(s.as_bytes());
                r.push(0x00); // NUL terminator
                r
            }
        }
    }

    // ── Core AML constructs ─────────────────────────────────────

    /// Emit a Scope(name) { body } construct.
    pub fn scope(&mut self, name: &str, body: impl FnOnce(&mut Aml)) {
        let mut inner = Aml::new();
        body(&mut inner);
        let inner_bytes = inner.into_bytes();
        let name_bytes = Self::encode_name_string(name);

        // Content = name + inner
        let content_len = name_bytes.len() + inner_bytes.len();
        let pkg_len_size = Self::pkg_length_size(content_len);
        let total_pkg_len = pkg_len_size + content_len;

        self.buf.push(AML_SCOPE_OP);
        self.buf
            .extend_from_slice(&Self::encode_pkg_length(total_pkg_len));
        self.buf.extend_from_slice(&name_bytes);
        self.buf.extend_from_slice(&inner_bytes);
    }

    /// Emit a Device(name) { body } construct.
    pub fn device(&mut self, name: &str, body: impl FnOnce(&mut Aml)) {
        let mut inner = Aml::new();
        body(&mut inner);
        let inner_bytes = inner.into_bytes();
        let name_bytes = Self::encode_name_string(name);

        let content_len = name_bytes.len() + inner_bytes.len();
        let pkg_len_size = Self::pkg_length_size(content_len);
        let total_pkg_len = pkg_len_size + content_len;

        self.buf.push(AML_EXT_PREFIX);
        self.buf.push(AML_DEVICE_OP);
        self.buf
            .extend_from_slice(&Self::encode_pkg_length(total_pkg_len));
        self.buf.extend_from_slice(&name_bytes);
        self.buf.extend_from_slice(&inner_bytes);
    }

    /// Emit a Method(name, argcount) { body } construct.
    pub fn method(
        &mut self,
        name: &str,
        args: u8,
        serialized: bool,
        body: impl FnOnce(&mut Aml),
    ) {
        let mut inner = Aml::new();
        body(&mut inner);
        let inner_bytes = inner.into_bytes();
        let name_bytes = Self::encode_name_string(name);

        // Method flags byte: bits [2:0] = ArgCount, bit 3 = Serialized
        let flags = (args & 0x07) | if serialized { 0x08 } else { 0x00 };

        // Content = name + flags + inner
        let content_len = name_bytes.len() + 1 + inner_bytes.len();
        let pkg_len_size = Self::pkg_length_size(content_len);
        let total_pkg_len = pkg_len_size + content_len;

        self.buf.push(AML_METHOD_OP);
        self.buf
            .extend_from_slice(&Self::encode_pkg_length(total_pkg_len));
        self.buf.extend_from_slice(&name_bytes);
        self.buf.push(flags);
        self.buf.extend_from_slice(&inner_bytes);
    }

    /// Emit Name(name, value).
    pub fn name_val(&mut self, name: &str, val: AmlValue) {
        let name_bytes = Self::encode_name_string(name);
        let val_bytes = Self::encode_value(&val);

        self.buf.push(AML_NAME_OP);
        self.buf.extend_from_slice(&name_bytes);
        self.buf.extend_from_slice(&val_bytes);
    }

    /// Emit Name(name, Package(count) { elements... }).
    pub fn name_package(&mut self, name: &str, elements: &[AmlValue]) {
        let name_bytes = Self::encode_name_string(name);

        // Build the package body: count byte + encoded elements
        let mut pkg_body = Vec::new();
        pkg_body.push(elements.len() as u8); // NumElements
        for elem in elements {
            pkg_body.extend_from_slice(&Self::encode_value(elem));
        }

        // Package = PackageOp + PkgLength + body
        let pkg_content_len = pkg_body.len();
        let pkg_len_size = Self::pkg_length_size(pkg_content_len);
        let pkg_total = pkg_len_size + pkg_content_len;

        self.buf.push(AML_NAME_OP);
        self.buf.extend_from_slice(&name_bytes);
        self.buf.push(AML_PACKAGE_OP);
        self.buf
            .extend_from_slice(&Self::encode_pkg_length(pkg_total));
        self.buf.extend_from_slice(&pkg_body);
    }

    /// Emit Name(name, Package(count) { sub-packages... }) where
    /// each sub-package is a slice of AmlValues.
    pub fn name_nested_packages(
        &mut self,
        name: &str,
        packages: &[&[AmlValue]],
    ) {
        let name_bytes = Self::encode_name_string(name);

        // Build the outer package body
        let mut outer_body = Vec::new();
        outer_body.push(packages.len() as u8); // NumElements

        for elements in packages {
            // Build inner package
            let mut inner_body = Vec::new();
            inner_body.push(elements.len() as u8);
            for elem in *elements {
                inner_body.extend_from_slice(&Self::encode_value(elem));
            }

            let inner_len = inner_body.len();
            let inner_pkg_size = Self::pkg_length_size(inner_len);
            let inner_total = inner_pkg_size + inner_len;

            outer_body.push(AML_PACKAGE_OP);
            outer_body.extend_from_slice(&Self::encode_pkg_length(inner_total));
            outer_body.extend_from_slice(&inner_body);
        }

        // Wrap in outer Package
        let outer_len = outer_body.len();
        let outer_pkg_size = Self::pkg_length_size(outer_len);
        let outer_total = outer_pkg_size + outer_len;

        self.buf.push(AML_NAME_OP);
        self.buf.extend_from_slice(&name_bytes);
        self.buf.push(AML_PACKAGE_OP);
        self.buf
            .extend_from_slice(&Self::encode_pkg_length(outer_total));
        self.buf.extend_from_slice(&outer_body);
    }

    /// Emit Name(name, ResourceTemplate() { body }).
    ///
    /// A ResourceTemplate is encoded as a Buffer containing resource
    /// descriptors terminated by an EndTag.
    pub fn name_resource_template(
        &mut self,
        name: &str,
        body: impl FnOnce(&mut Aml),
    ) {
        let mut inner = Aml::new();
        body(&mut inner);
        let mut res_bytes = inner.into_bytes();

        // Append EndTag descriptor: tag=0x79, checksum=0x00
        res_bytes.push(ACPI_RES_END_TAG);
        res_bytes.push(0x00);

        let name_bytes = Self::encode_name_string(name);

        // Buffer = BufferOp + PkgLength + BufferSize(DWord) + data
        let buffer_size = res_bytes.len() as u32;
        let size_bytes = Self::encode_value(&AmlValue::DWord(buffer_size));
        let buffer_content_len = size_bytes.len() + res_bytes.len();
        let buf_pkg_size = Self::pkg_length_size(buffer_content_len);
        let buf_total = buf_pkg_size + buffer_content_len;

        self.buf.push(AML_NAME_OP);
        self.buf.extend_from_slice(&name_bytes);
        self.buf.push(AML_BUFFER_OP);
        self.buf
            .extend_from_slice(&Self::encode_pkg_length(buf_total));
        self.buf.extend_from_slice(&size_bytes);
        self.buf.extend_from_slice(&res_bytes);
    }

    // ── Resource descriptors ────────────────────────────────────

    /// Emit an IO resource descriptor (small, tag = 0x47).
    ///
    /// IO(Decode16, min, max, align, len)
    pub fn io_resource(&mut self, min: u16, max: u16, align: u8, len: u8) {
        self.buf.push(0x47); // IO descriptor tag
        self.buf.push(0x01); // _DEC: Decode16
        self.buf.extend_from_slice(&min.to_le_bytes());
        self.buf.extend_from_slice(&max.to_le_bytes());
        self.buf.push(align);
        self.buf.push(len);
    }

    /// Emit an IRQNoFlags resource descriptor (small, tag = 0x22).
    pub fn irq_no_flags(&mut self, irq: u8) {
        assert!(irq < 16, "IRQ must be 0-15, got {}", irq);
        let mask: u16 = 1 << irq;
        self.buf.push(0x22); // IRQNoFlags tag (small, 2 bytes data)
        self.buf.extend_from_slice(&mask.to_le_bytes());
    }

    /// Emit a Memory32Fixed resource descriptor (large, tag = 0x86).
    pub fn memory32_fixed(&mut self, addr: u32, len: u32) {
        self.buf.push(0x86); // Memory32Fixed tag
        self.buf.extend_from_slice(&9u16.to_le_bytes()); // length of body
        self.buf.push(0x01); // Read/Write
        self.buf.extend_from_slice(&addr.to_le_bytes());
        self.buf.extend_from_slice(&len.to_le_bytes());
    }

    /// Emit a WordBusNumber resource descriptor (large, tag = 0x88).
    ///
    /// WordBusNumber(ResourceProducer, MinFixed, MaxFixed, PosDecode,
    ///   granularity=0, min, max, translation=0, len)
    pub fn word_bus_number(&mut self, min: u16, max: u16, len: u16) {
        self.buf.push(0x88); // Word address space descriptor
        self.buf.extend_from_slice(&13u16.to_le_bytes()); // body length
                                                          // Resource type = 2 (Bus number range)
        self.buf.push(0x02);
        // General flags: ResourceProducer=0, PosDecode=0, MinFixed=1, MaxFixed=1
        // Bits [7:4]=type-specific, [3]=MinFixed, [2]=MaxFixed, [1:0]=consumer/producer
        // MinFixed | MaxFixed = 0x0C
        self.buf.push(0x0C);
        // Type-specific flags: 0
        self.buf.push(0x00);
        // Granularity (u16)
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        // Range Minimum (u16)
        self.buf.extend_from_slice(&min.to_le_bytes());
        // Range Maximum (u16)
        self.buf.extend_from_slice(&max.to_le_bytes());
        // Translation Offset (u16)
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        // Length (u16)
        self.buf.extend_from_slice(&len.to_le_bytes());
    }

    /// Emit a WordIO resource descriptor (large, tag = 0x88).
    ///
    /// WordIO(ResourceProducer, MinFixed, MaxFixed, PosDecode,
    ///   EntireRange, granularity=0, min, max, translation=0, len)
    pub fn word_io(&mut self, min: u16, max: u16, len: u16) {
        self.buf.push(0x88); // Word address space descriptor
        self.buf.extend_from_slice(&13u16.to_le_bytes()); // body length
                                                          // Resource type = 1 (IO range)
        self.buf.push(0x01);
        // General flags: MinFixed | MaxFixed
        self.buf.push(0x0C);
        // Type-specific flags: ISA ranges only (0x03 = EntireRange, TypeStatic)
        self.buf.push(0x03);
        // Granularity
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        // Range Minimum
        self.buf.extend_from_slice(&min.to_le_bytes());
        // Range Maximum
        self.buf.extend_from_slice(&max.to_le_bytes());
        // Translation Offset
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        // Length
        self.buf.extend_from_slice(&len.to_le_bytes());
    }

    /// Emit a DWordMemory resource descriptor (large, tag = 0x87).
    ///
    /// DWordMemory(ResourceProducer, PosDecode, MinFixed, MaxFixed,
    ///   NonCacheable, ReadWrite, granularity=0, min, max,
    ///   translation=0, len)
    pub fn dword_memory(&mut self, min: u32, max: u32, len: u32) {
        self.buf.push(0x87); // DWord address space descriptor
        self.buf.extend_from_slice(&23u16.to_le_bytes()); // body length
                                                          // Resource type = 0 (Memory range)
        self.buf.push(0x00);
        // General flags: MinFixed | MaxFixed
        self.buf.push(0x0C);
        // Type-specific flags: NonCacheable=0, ReadWrite=1, TypeStatic=0,
        // AddressRangeMemory=0
        self.buf.push(0x01);
        // Granularity (u32)
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        // Range Minimum (u32)
        self.buf.extend_from_slice(&min.to_le_bytes());
        // Range Maximum (u32)
        self.buf.extend_from_slice(&max.to_le_bytes());
        // Translation Offset (u32)
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        // Length (u32)
        self.buf.extend_from_slice(&len.to_le_bytes());
    }

    // ── AML primitives ──────────────────────────────────────────

    /// Emit Return(nameref).
    pub fn return_name(&mut self, name: &str) {
        self.buf.push(AML_RETURN_OP);
        self.buf.extend_from_slice(&Self::encode_name_string(name));
    }

    /// Emit Return(value).
    pub fn return_val(&mut self, val: AmlValue) {
        self.buf.push(AML_RETURN_OP);
        self.buf.extend_from_slice(&Self::encode_value(&val));
    }

    /// Emit Store(Arg0, name).
    pub fn store_arg0(&mut self, name: &str) {
        self.buf.push(AML_STORE_OP);
        self.buf.push(AML_ARG0);
        self.buf.extend_from_slice(&Self::encode_name_string(name));
    }

    /// Emit If(LEqual(name, val)) { body }.
    pub fn if_equal(
        &mut self,
        name: &str,
        val: u64,
        body: impl FnOnce(&mut Aml),
    ) {
        let mut inner = Aml::new();
        body(&mut inner);
        let inner_bytes = inner.into_bytes();

        // Predicate: LEqual(name, val)
        let name_bytes = Self::encode_name_string(name);
        let val_aml = if val == 0 {
            AmlValue::Zero
        } else if val == 1 {
            AmlValue::One
        } else if val <= 0xFF {
            AmlValue::Byte(val as u8)
        } else if val <= 0xFFFF {
            AmlValue::Word(val as u16)
        } else if val <= 0xFFFF_FFFF {
            AmlValue::DWord(val as u32)
        } else {
            AmlValue::QWord(val)
        };
        let val_bytes = Self::encode_value(&val_aml);

        // predicate = LEqual op + name + val
        let pred_len = 1 + name_bytes.len() + val_bytes.len();
        let content_len = pred_len + inner_bytes.len();
        let pkg_len_size = Self::pkg_length_size(content_len);
        let total_pkg_len = pkg_len_size + content_len;

        self.buf.push(AML_IF_OP);
        self.buf
            .extend_from_slice(&Self::encode_pkg_length(total_pkg_len));
        self.buf.push(AML_LEQUAL_OP);
        self.buf.extend_from_slice(&name_bytes);
        self.buf.extend_from_slice(&val_bytes);
        self.buf.extend_from_slice(&inner_bytes);
    }

    // ── EISA ID helper ──────────────────────────────────────────

    /// Convert an EISA ID string (e.g., "PNP0A03") to a packed u32.
    ///
    /// EISA IDs consist of a 3-character manufacturer code (compressed
    /// 7-bit ASCII, where A=1, B=2, etc.) followed by a 4-digit hex
    /// product ID.
    ///
    /// The encoding packs 3 chars into bits [15:0] and the hex value
    /// into bits [31:16], then byte-swaps to little-endian.
    pub fn eisa_id(id: &str) -> u32 {
        let bytes = id.as_bytes();
        assert!(
            bytes.len() == 7,
            "EISA ID must be exactly 7 characters: {:?}",
            id,
        );

        // First 3 chars: compressed (A=1, B=2, ...)
        let c0 = (bytes[0] - b'@') & 0x1F;
        let c1 = (bytes[1] - b'@') & 0x1F;
        let c2 = (bytes[2] - b'@') & 0x1F;

        // Pack: bits [15:10] = c0, [9:5] = c1, [4:0] = c2
        let mfg: u16 = ((c0 as u16) << 10) | ((c1 as u16) << 5) | (c2 as u16);

        // Last 4 chars: hex product ID
        let hex_str = &id[3..7];
        let product = u16::from_str_radix(hex_str, 16).unwrap_or_else(|_| {
            panic!("Invalid hex in EISA ID: {:?}", hex_str)
        });

        // EISA encoding: swap bytes within each 16-bit half
        let b0 = (mfg >> 8) as u8;
        let b1 = (mfg & 0xFF) as u8;
        let b2 = (product >> 8) as u8;
        let b3 = (product & 0xFF) as u8;

        u32::from_le_bytes([b0, b1, b2, b3])
    }
}

#[cfg(test)]
pub(crate) fn walk_aml(buf: &[u8]) -> Result<(), String> {
    let mut walker = AmlWalker { buf, pos: 0 };
    walker.walk_terms(buf.len())?;
    if walker.pos != buf.len() {
        return Err(format!(
            "AML has {} trailing bytes at offset {}",
            buf.len() - walker.pos,
            walker.pos,
        ));
    }
    Ok(())
}

#[cfg(test)]
struct AmlWalker<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[cfg(test)]
impl AmlWalker<'_> {
    fn fail(&self, message: &str) -> String {
        format!("AML offset {}: {message}", self.pos)
    }

    fn read_byte(&mut self, end: usize) -> Result<u8, String> {
        if self.pos >= end || self.pos >= self.buf.len() {
            return Err(self.fail("unexpected end of input"));
        }
        let byte = self.buf[self.pos];
        self.pos += 1;
        Ok(byte)
    }

    fn skip(&mut self, count: usize, end: usize) -> Result<(), String> {
        let next = self
            .pos
            .checked_add(count)
            .ok_or_else(|| self.fail("offset overflow"))?;
        if next > end || next > self.buf.len() {
            return Err(self.fail("object exceeds enclosing package"));
        }
        self.pos = next;
        Ok(())
    }

    fn package_end(&mut self, enclosing_end: usize) -> Result<usize, String> {
        let start = self.pos;
        let lead = self.read_byte(enclosing_end)?;
        let follow = usize::from(lead >> 6);
        if follow > 0 && lead & 0x30 != 0 {
            return Err(self.fail("reserved PkgLength bits are set"));
        }

        let mut length = if follow == 0 {
            usize::from(lead & 0x3f)
        } else {
            usize::from(lead & 0x0f)
        };
        for index in 0..follow {
            let byte = usize::from(self.read_byte(enclosing_end)?);
            length |= byte << (4 + index * 8);
        }

        let encoded_size = follow + 1;
        if length < encoded_size {
            return Err(self.fail("PkgLength ends inside its own encoding"));
        }
        let package_end = start
            .checked_add(length)
            .ok_or_else(|| self.fail("PkgLength overflow"))?;
        if package_end > enclosing_end || package_end > self.buf.len() {
            return Err(self.fail("PkgLength exceeds enclosing package"));
        }
        Ok(package_end)
    }

    fn name_string(&mut self, end: usize) -> Result<(), String> {
        while self.pos < end && matches!(self.buf[self.pos], 0x5c | 0x5e) {
            self.pos += 1;
        }
        let lead = self.read_byte(end)?;
        match lead {
            0x00 => Ok(()),
            0x2e => self.skip(8, end),
            0x2f => {
                let segments = usize::from(self.read_byte(end)?);
                self.skip(
                    segments
                        .checked_mul(4)
                        .ok_or_else(|| self.fail("NameString overflow"))?,
                    end,
                )
            }
            _ => self.skip(3, end),
        }
    }

    fn integer(&mut self, end: usize) -> Result<u64, String> {
        let opcode = self.read_byte(end)?;
        let width = match opcode {
            AML_ZERO_OP => return Ok(0),
            AML_ONE_OP => return Ok(1),
            AML_BYTE_PREFIX => 1,
            AML_WORD_PREFIX => 2,
            AML_DWORD_PREFIX => 4,
            AML_QWORD_PREFIX => 8,
            _ => return Err(self.fail("expected integer object")),
        };
        let start = self.pos;
        self.skip(width, end)?;
        let mut value = 0u64;
        for (shift, byte) in self.buf[start..self.pos].iter().enumerate() {
            value |= u64::from(*byte) << (shift * 8);
        }
        Ok(value)
    }

    fn data_object(&mut self, end: usize) -> Result<(), String> {
        if self.pos >= end {
            return Err(self.fail("missing data object"));
        }
        match self.buf[self.pos] {
            AML_ZERO_OP | AML_ONE_OP | AML_BYTE_PREFIX | AML_WORD_PREFIX
            | AML_DWORD_PREFIX | AML_QWORD_PREFIX => {
                self.integer(end)?;
                Ok(())
            }
            AML_STRING_PREFIX => {
                self.pos += 1;
                let nul = self.buf[self.pos..end]
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(|| self.fail("unterminated string"))?;
                self.skip(nul + 1, end)
            }
            AML_PACKAGE_OP => {
                self.pos += 1;
                let package_end = self.package_end(end)?;
                let elements = usize::from(self.read_byte(package_end)?);
                for _ in 0..elements {
                    self.data_object(package_end)?;
                }
                if self.pos != package_end {
                    return Err(
                        self.fail("Package child did not end at PkgLength")
                    );
                }
                Ok(())
            }
            AML_BUFFER_OP => {
                self.pos += 1;
                let package_end = self.package_end(end)?;
                let declared_size = self.integer(package_end)?;
                let actual_size = package_end - self.pos;
                if declared_size != actual_size as u64 {
                    return Err(
                        self.fail("Buffer size does not match PkgLength")
                    );
                }
                self.pos = package_end;
                Ok(())
            }
            _ => Err(self.fail("unsupported data object")),
        }
    }

    fn term_arg(&mut self, end: usize) -> Result<(), String> {
        if self.pos >= end {
            return Err(self.fail("missing term argument"));
        }
        match self.buf[self.pos] {
            AML_ZERO_OP | AML_ONE_OP | AML_BYTE_PREFIX | AML_WORD_PREFIX
            | AML_DWORD_PREFIX | AML_QWORD_PREFIX | AML_STRING_PREFIX
            | AML_PACKAGE_OP | AML_BUFFER_OP => self.data_object(end),
            AML_LEQUAL_OP => {
                self.pos += 1;
                self.term_arg(end)?;
                self.term_arg(end)
            }
            AML_ARG0 => {
                self.pos += 1;
                Ok(())
            }
            _ => self.name_string(end),
        }
    }

    fn package_terms(
        &mut self,
        enclosing_end: usize,
        has_name: bool,
        has_flags: bool,
    ) -> Result<(), String> {
        let package_end = self.package_end(enclosing_end)?;
        if has_name {
            self.name_string(package_end)?;
        }
        if has_flags {
            self.read_byte(package_end)?;
        }
        self.walk_terms(package_end)?;
        if self.pos != package_end {
            return Err(self.fail("child did not end at PkgLength"));
        }
        Ok(())
    }

    fn walk_terms(&mut self, end: usize) -> Result<(), String> {
        while self.pos < end {
            let opcode = self.read_byte(end)?;
            match opcode {
                AML_SCOPE_OP => self.package_terms(end, true, false)?,
                AML_EXT_PREFIX => {
                    if self.read_byte(end)? != AML_DEVICE_OP {
                        return Err(self.fail("unsupported extended opcode"));
                    }
                    self.package_terms(end, true, false)?;
                }
                AML_METHOD_OP => self.package_terms(end, true, true)?,
                AML_IF_OP => {
                    let package_end = self.package_end(end)?;
                    self.term_arg(package_end)?;
                    self.walk_terms(package_end)?;
                    if self.pos != package_end {
                        return Err(
                            self.fail("If child did not end at PkgLength")
                        );
                    }
                }
                AML_NAME_OP => {
                    self.name_string(end)?;
                    self.data_object(end)?;
                }
                AML_RETURN_OP => self.term_arg(end)?,
                AML_STORE_OP => {
                    self.term_arg(end)?;
                    self.term_arg(end)?;
                }
                _ => return Err(self.fail("unsupported term opcode")),
            }
        }
        if self.pos != end {
            return Err(self.fail("term list crossed PkgLength boundary"));
        }
        Ok(())
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eisa_id_pnp0a03() {
        // PNP0A03 is the standard PCI host bridge ID.
        // P=0x10, N=0x0E, P=0x10
        // mfg = (0x10 << 10) | (0x0E << 5) | 0x10 = 0x41D0
        // product = 0x0A03
        // bytes: [0x41, 0xD0, 0x0A, 0x03]
        let id = Aml::eisa_id("PNP0A03");
        assert_eq!(id, 0x030AD041);
    }

    #[test]
    fn eisa_id_pnp0501() {
        // PNP0501 = 16550A UART
        let id = Aml::eisa_id("PNP0501");
        assert_eq!(id, 0x0105D041);
    }

    #[test]
    fn eisa_id_pnp0103() {
        // PNP0103 = HPET
        let id = Aml::eisa_id("PNP0103");
        assert_eq!(id, 0x0301D041);
    }

    #[test]
    fn eisa_id_pnp0000() {
        // PNP0000 = 8259 PIC
        let id = Aml::eisa_id("PNP0000");
        assert_eq!(id, 0x0000D041);
    }

    #[test]
    fn eisa_id_pnp0100() {
        // PNP0100 = System timer
        let id = Aml::eisa_id("PNP0100");
        assert_eq!(id, 0x0001D041);
    }

    #[test]
    fn eisa_id_pnp0b00() {
        // PNP0B00 = RTC
        let id = Aml::eisa_id("PNP0B00");
        assert_eq!(id, 0x000BD041);
    }

    #[test]
    fn name_seg_padding() {
        let seg = Aml::encode_name_seg("_SB");
        assert_eq!(seg, [b'_', b'S', b'B', b'_']);
    }

    #[test]
    fn name_seg_full() {
        let seg = Aml::encode_name_seg("PCI0");
        assert_eq!(seg, [b'P', b'C', b'I', b'0']);
    }

    #[test]
    fn name_string_root_dual() {
        let bytes = Aml::encode_name_string("\\_SB.PCI0");
        // Should be: ROOT_PREFIX + DUAL_NAME + "_SB_" + "PCI0"
        assert_eq!(bytes[0], AML_ROOT_PREFIX);
        assert_eq!(bytes[1], AML_DUAL_NAME_PREFIX);
        assert_eq!(&bytes[2..6], b"_SB_");
        assert_eq!(&bytes[6..10], b"PCI0");
    }

    #[test]
    fn name_string_simple() {
        let bytes = Aml::encode_name_string("PCI0");
        assert_eq!(bytes, b"PCI0");
    }

    #[test]
    fn pkg_length_small() {
        // Length < 63: single byte
        let enc = Aml::encode_pkg_length(10);
        assert_eq!(enc.len(), 1);
        assert_eq!(enc[0], 10);
    }

    #[test]
    fn pkg_length_medium() {
        // Length = 100 (needs 2 bytes)
        let enc = Aml::encode_pkg_length(100);
        assert_eq!(enc.len(), 2);
        // First byte: low 4 bits of 100 + 0x40
        assert_eq!(enc[0], (100 & 0x0F) as u8 | 0x40);
        // Second byte: bits 4-11
        assert_eq!(enc[1], (100 >> 4) as u8);
    }

    #[test]
    fn value_zero() {
        let enc = Aml::encode_value(&AmlValue::Zero);
        assert_eq!(enc, vec![AML_ZERO_OP]);
    }

    #[test]
    fn value_byte() {
        let enc = Aml::encode_value(&AmlValue::Byte(0x42));
        assert_eq!(enc, vec![AML_BYTE_PREFIX, 0x42]);
    }

    #[test]
    fn value_dword() {
        let enc = Aml::encode_value(&AmlValue::DWord(0x12345678));
        assert_eq!(enc, vec![AML_DWORD_PREFIX, 0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn scope_empty() {
        let mut aml = Aml::new();
        aml.scope("\\_SB", |_| {});
        let bytes = aml.into_bytes();
        // ScopeOp + PkgLen + name
        assert_eq!(bytes[0], AML_SCOPE_OP);
        // Name "\\_SB" encodes to ROOT_PREFIX + "_SB_" = 5 bytes
        // content_len = 5 (name), pkg_len_size = 1, total = 6
        assert_eq!(bytes[1], 6); // pkglen = 6
        assert_eq!(bytes[2], AML_ROOT_PREFIX);
        assert_eq!(&bytes[3..7], b"_SB_");
    }

    #[test]
    fn device_with_name() {
        let mut aml = Aml::new();
        aml.device("PCI0", |d| {
            d.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0A03")));
        });
        let bytes = aml.into_bytes();
        // Should start with EXT_PREFIX + DEVICE_OP
        assert_eq!(bytes[0], AML_EXT_PREFIX);
        assert_eq!(bytes[1], AML_DEVICE_OP);
    }

    #[test]
    fn method_with_return() {
        let mut aml = Aml::new();
        aml.method("_BBN", 0, false, |m| {
            m.return_val(AmlValue::Zero);
        });
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], AML_METHOD_OP);
    }

    #[test]
    fn io_resource_descriptor() {
        let mut aml = Aml::new();
        aml.io_resource(0x3F8, 0x3F8, 1, 8);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], 0x47); // IO descriptor tag
        assert_eq!(bytes[1], 0x01); // Decode16
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 0x3F8);
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 0x3F8);
        assert_eq!(bytes[6], 1); // alignment
        assert_eq!(bytes[7], 8); // length
    }

    #[test]
    fn irq_no_flags_descriptor() {
        let mut aml = Aml::new();
        aml.irq_no_flags(4);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], 0x22); // IRQNoFlags tag
        let mask = u16::from_le_bytes([bytes[1], bytes[2]]);
        assert_eq!(mask, 1 << 4);
    }

    #[test]
    fn memory32_fixed_descriptor() {
        let mut aml = Aml::new();
        aml.memory32_fixed(0xFED0_0000, 0x400);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], 0x86); // Memory32Fixed tag
        let body_len = u16::from_le_bytes([bytes[1], bytes[2]]);
        assert_eq!(body_len, 9);
        assert_eq!(bytes[3], 0x01); // ReadWrite
        let addr = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(addr, 0xFED0_0000);
    }

    #[test]
    fn resource_template_with_end_tag() {
        let mut aml = Aml::new();
        aml.name_resource_template("_CRS", |r| {
            r.io_resource(0x3F8, 0x3F8, 1, 8);
        });
        let bytes = aml.into_bytes();
        // Should contain NameOp at start
        assert_eq!(bytes[0], AML_NAME_OP);
        // Should end with EndTag (0x79, 0x00) inside the buffer
        let end_pos = bytes.len() - 2;
        // Find the EndTag inside
        let has_end_tag = bytes.windows(2).any(|w| w == [0x79, 0x00]);
        assert!(has_end_tag, "ResourceTemplate must end with EndTag");
        // The last two content bytes before any padding should be 0x79, 0x00
        assert_eq!(bytes[end_pos], 0x79);
        assert_eq!(bytes[end_pos + 1], 0x00);
    }

    #[test]
    fn store_arg0_to_name() {
        let mut aml = Aml::new();
        aml.store_arg0("PICM");
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], AML_STORE_OP);
        assert_eq!(bytes[1], AML_ARG0);
        assert_eq!(&bytes[2..6], b"PICM");
    }

    #[test]
    fn if_equal_construct() {
        let mut aml = Aml::new();
        aml.if_equal("PICM", 0, |body| {
            body.return_name("APRT");
        });
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], AML_IF_OP);
        // Should contain LEqual op somewhere inside
        assert!(bytes.contains(&AML_LEQUAL_OP));
    }

    #[test]
    fn nested_packages() {
        let mut aml = Aml::new();
        let pkg1: &[AmlValue] = &[
            AmlValue::DWord(0x0004FFFF),
            AmlValue::Zero,
            AmlValue::Zero,
            AmlValue::Byte(0x10),
        ];
        aml.name_nested_packages("APRT", &[pkg1]);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], AML_NAME_OP);
        // Should contain PackageOp
        assert!(bytes.contains(&AML_PACKAGE_OP));
    }
}
