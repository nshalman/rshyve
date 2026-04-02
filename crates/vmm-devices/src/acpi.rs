// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI table generation for bhyve-based VMs.
//!
//! Generates the minimum set of ACPI tables required to boot a UEFI
//! guest: RSDP, RSDT, XSDT, FADT, FACS, DSDT, MADT, and HPET.
//!
//! Tables are laid out contiguously in guest physical memory starting
//! at [`BHYVE_ACPI_BASE`] (0xF2400). The UEFI firmware discovers them
//! via the RSDP, or via fw_cfg file entries.
//!
//! # E820 memory map
//!
//! The [`build_e820`] function generates an E820 table suitable for
//! inclusion in the fw_cfg device as "etc/e820".

/// Guest physical address where ACPI tables are placed.
pub const BHYVE_ACPI_BASE: u64 = 0xF2400;

// ── Well-known hardware addresses ─────────────────────────────────

const LAPIC_ADDR: u32 = 0xFEE0_0000;
const IOAPIC_ADDR: u32 = 0xFEC0_0000;
const HPET_ADDR: u32 = 0xFED0_0000;

// ── ACPI PM I/O ports (must match BhyvePmTimer / kernel config) ───

/// ACPI PM base port (matches PMBASE_DEFAULT in pmtimer.rs).
const PMBASE: u32 = 0x400;
/// PM1a event block port.
const PM1A_EVT_ADDR: u32 = PMBASE;
/// PM1a control block port.
const PM1A_CNT_ADDR: u32 = PMBASE + 0x04;
/// PM timer port (PMBASE + 0x08, matching VM_PMTMR_LOCATE).
const IO_PMTMR: u32 = PMBASE + 0x08;

/// SCI interrupt number.
const SCI_INT: u16 = 9;

/// Reset register port.
const RESET_REG_PORT: u64 = 0x0CF9;

// ── ACPI table header ─────────────────────────────────────────────

const ACPI_HDR_SIZE: usize = 36;

/// OEM ID used in all tables.
const OEM_ID: &[u8; 6] = b"BHYVE ";
/// OEM table ID used in all tables.
const OEM_TABLE_ID: &[u8; 8] = b"BVXSDT  ";
/// Creator ID.
const CREATOR_ID: &[u8; 4] = b"BHYV";

/// Write an ACPI SDT header into a buffer.
///
/// The checksum field is initially set to 0; call [`fix_checksum`]
/// after the full table is written.
fn write_header(
    buf: &mut Vec<u8>,
    signature: &[u8; 4],
    length: u32,
    revision: u8,
) {
    let start = buf.len();
    buf.extend_from_slice(signature);        // 0: signature
    buf.extend_from_slice(&length.to_le_bytes()); // 4: length
    buf.push(revision);                      // 8: revision
    buf.push(0);                             // 9: checksum (filled later)
    buf.extend_from_slice(OEM_ID);           // 10: oem_id
    buf.extend_from_slice(OEM_TABLE_ID);     // 16: oem_table_id
    buf.extend_from_slice(&1u32.to_le_bytes()); // 24: oem_revision
    buf.extend_from_slice(CREATOR_ID);       // 28: creator_id
    buf.extend_from_slice(&1u32.to_le_bytes()); // 32: creator_revision
    debug_assert_eq!(buf.len() - start, ACPI_HDR_SIZE);
}

/// Fix the checksum byte so that the sum of all bytes in
/// `buf[start..start+len]` is 0 mod 256.
fn fix_checksum(buf: &mut [u8], start: usize, len: usize) {
    let sum: u8 = buf[start..start + len]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    // Checksum is at offset 9 within the header (or wherever the
    // caller specifies via the start parameter for RSDP).
    buf[start + 9] = 0u8.wrapping_sub(sum);
}

/// Fix a checksum at a specific byte position.
fn fix_checksum_at(buf: &mut [u8], cksum_offset: usize, start: usize, len: usize) {
    buf[cksum_offset] = 0;
    let sum: u8 = buf[start..start + len]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    buf[cksum_offset] = 0u8.wrapping_sub(sum);
}

// ── Generic Address Structure (GAS) ──────────────────────────────

/// Write an ACPI Generic Address Structure (12 bytes).
fn write_gas(
    buf: &mut Vec<u8>,
    address_space: u8,
    bit_width: u8,
    bit_offset: u8,
    access_size: u8,
    address: u64,
) {
    buf.push(address_space);
    buf.push(bit_width);
    buf.push(bit_offset);
    buf.push(access_size);
    buf.extend_from_slice(&address.to_le_bytes());
}

/// Write a zeroed GAS (12 bytes of zeroes).
fn write_gas_zero(buf: &mut Vec<u8>) {
    buf.extend_from_slice(&[0u8; 12]);
}

// ── FACS ──────────────────────────────────────────────────────────

const FACS_SIZE: usize = 64;

fn build_facs() -> Vec<u8> {
    let mut buf = vec![0u8; FACS_SIZE];
    // Signature: "FACS" at offset 0
    buf[0..4].copy_from_slice(b"FACS");
    // Length at offset 4 (u32 LE)
    buf[4..8].copy_from_slice(&(FACS_SIZE as u32).to_le_bytes());
    // Version at offset 32 (u32 LE)
    buf[32..36].copy_from_slice(&2u32.to_le_bytes());
    buf
}

// ── DSDT ──────────────────────────────────────────────────────────

/// Build the DSDT containing AML bytecode that describes the
/// platform's device topology to the guest OS.
///
/// This matches bhyve's DSDT structure:
/// - Sleep state S5 for shutdown support
/// - PIC/APIC mode switch (PICM / _PIC method)
/// - PCI host bridge (PCI0) with bus resources and interrupt routing
/// - HPET device
/// - ISA/LPC bridge with COM1, COM2, PIC, Timer, and RTC devices
fn build_dsdt() -> Vec<u8> {
    use crate::aml::{Aml, AmlValue};

    let mut aml = Aml::new();

    // ── Sleep state S5 (soft-off) ──────────────────────────────
    // Name(_S5_, Package() { 0x05, Zero })
    aml.name_package("_S5_", &[AmlValue::Byte(0x05), AmlValue::Zero]);

    // ── PIC/APIC mode indicator ────────────────────────────────
    // Name(PICM, Zero)
    aml.name_val("PICM", AmlValue::Zero);

    // Method(_PIC, 1) { Store(Arg0, PICM) }
    aml.method("_PIC", 1, false, |m| {
        m.store_arg0("PICM");
    });

    // ── Scope(\_SB) ────────────────────────────────────────────
    aml.scope("\\_SB", |sb| {
        // ── PCI Host Bridge: Device(PCI0) ──────────────────
        sb.device("PC00", |pci| {
            // _HID = PNP0A03 (PCI host bridge)
            pci.name_val(
                "_HID",
                AmlValue::DWord(Aml::eisa_id("PNP0A03")),
            );
            // _ADR = 0
            pci.name_val("_ADR", AmlValue::Zero);

            // Method(_BBN, 0) { Return(Zero) }
            pci.method("_BBN", 0, false, |m| {
                m.return_val(AmlValue::Zero);
            });

            // ── PCI bus resources ──────────────────────────
            pci.name_resource_template("_CRS", |r| {
                // Bus numbers 0-0xFF
                r.word_bus_number(0x0000, 0x00FF, 0x0100);

                // PCI config space I/O ports (0xCF8-0xCFF)
                r.io_resource(0x0CF8, 0x0CF8, 1, 8);

                // I/O range below PCI config
                r.word_io(0x0000, 0x0CF7, 0x0CF8);

                // I/O range above PCI config
                r.word_io(0x0D00, 0xFFFF, 0xF300);

                // 32-bit MMIO window
                r.dword_memory(
                    0xC000_0000,
                    0xFEBF_FFFF,
                    0x3EC0_0000,
                );
            });

            // ── PCI interrupt routing ─────────────────────
            // Use IOAPIC pins 16+ for PCI devices, matching C bhyve's
            // ioapic_pci_alloc_irq(): ioapic_irq = 16 + (4 + slot + pin) % 8
            let mut storage: Vec<[AmlValue; 4]> = Vec::new();

            for slot in 1u32..32 {
                for pin in 0u32..4 {
                    let gsi = 16 + (4 + slot + pin) % 8;
                    storage.push([
                        AmlValue::DWord((slot << 16) | 0xFFFF),
                        AmlValue::Byte(pin as u8),
                        AmlValue::Zero,
                        AmlValue::DWord(gsi),
                    ]);
                }
            }

            // Build slice references
            let refs: Vec<&[AmlValue]> =
                storage.iter().map(|s| s.as_slice()).collect();
            pci.name_nested_packages("APRT", &refs);

            // Method(_PRT, 0) { Return(APRT) }
            pci.method("_PRT", 0, false, |m| {
                m.return_name("APRT");
            });

            // ── HPET device ───────────────────────────────
            pci.device("HPET", |hpet| {
                hpet.name_val(
                    "_HID",
                    AmlValue::DWord(Aml::eisa_id("PNP0103")),
                );
                hpet.name_val("_UID", AmlValue::Zero);
                hpet.name_resource_template("_CRS", |r| {
                    r.memory32_fixed(HPET_ADDR, 0x400);
                });
            });

            // ── ISA/LPC bridge ────────────────────────────
            // LPC bridge at slot 1, function 0
            pci.device("ISA_", |isa| {
                isa.name_val(
                    "_ADR",
                    AmlValue::DWord(0x0001_0000),
                );

                // COM1 UART (0x3F8, IRQ 4)
                isa.device("COM1", |com| {
                    com.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0501")),
                    );
                    com.name_val("_UID", AmlValue::One);
                    com.name_resource_template("_CRS", |r| {
                        r.io_resource(0x3F8, 0x3F8, 1, 8);
                        r.irq_no_flags(4);
                    });
                });

                // COM2 UART (0x2F8, IRQ 3)
                isa.device("COM2", |com| {
                    com.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0501")),
                    );
                    com.name_val("_UID", AmlValue::Byte(2));
                    com.name_resource_template("_CRS", |r| {
                        r.io_resource(0x2F8, 0x2F8, 1, 8);
                        r.irq_no_flags(3);
                    });
                });

                // 8259 PIC (0x20-0x21, 0xA0-0xA1, IRQ 2)
                isa.device("PIC_", |pic| {
                    pic.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0000")),
                    );
                    pic.name_resource_template("_CRS", |r| {
                        r.io_resource(0x20, 0x20, 1, 2);
                        r.io_resource(0xA0, 0xA0, 1, 2);
                        r.irq_no_flags(2);
                    });
                });

                // System timer (0x40-0x43, IRQ 0)
                isa.device("TIMR", |tmr| {
                    tmr.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0100")),
                    );
                    tmr.name_resource_template("_CRS", |r| {
                        r.io_resource(0x40, 0x40, 1, 4);
                        r.irq_no_flags(0);
                    });
                });

                // RTC (0x70-0x71, IRQ 8)
                isa.device("RTC0", |rtc| {
                    rtc.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0B00")),
                    );
                    rtc.name_resource_template("_CRS", |r| {
                        r.io_resource(0x70, 0x70, 1, 2);
                        r.irq_no_flags(8);
                    });
                });
            });
        });
    });

    // ── Wrap in DSDT header ────────────────────────────────────
    let aml_body = aml.into_bytes();
    let total_length = ACPI_HDR_SIZE + aml_body.len();
    let mut buf = Vec::with_capacity(total_length);
    write_header(&mut buf, b"DSDT", total_length as u32, 2);
    buf.extend_from_slice(&aml_body);
    fix_checksum(&mut buf, 0, total_length);
    buf
}

// ── FADT ──────────────────────────────────────────────────────────

/// FADT size for ACPI 6.0 (revision 6, minor 1).
const FADT_SIZE: u32 = 276;

// FADT flag bits
const FADT_WBINVD: u32 = 1 << 0;
const FADT_PROC_C1: u32 = 1 << 2;
const FADT_SLP_BUTTON: u32 = 1 << 5;
const FADT_TMR_VAL_EXT: u32 = 1 << 8;
const FADT_RESET_REG_SUP: u32 = 1 << 10;
const FADT_HEADLESS: u32 = 1 << 12;

// GAS address space IDs
const ACPI_AS_SYSTEM_IO: u8 = 1;
// Access width constants
const ACPI_GAS_BYTE: u8 = 1;
const ACPI_GAS_WORD: u8 = 2;
const ACPI_GAS_DWORD: u8 = 3;
#[allow(dead_code)]
const ACPI_GAS_UNDEF: u8 = 0;

fn build_fadt(facs_gpa: u32, dsdt_gpa: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(FADT_SIZE as usize);

    // SDT header (offsets 0..36)
    write_header(&mut buf, b"FACP", FADT_SIZE, 5);

    // offset 36: FIRMWARE_CTRL (FACS pointer, 4 bytes)
    buf.extend_from_slice(&facs_gpa.to_le_bytes());
    // offset 40: DSDT pointer (4 bytes)
    buf.extend_from_slice(&dsdt_gpa.to_le_bytes());

    // offset 44: reserved (was INT_MODEL, now must be 0)
    buf.push(0);
    // offset 45: Preferred_PM_Profile (unspecified = 0)
    buf.push(0);

    // offset 46: SCI_INT (u16 LE)
    buf.extend_from_slice(&SCI_INT.to_le_bytes());
    // offset 48: SMI_CMD (u32 LE) - port 0xB2 for ACPI enable/disable
    buf.extend_from_slice(&0xB2u32.to_le_bytes());
    // offset 52: ACPI_ENABLE - value to write to SMI_CMD
    buf.push(0xA0);
    // offset 53: ACPI_DISABLE
    buf.push(0xA1);
    // offset 54: S4BIOS_REQ
    buf.push(0);
    // offset 55: PSTATE_CNT
    buf.push(0);

    // offset 56: PM1a_EVT_BLK (u32 LE)
    buf.extend_from_slice(&PM1A_EVT_ADDR.to_le_bytes());
    // offset 60: PM1b_EVT_BLK (u32 LE) - not used
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 64: PM1a_CNT_BLK (u32 LE)
    buf.extend_from_slice(&PM1A_CNT_ADDR.to_le_bytes());
    // offset 68: PM1b_CNT_BLK (u32 LE) - not used
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 72: PM2_CNT_BLK (u32 LE) - not used
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 76: PM_TMR_BLK (u32 LE)
    buf.extend_from_slice(&IO_PMTMR.to_le_bytes());
    // offset 80: GPE0_BLK (u32 LE) - not used
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 84: GPE1_BLK (u32 LE)
    buf.extend_from_slice(&0u32.to_le_bytes());

    // offset 88: PM1_EVT_LEN
    buf.push(4);
    // offset 89: PM1_CNT_LEN
    buf.push(2);
    // offset 90: PM2_CNT_LEN
    buf.push(0);
    // offset 91: PM_TMR_LEN
    buf.push(4);
    // offset 92: GPE0_BLK_LEN
    buf.push(0);
    // offset 93: GPE1_BLK_LEN
    buf.push(0);
    // offset 94: GPE1_BASE
    buf.push(0);
    // offset 95: CST_CNT
    buf.push(0);
    // offset 96: P_LVL2_LAT (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 98: P_LVL3_LAT (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 100: FLUSH_SIZE (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 102: FLUSH_STRIDE (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 104: DUTY_OFFSET
    buf.push(0);
    // offset 105: DUTY_WIDTH
    buf.push(0);
    // offset 106: DAY_ALRM
    buf.push(0);
    // offset 107: MON_ALRM
    buf.push(0);
    // offset 108: CENTURY
    buf.push(0x32);

    // offset 109: IAPC_BOOT_ARCH (u16 LE) - NO_VGA | NO_ASPM
    let boot_flags: u16 = (1 << 2) | (1 << 4); // NO_VGA=bit2, NO_ASPM=bit4
    buf.extend_from_slice(&boot_flags.to_le_bytes());

    // offset 111: reserved
    buf.push(0);

    // offset 112: Flags (u32 LE)
    let flags = FADT_WBINVD | FADT_PROC_C1 | FADT_SLP_BUTTON
        | FADT_TMR_VAL_EXT | FADT_RESET_REG_SUP | FADT_HEADLESS;
    buf.extend_from_slice(&flags.to_le_bytes());

    // offset 116: RESET_REG (12-byte GAS)
    write_gas(&mut buf, ACPI_AS_SYSTEM_IO, 8, 0, ACPI_GAS_BYTE, RESET_REG_PORT);

    // offset 128: RESET_VALUE
    buf.push(6); // Writing 6 to 0xCF9 triggers system reset

    // offset 129: ARM_BOOT_ARCH (u16 LE) - x86, not used
    buf.extend_from_slice(&0u16.to_le_bytes());

    // offset 131: FADT Minor Version
    buf.push(1);

    // offset 132: X_FIRMWARE_CTRL (u64 LE) - extended FACS pointer
    buf.extend_from_slice(&(facs_gpa as u64).to_le_bytes());
    // offset 140: X_DSDT (u64 LE)
    buf.extend_from_slice(&(dsdt_gpa as u64).to_le_bytes());

    // offset 148: X_PM1a_EVT_BLK (GAS, 12 bytes)
    write_gas(&mut buf, ACPI_AS_SYSTEM_IO, 0x20, 0, ACPI_GAS_WORD, PM1A_EVT_ADDR as u64);
    // offset 160: X_PM1b_EVT_BLK
    write_gas_zero(&mut buf);
    // offset 172: X_PM1a_CNT_BLK
    write_gas(&mut buf, ACPI_AS_SYSTEM_IO, 0x10, 0, ACPI_GAS_WORD, PM1A_CNT_ADDR as u64);
    // offset 184: X_PM1b_CNT_BLK
    write_gas_zero(&mut buf);
    // offset 196: X_PM2_CNT_BLK
    write_gas_zero(&mut buf);
    // offset 208: X_PM_TMR_BLK
    write_gas(&mut buf, ACPI_AS_SYSTEM_IO, 0x20, 0, ACPI_GAS_DWORD, IO_PMTMR as u64);
    // offset 220: X_GPE0_BLK
    write_gas_zero(&mut buf);
    // offset 232: X_GPE1_BLK
    write_gas_zero(&mut buf);
    // offset 244: SLEEP_CONTROL_REG
    write_gas_zero(&mut buf);
    // offset 256: SLEEP_STATUS_REG
    write_gas_zero(&mut buf);
    // offset 268: Hypervisor Vendor Identity (u64 LE)
    buf.extend_from_slice(&0u64.to_le_bytes());

    assert_eq!(buf.len(), FADT_SIZE as usize);

    // Fix checksum
    fix_checksum(&mut buf, 0, FADT_SIZE as usize);

    buf
}

// ── MADT ──────────────────────────────────────────────────────────

// MADT entry types
const MADT_TYPE_LOCAL_APIC: u8 = 0;
const MADT_TYPE_IO_APIC: u8 = 1;
const MADT_TYPE_INT_OVERRIDE: u8 = 2;
const MADT_TYPE_LOCAL_APIC_NMI: u8 = 4;

// MADT flags
const MADT_PCAT_COMPAT: u32 = 1;
const MADT_LAPIC_ENABLED: u32 = 1;

// Interrupt polarity/trigger
const MADT_POLARITY_ACTIVE_HIGH: u16 = 0x01;
const MADT_POLARITY_ACTIVE_LOW: u16 = 0x03;
const MADT_TRIGGER_EDGE: u16 = 0x04;
const MADT_TRIGGER_LEVEL: u16 = 0x0C;

fn build_madt(num_cpus: u32) -> Vec<u8> {
    // Calculate total size:
    // header (36) + MADT-specific (8) +
    // local APIC entries (8 each) +
    // I/O APIC (12) + 2 interrupt overrides (10 each) +
    // local APIC NMI (6)
    let madt_specific = 8usize; // LAPIC addr (4) + flags (4)
    let lapic_entries = num_cpus as usize * 8;
    let ioapic_entry = 12usize;
    let irq_overrides = 2 * 10usize;
    let lapic_nmi = 6usize;
    let total = ACPI_HDR_SIZE + madt_specific + lapic_entries
        + ioapic_entry + irq_overrides + lapic_nmi;

    let mut buf = Vec::with_capacity(total);

    // SDT header
    write_header(&mut buf, b"APIC", total as u32, 1);

    // MADT-specific fields (after header)
    // Local APIC address (u32 LE)
    buf.extend_from_slice(&LAPIC_ADDR.to_le_bytes());
    // Flags (u32 LE) - PCAT_COMPAT
    buf.extend_from_slice(&MADT_PCAT_COMPAT.to_le_bytes());

    // Local APIC entries (one per CPU)
    for i in 0..num_cpus {
        buf.push(MADT_TYPE_LOCAL_APIC);   // type
        buf.push(8);                       // length
        buf.push(i as u8);                 // processor ID
        buf.push(i as u8);                 // APIC ID
        buf.extend_from_slice(&MADT_LAPIC_ENABLED.to_le_bytes()); // flags
    }

    // I/O APIC entry
    buf.push(MADT_TYPE_IO_APIC);  // type
    buf.push(12);                  // length
    buf.push(0);                   // I/O APIC ID
    buf.push(0);                   // reserved
    buf.extend_from_slice(&IOAPIC_ADDR.to_le_bytes()); // address
    buf.extend_from_slice(&0u32.to_le_bytes());        // GSI base

    // Interrupt override: IRQ 0 -> GSI 2 (edge, active-high)
    buf.push(MADT_TYPE_INT_OVERRIDE); // type
    buf.push(10);                      // length
    buf.push(0);                       // bus (ISA)
    buf.push(0);                       // source IRQ
    buf.extend_from_slice(&2u32.to_le_bytes()); // GSI
    let inti_flags = MADT_POLARITY_ACTIVE_HIGH | MADT_TRIGGER_EDGE;
    buf.extend_from_slice(&inti_flags.to_le_bytes());

    // Interrupt override: SCI IRQ 9 -> GSI 9 (level, active-low)
    buf.push(MADT_TYPE_INT_OVERRIDE); // type
    buf.push(10);                      // length
    buf.push(0);                       // bus (ISA)
    buf.push(SCI_INT as u8);          // source IRQ
    buf.extend_from_slice(&(SCI_INT as u32).to_le_bytes()); // GSI
    let inti_flags = MADT_POLARITY_ACTIVE_LOW | MADT_TRIGGER_LEVEL;
    buf.extend_from_slice(&inti_flags.to_le_bytes());

    // Local APIC NMI: LINT1 for all CPUs
    buf.push(MADT_TYPE_LOCAL_APIC_NMI); // type
    buf.push(6);                         // length
    buf.push(0xFF);                      // processor ID (all)
    let nmi_flags = MADT_POLARITY_ACTIVE_HIGH | MADT_TRIGGER_EDGE;
    buf.extend_from_slice(&nmi_flags.to_le_bytes()); // flags
    buf.push(1);                         // LINT# (LINT1)

    assert_eq!(buf.len(), total);
    fix_checksum(&mut buf, 0, total);

    buf
}

// ── HPET ──────────────────────────────────────────────────────────

const HPET_TABLE_SIZE: u32 = 56;

fn build_hpet() -> Vec<u8> {
    let mut buf = Vec::with_capacity(HPET_TABLE_SIZE as usize);

    // SDT header
    write_header(&mut buf, b"HPET", HPET_TABLE_SIZE, 1);

    // offset 36: Hardware Rev ID / comparators / counter size / etc.
    // Event Timer Block ID (u32 LE) - using 0 (will be filled by kernel)
    buf.extend_from_slice(&0u32.to_le_bytes());

    // offset 40: Base Address (GAS, 12 bytes) - MMIO at HPET_ADDR
    buf.push(0); // address_space = system memory
    buf.push(0); // bit_width
    buf.push(0); // bit_offset
    buf.push(0); // access_size (legacy)
    buf.extend_from_slice(&(HPET_ADDR as u64).to_le_bytes());

    // offset 52: Sequence Number (u8)
    buf.push(0);
    // offset 53: Minimum Clock Tick (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 55: Page Protection and OEM Attribute (u8)
    buf.push(0x04); // ACPI_HPET_PAGE_PROTECT4

    assert_eq!(buf.len(), HPET_TABLE_SIZE as usize);
    fix_checksum(&mut buf, 0, HPET_TABLE_SIZE as usize);

    buf
}

// ── SPCR (Serial Port Console Redirection) ──────────────────────

/// SPCR table size (80 bytes: 36 header + 44 body).
const SPCR_TABLE_SIZE: u32 = 80;

fn build_spcr() -> Vec<u8> {
    let mut buf = Vec::with_capacity(SPCR_TABLE_SIZE as usize);

    write_header(&mut buf, b"SPCR", SPCR_TABLE_SIZE, 1);

    // offset 36: Interface Type (1B) = 16550 compatible
    buf.push(0x00); // Full 16550
    // offset 37-39: Reserved
    buf.extend_from_slice(&[0u8; 3]);

    // offset 40: Base Address (GAS, 12 bytes) - I/O port 0x3F8
    write_gas(&mut buf, ACPI_AS_SYSTEM_IO, 8, 0, ACPI_GAS_BYTE, 0x3F8);

    // offset 52: Interrupt Type (1B)
    buf.push(0x01); // Dual-8259 compatible
    // offset 53: IRQ (1B) - ISA IRQ 4
    buf.push(4);
    // offset 54-57: Global System Interrupt (4B LE)
    buf.extend_from_slice(&4u32.to_le_bytes());
    // offset 58: Baud Rate (1B) - 7 = 115200
    buf.push(7);
    // offset 59: Parity (1B) - 0 = No parity
    buf.push(0);
    // offset 60: Stop Bits (1B) - 1 = 1 stop bit
    buf.push(1);
    // offset 61: Flow Control (1B) - 0 = None
    buf.push(0);
    // offset 62: Terminal Type (1B) - 0 = VT100
    buf.push(0);
    // offset 63: Language (1B) - Reserved
    buf.push(0);
    // offset 64-65: PCI Device ID (2B) - 0xFFFF = not PCI
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
    // offset 66-67: PCI Vendor ID (2B) - 0xFFFF
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
    // offset 68: PCI Bus (1B)
    buf.push(0);
    // offset 69: PCI Device (1B)
    buf.push(0);
    // offset 70: PCI Function (1B)
    buf.push(0);
    // offset 71-74: PCI Flags (4B)
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 75: PCI Segment (1B)
    buf.push(0);
    // offset 76-79: Reserved (4B)
    buf.extend_from_slice(&0u32.to_le_bytes());

    assert_eq!(buf.len(), SPCR_TABLE_SIZE as usize);
    fix_checksum(&mut buf, 0, SPCR_TABLE_SIZE as usize);
    buf
}

// ── RSDT / XSDT ──────────────────────────────────────────────────

fn build_rsdt(table_addrs: &[u32]) -> Vec<u8> {
    let total = ACPI_HDR_SIZE + table_addrs.len() * 4;
    let mut buf = Vec::with_capacity(total);

    write_header(&mut buf, b"RSDT", total as u32, 1);
    for &addr in table_addrs {
        buf.extend_from_slice(&addr.to_le_bytes());
    }

    assert_eq!(buf.len(), total);
    fix_checksum(&mut buf, 0, total);
    buf
}

fn build_xsdt(table_addrs: &[u64]) -> Vec<u8> {
    let total = ACPI_HDR_SIZE + table_addrs.len() * 8;
    let mut buf = Vec::with_capacity(total);

    write_header(&mut buf, b"XSDT", total as u32, 1);
    for &addr in table_addrs {
        buf.extend_from_slice(&addr.to_le_bytes());
    }

    assert_eq!(buf.len(), total);
    fix_checksum(&mut buf, 0, total);
    buf
}

// ── RSDP ──────────────────────────────────────────────────────────

const RSDP_SIZE: usize = 36;

fn build_rsdp(rsdt_addr: u32, xsdt_addr: u64) -> Vec<u8> {
    let mut buf = vec![0u8; RSDP_SIZE];

    // offset 0: Signature "RSD PTR " (8 bytes)
    buf[0..8].copy_from_slice(b"RSD PTR ");
    // offset 8: Checksum (covers first 20 bytes, fixed later)
    buf[8] = 0;
    // offset 9: OEM ID (6 bytes)
    buf[9..15].copy_from_slice(OEM_ID);
    // offset 15: Revision (2 for ACPI 2.0+)
    buf[15] = 2;
    // offset 16: RSDT address (u32 LE)
    buf[16..20].copy_from_slice(&rsdt_addr.to_le_bytes());
    // offset 20: Length (u32 LE) = 36 for RSDP v2
    buf[20..24].copy_from_slice(&(RSDP_SIZE as u32).to_le_bytes());
    // offset 24: XSDT address (u64 LE)
    buf[24..32].copy_from_slice(&xsdt_addr.to_le_bytes());
    // offset 32: Extended checksum (covers all 36 bytes, fixed later)
    buf[32] = 0;
    // offset 33..36: reserved
    buf[33] = 0;
    buf[34] = 0;
    buf[35] = 0;

    // Fix checksum for first 20 bytes (stored at offset 8)
    fix_checksum_at(&mut buf, 8, 0, 20);
    // Fix extended checksum for all 36 bytes (stored at offset 32)
    fix_checksum_at(&mut buf, 32, 0, RSDP_SIZE);

    buf
}

// ── E820 memory map ───────────────────────────────────────────────

/// E820 memory type constants.
pub const E820_TYPE_RAM: u32 = 1;
pub const E820_TYPE_RESERVED: u32 = 2;
#[allow(dead_code)]
pub const E820_TYPE_ACPI: u32 = 3;
#[allow(dead_code)]
pub const E820_TYPE_NVS: u32 = 4;

/// An E820 memory map entry (20 bytes, packed).
///
/// Matches the format expected by QEMU/OVMF via fw_cfg "etc/e820".
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub type_: u32,
}

/// VGA memory hole start.
const E820_VGA_BASE: u64 = 0xA_0000;
/// ROM area end (1 MiB boundary).
const E820_ROM_END: u64 = 0x10_0000;
/// 4 GiB boundary where high memory starts.
const FOUR_GIB: u64 = 0x1_0000_0000;
/// Default lowmem limit (below MMIO hole).
const LOWMEM_LIMIT: u64 = 0xC000_0000;

/// Build structured E820 entries from total VM memory size.
///
/// Returns the entry list for reuse by both fw_cfg and direct boot
/// (Linux boot_params zero page).
pub fn build_e820_entries(mem_size: usize) -> Vec<E820Entry> {
    let mut entries = Vec::with_capacity(4);

    let lowmem = (mem_size as u64).min(LOWMEM_LIMIT);
    let highmem = if mem_size as u64 > LOWMEM_LIMIT {
        mem_size as u64 - LOWMEM_LIMIT
    } else {
        0
    };

    // Low RAM below VGA hole
    if lowmem > 0 {
        let low_end = lowmem.min(E820_VGA_BASE);
        if low_end > 0 {
            entries.push(E820Entry {
                addr: 0,
                size: low_end,
                type_: E820_TYPE_RAM,
            });
        }
    }

    // Reserved: VGA + ROM area [0xA0000, 0x100000)
    entries.push(E820Entry {
        addr: E820_VGA_BASE,
        size: E820_ROM_END - E820_VGA_BASE,
        type_: E820_TYPE_RESERVED,
    });

    // Low RAM above ROM area (if any)
    if lowmem > E820_ROM_END {
        entries.push(E820Entry {
            addr: E820_ROM_END,
            size: lowmem - E820_ROM_END,
            type_: E820_TYPE_RAM,
        });
    }

    // High RAM above 4 GiB
    if highmem > 0 {
        entries.push(E820Entry {
            addr: FOUR_GIB,
            size: highmem,
            type_: E820_TYPE_RAM,
        });
    }

    entries
}

/// Build E820 memory map as bytes for fw_cfg "etc/e820".
pub fn build_e820(mem_size: usize) -> Vec<u8> {
    let entries = build_e820_entries(mem_size);
    let mut buf = Vec::with_capacity(entries.len() * std::mem::size_of::<E820Entry>());
    for entry in &entries {
        buf.extend_from_slice(&entry.addr.to_le_bytes());
        buf.extend_from_slice(&entry.size.to_le_bytes());
        buf.extend_from_slice(&entry.type_.to_le_bytes());
    }
    buf
}

// ── Public API: generate all ACPI tables ──────────────────────────

/// Result of ACPI table generation (legacy: GPA-based pointers).
pub struct AcpiTables {
    /// All tables concatenated, to be written at BHYVE_ACPI_BASE.
    pub data: Vec<u8>,
    /// Individual table entries for fw_cfg (name, data).
    pub fwcfg_entries: Vec<(&'static str, Vec<u8>)>,
}

/// Generate all ACPI tables for the VM (legacy GPA-based layout).
///
/// Tables are laid out contiguously starting at `BHYVE_ACPI_BASE`.
/// Returns the concatenated blob and individual table data for
/// fw_cfg registration.
pub fn generate_acpi_tables(num_cpus: u32) -> AcpiTables {
    let base = BHYVE_ACPI_BASE;
    let mut blob = Vec::new();

    // Build tables in dependency order. We need to know GPAs of
    // FACS and DSDT before building FADT, and GPAs of FADT/MADT/HPET
    // before building RSDT/XSDT.
    //
    // Layout order in memory:
    //   RSDP | FACS | DSDT | FADT | MADT | HPET | RSDT | XSDT

    // Reserve space for RSDP (will be filled in at the end)
    let rsdp_offset = blob.len();
    blob.extend_from_slice(&[0u8; RSDP_SIZE]);
    let rsdp_gpa = base + rsdp_offset as u64;

    // FACS
    let facs_offset = blob.len();
    let facs = build_facs();
    blob.extend_from_slice(&facs);
    let facs_gpa = base + facs_offset as u64;

    // DSDT
    let dsdt_offset = blob.len();
    let dsdt = build_dsdt();
    blob.extend_from_slice(&dsdt);
    let dsdt_gpa = base + dsdt_offset as u64;

    // FADT (needs FACS and DSDT GPAs)
    let fadt_offset = blob.len();
    let fadt = build_fadt(facs_gpa as u32, dsdt_gpa as u32);
    blob.extend_from_slice(&fadt);
    let fadt_gpa = base + fadt_offset as u64;

    // MADT
    let madt_offset = blob.len();
    let madt = build_madt(num_cpus);
    blob.extend_from_slice(&madt);
    let madt_gpa = base + madt_offset as u64;

    // HPET
    let hpet_offset = blob.len();
    let hpet = build_hpet();
    blob.extend_from_slice(&hpet);
    let hpet_gpa = base + hpet_offset as u64;

    // SPCR (serial console redirection to COM1)
    let spcr_offset = blob.len();
    let spcr = build_spcr();
    blob.extend_from_slice(&spcr);
    let spcr_gpa = base + spcr_offset as u64;

    // RSDT (pointers to FADT, MADT, HPET, SPCR as u32)
    let rsdt_offset = blob.len();
    let rsdt = build_rsdt(&[fadt_gpa as u32, madt_gpa as u32, hpet_gpa as u32, spcr_gpa as u32]);
    blob.extend_from_slice(&rsdt);
    let rsdt_gpa = base + rsdt_offset as u64;

    // XSDT (pointers to FADT, MADT, HPET, SPCR as u64)
    let xsdt_offset = blob.len();
    let xsdt = build_xsdt(&[fadt_gpa, madt_gpa, hpet_gpa, spcr_gpa]);
    blob.extend_from_slice(&xsdt);
    let xsdt_gpa = base + xsdt_offset as u64;

    // Now build and place the RSDP at the reserved slot
    let rsdp = build_rsdp(rsdt_gpa as u32, xsdt_gpa);
    blob[rsdp_offset..rsdp_offset + RSDP_SIZE].copy_from_slice(&rsdp);

    // Build fw_cfg entries for ACPI table loader protocol
    let fwcfg_entries = vec![
        ("etc/acpi/rsdp", rsdp),
        ("etc/acpi/tables", blob.clone()),
    ];

    let _ = rsdp_gpa; // used only to place in memory

    AcpiTables {
        data: blob,
        fwcfg_entries,
    }
}

// ── QEMU table loader protocol ──────────────────────────────────
//
// The UEFI firmware reads `etc/table-loader` from fw_cfg. It contains
// an array of 128-byte commands that tell the firmware how to allocate,
// link, and checksum ACPI tables in guest memory.
//
// For the loader protocol, pointer fields in the blobs must contain
// the OFFSET of the target within the source blob, not a GPA. The
// firmware adds the allocated guest address of the source blob to
// produce the final GPA.

/// Size of each loader command entry.
const LOADER_ENTRY_SIZE: usize = 128;

/// Maximum name length in loader commands (same as fw_cfg).
const LOADER_MAX_NAME: usize = 56;

/// Loader command types.
const LOADER_CMD_ALLOC: u32 = 1;
const LOADER_CMD_ADD_POINTER: u32 = 2;
const LOADER_CMD_ADD_CHECKSUM: u32 = 3;

/// Memory zones for ALLOC.
const ZONE_HIGH: u8 = 1;
const ZONE_FSEG: u8 = 2;

/// fw_cfg blob names.
const TABLES_BLOB_NAME: &str = "etc/acpi/tables";
const RSDP_BLOB_NAME: &str = "etc/acpi/rsdp";

/// Layout information for ACPI tables in the fw_cfg blobs.
///
/// The `tables` blob contains all ACPI tables (FACS, DSDT, FADT,
/// MADT, HPET, RSDT, XSDT) concatenated. The `rsdp` blob is
/// separate (36 bytes). Pointer fields contain blob-relative offsets,
/// NOT guest physical addresses.
pub struct AcpiLayout {
    /// The tables blob (all tables except RSDP, concatenated).
    pub tables: Vec<u8>,
    /// The RSDP blob (36 bytes, separate from tables).
    pub rsdp: Vec<u8>,
    /// Offset of FACS within `tables`.
    pub facs_offset: usize,
    /// Size of FACS.
    pub facs_size: usize,
    /// Offset of DSDT within `tables`.
    pub dsdt_offset: usize,
    /// Size of DSDT.
    pub dsdt_size: usize,
    /// Offset of FADT within `tables`.
    pub fadt_offset: usize,
    /// Size of FADT.
    pub fadt_size: usize,
    /// Offset of MADT within `tables`.
    pub madt_offset: usize,
    /// Size of MADT.
    pub madt_size: usize,
    /// Offset of HPET within `tables`.
    pub hpet_offset: usize,
    /// Size of HPET.
    pub hpet_size: usize,
    /// Offset of RSDT within `tables`.
    pub rsdt_offset: usize,
    /// Size of RSDT.
    pub rsdt_size: usize,
    /// Offset of SPCR within `tables`.
    pub spcr_offset: usize,
    /// Size of SPCR.
    pub spcr_size: usize,
    /// Offset of XSDT within `tables`.
    pub xsdt_offset: usize,
    /// Size of XSDT.
    pub xsdt_size: usize,
}

/// Generate ACPI tables with blob-relative offsets for the table
/// loader protocol.
///
/// Unlike [`generate_acpi_tables`], this produces two separate blobs:
/// - `tables`: all tables except RSDP, with pointer fields set to
///   blob-relative offsets (the firmware adds the allocated address)
/// - `rsdp`: the RSDP structure, with rsdt_addr/xsdt_addr set to
///   the offset of RSDT/XSDT within the tables blob
///
/// Checksums are set to 0 in fields that will be recomputed by the
/// firmware via ADD_CHECKSUM commands.
/// Generate ACPI layout with optional DSDT override.
///
/// If `dsdt_override` is Some, uses the provided pre-compiled AML
/// bytecode instead of generating a DSDT from the AML builder.
/// This is useful for testing with iasl-compiled DSDT files.
pub fn generate_acpi_layout_with_dsdt(
    num_cpus: u32,
    dsdt_override: Option<Vec<u8>>,
) -> AcpiLayout {
    generate_acpi_layout_inner(num_cpus, dsdt_override)
}

pub fn generate_acpi_layout(num_cpus: u32) -> AcpiLayout {
    generate_acpi_layout_inner(num_cpus, None)
}

fn generate_acpi_layout_inner(
    num_cpus: u32,
    dsdt_override: Option<Vec<u8>>,
) -> AcpiLayout {
    let mut tables = Vec::new();

    // Layout order in the tables blob:
    //   FACS | DSDT | FADT | MADT | HPET | RSDT | XSDT
    //
    // Pointer fields use blob-relative offsets (not GPAs).

    // FACS (no pointers to patch, no standard checksum)
    let facs_offset = tables.len();
    let facs = build_facs();
    let facs_size = facs.len();
    tables.extend_from_slice(&facs);

    // DSDT (self-contained, no cross-table pointers)
    let dsdt_offset = tables.len();
    let dsdt = dsdt_override.unwrap_or_else(build_dsdt);
    let dsdt_size = dsdt.len();
    tables.extend_from_slice(&dsdt);

    // FADT: pointer fields must contain blob-relative offsets
    //   offset 36: FIRMWARE_CTRL (u32) -> facs_offset
    //   offset 40: DSDT (u32) -> dsdt_offset
    //   offset 132: X_FIRMWARE_CTRL (u64) -> facs_offset
    //   offset 140: X_DSDT (u64) -> dsdt_offset
    let fadt_offset = tables.len();
    let fadt = build_fadt(facs_offset as u32, dsdt_offset as u32);
    let fadt_size = fadt.len();
    tables.extend_from_slice(&fadt);
    // Fix up X_FIRMWARE_CTRL and X_DSDT to use blob offsets too
    // (build_fadt casts the u32 args to u64 for the extended fields)
    let x_facs_off = fadt_offset + 132;
    tables[x_facs_off..x_facs_off + 8]
        .copy_from_slice(&(facs_offset as u64).to_le_bytes());
    let x_dsdt_off = fadt_offset + 140;
    tables[x_dsdt_off..x_dsdt_off + 8]
        .copy_from_slice(&(dsdt_offset as u64).to_le_bytes());
    // Clear FADT checksum -- will be recomputed by loader
    tables[fadt_offset + 9] = 0;

    // MADT
    let madt_offset = tables.len();
    let madt = build_madt(num_cpus);
    let madt_size = madt.len();
    tables.extend_from_slice(&madt);
    // Clear MADT checksum
    tables[madt_offset + 9] = 0;

    // HPET
    let hpet_offset = tables.len();
    let hpet = build_hpet();
    let hpet_size = hpet.len();
    tables.extend_from_slice(&hpet);
    // Clear HPET checksum
    tables[hpet_offset + 9] = 0;

    // SPCR (serial console redirection to COM1)
    let spcr_offset = tables.len();
    let spcr = build_spcr();
    let spcr_size = spcr.len();
    tables.extend_from_slice(&spcr);
    tables[spcr_offset + 9] = 0; // Clear SPCR checksum

    // RSDT: entries are u32 blob offsets to FADT, MADT, HPET, SPCR
    let rsdt_offset = tables.len();
    let rsdt = build_rsdt(&[
        fadt_offset as u32,
        madt_offset as u32,
        hpet_offset as u32,
        spcr_offset as u32,
    ]);
    let rsdt_size = rsdt.len();
    tables.extend_from_slice(&rsdt);
    // Clear RSDT checksum
    tables[rsdt_offset + 9] = 0;

    // XSDT: entries are u64 blob offsets to FADT, MADT, HPET, SPCR
    let xsdt_offset = tables.len();
    let xsdt = build_xsdt(&[
        fadt_offset as u64,
        madt_offset as u64,
        hpet_offset as u64,
        spcr_offset as u64,
    ]);
    let xsdt_size = xsdt.len();
    tables.extend_from_slice(&xsdt);
    // Clear XSDT checksum
    tables[xsdt_offset + 9] = 0;

    // RSDP: rsdt_addr = rsdt_offset, xsdt_addr = xsdt_offset
    // (blob-relative offsets into the tables blob)
    let mut rsdp = build_rsdp(rsdt_offset as u32, xsdt_offset as u64);
    // Clear both checksums -- will be recomputed by loader
    rsdp[8] = 0;
    rsdp[32] = 0;

    // Also clear the DSDT checksum (it's self-contained but the
    // firmware will recompute it for consistency)
    tables[dsdt_offset + 9] = 0;

    AcpiLayout {
        tables,
        rsdp,
        facs_offset,
        facs_size,
        dsdt_offset,
        dsdt_size,
        fadt_offset,
        fadt_size,
        madt_offset,
        madt_size,
        hpet_offset,
        hpet_size,
        spcr_offset,
        spcr_size,
        rsdt_offset,
        rsdt_size,
        xsdt_offset,
        xsdt_size,
    }
}

/// Write a NUL-terminated name into a fixed-size buffer.
///
/// Panics if `name` is too long (>= `LOADER_MAX_NAME` bytes).
fn write_loader_name(buf: &mut [u8; LOADER_MAX_NAME], name: &str) {
    let bytes = name.as_bytes();
    assert!(
        bytes.len() < LOADER_MAX_NAME,
        "loader name too long: {} (max {})",
        name,
        LOADER_MAX_NAME - 1,
    );
    buf[..bytes.len()].copy_from_slice(bytes);
    // Remaining bytes are already 0 (NUL padding)
}

/// Build an ALLOC loader command.
fn loader_alloc_cmd(name: &str, alignment: u32, zone: u8) -> [u8; LOADER_ENTRY_SIZE] {
    let mut entry = [0u8; LOADER_ENTRY_SIZE];

    // offset 0: command (u32 LE)
    entry[0..4].copy_from_slice(&LOADER_CMD_ALLOC.to_le_bytes());

    // offset 4: name (56 bytes, NUL-padded)
    let mut name_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut name_buf, name);
    entry[4..4 + LOADER_MAX_NAME].copy_from_slice(&name_buf);

    // offset 60: alignment (u32 LE)
    entry[60..64].copy_from_slice(&alignment.to_le_bytes());

    // offset 64: zone (u8)
    entry[64] = zone;

    entry
}

/// Build an ADD_POINTER loader command.
///
/// The firmware reads `size` bytes from `dest_name` at `offset`,
/// adds the guest address of `src_name`'s allocated blob, and writes
/// the result back.
fn loader_add_pointer_cmd(
    dest_name: &str,
    src_name: &str,
    offset: u32,
    size: u8,
) -> [u8; LOADER_ENTRY_SIZE] {
    let mut entry = [0u8; LOADER_ENTRY_SIZE];

    // offset 0: command (u32 LE)
    entry[0..4].copy_from_slice(&LOADER_CMD_ADD_POINTER.to_le_bytes());

    // offset 4: dest_name (56 bytes)
    let mut dest_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut dest_buf, dest_name);
    entry[4..4 + LOADER_MAX_NAME].copy_from_slice(&dest_buf);

    // offset 60: src_name (56 bytes)
    let mut src_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut src_buf, src_name);
    entry[60..60 + LOADER_MAX_NAME].copy_from_slice(&src_buf);

    // offset 116: offset (u32 LE)
    entry[116..120].copy_from_slice(&offset.to_le_bytes());

    // offset 120: size (u8)
    entry[120] = size;

    entry
}

/// Build an ADD_CHECKSUM loader command.
///
/// The firmware computes a checksum over `name[start..start+len]`
/// and stores it at `name[cksum_offset]` such that the sum of all
/// bytes in the range is 0.
fn loader_add_checksum_cmd(
    name: &str,
    cksum_offset: u32,
    start: u32,
    len: u32,
) -> [u8; LOADER_ENTRY_SIZE] {
    let mut entry = [0u8; LOADER_ENTRY_SIZE];

    // offset 0: command (u32 LE)
    entry[0..4].copy_from_slice(&LOADER_CMD_ADD_CHECKSUM.to_le_bytes());

    // offset 4: name (56 bytes)
    let mut name_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut name_buf, name);
    entry[4..4 + LOADER_MAX_NAME].copy_from_slice(&name_buf);

    // offset 60: checksum offset (u32 LE)
    entry[60..64].copy_from_slice(&cksum_offset.to_le_bytes());

    // offset 64: start (u32 LE)
    entry[64..68].copy_from_slice(&start.to_le_bytes());

    // offset 68: length (u32 LE)
    entry[68..72].copy_from_slice(&len.to_le_bytes());

    entry
}

/// Generate the `etc/table-loader` fw_cfg entry.
///
/// This creates the array of 128-byte commands that tell the UEFI
/// firmware how to allocate, link, and checksum the ACPI tables.
pub fn generate_table_loader(layout: &AcpiLayout) -> Vec<u8> {
    let mut loader = Vec::new();

    // ── 1. ALLOC commands ──────────────────────────────────────
    // These must come first. The firmware loads the blob data into
    // guest memory when processing ALLOC.

    // Allocate the tables blob (64-byte aligned, above 1MB)
    loader.extend_from_slice(&loader_alloc_cmd(
        TABLES_BLOB_NAME, 0x40, ZONE_HIGH,
    ));

    // Allocate the RSDP (16-byte aligned, FSEG zone for legacy
    // BIOS compatibility, though OVMF doesn't strictly require it)
    loader.extend_from_slice(&loader_alloc_cmd(
        RSDP_BLOB_NAME, 0x10, ZONE_FSEG,
    ));

    // ── 2. ADD_POINTER: RSDP → tables blob ────────────────────

    // RSDP.rsdt_addr (offset 16 in RSDP, 4-byte pointer)
    // Points to RSDT within the tables blob.
    loader.extend_from_slice(&loader_add_pointer_cmd(
        RSDP_BLOB_NAME,
        TABLES_BLOB_NAME,
        16, // offset of rsdt_addr in RSDP
        4,  // 32-bit pointer
    ));

    // RSDP.xsdt_addr (offset 24 in RSDP, 8-byte pointer)
    // Points to XSDT within the tables blob.
    loader.extend_from_slice(&loader_add_pointer_cmd(
        RSDP_BLOB_NAME,
        TABLES_BLOB_NAME,
        24, // offset of xsdt_addr in RSDP
        8,  // 64-bit pointer
    ));

    // ── 3. ADD_POINTER: FADT → FACS, DSDT ─────────────────────

    // FADT.firmware_ctrl (offset 36 in FADT, 4-byte pointer to FACS)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 36) as u32,
        4,
    ));

    // FADT.dsdt (offset 40 in FADT, 4-byte pointer to DSDT)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 40) as u32,
        4,
    ));

    // FADT.x_firmware_ctrl (offset 132 in FADT, 8-byte pointer to FACS)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 132) as u32,
        8,
    ));

    // FADT.x_dsdt (offset 140 in FADT, 8-byte pointer to DSDT)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 140) as u32,
        8,
    ));

    // ── 4. ADD_POINTER: RSDT entries → FADT, MADT, HPET ──────
    // RSDT entries are 4-byte pointers starting at offset 36
    // (after the 36-byte ACPI header). Order: FADT, MADT, HPET.

    for (i, _table_offset) in [
        layout.fadt_offset,
        layout.madt_offset,
        layout.hpet_offset,
        layout.spcr_offset,
    ].iter().enumerate() {
        loader.extend_from_slice(&loader_add_pointer_cmd(
            TABLES_BLOB_NAME,
            TABLES_BLOB_NAME,
            (layout.rsdt_offset + ACPI_HDR_SIZE + i * 4) as u32,
            4,
        ));
    }

    // ── 5. ADD_POINTER: XSDT entries → FADT, MADT, HPET, SPCR ──
    // XSDT entries are 8-byte pointers starting at offset 36.

    for (i, _table_offset) in [
        layout.fadt_offset,
        layout.madt_offset,
        layout.hpet_offset,
        layout.spcr_offset,
    ].iter().enumerate() {
        loader.extend_from_slice(&loader_add_pointer_cmd(
            TABLES_BLOB_NAME,
            TABLES_BLOB_NAME,
            (layout.xsdt_offset + ACPI_HDR_SIZE + i * 8) as u32,
            8,
        ));
    }

    // ── 6. ADD_CHECKSUM commands ──────────────────────────────
    // Each table with a standard ACPI header has a checksum at
    // offset 9 that covers the entire table.

    // RSDP has two checksums:
    //   offset 8: covers bytes 0..20 (legacy RSDP v1 portion)
    //   offset 32: covers bytes 0..36 (full RSDP v2)
    loader.extend_from_slice(&loader_add_checksum_cmd(
        RSDP_BLOB_NAME, 8, 0, 20,
    ));
    loader.extend_from_slice(&loader_add_checksum_cmd(
        RSDP_BLOB_NAME, 32, 0, RSDP_SIZE as u32,
    ));

    // FADT checksum (offset 9 relative to start of FADT)
    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 9) as u32,
        layout.fadt_offset as u32,
        layout.fadt_size as u32,
    ));

    // DSDT checksum
    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.dsdt_offset + 9) as u32,
        layout.dsdt_offset as u32,
        layout.dsdt_size as u32,
    ));

    // MADT checksum
    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.madt_offset + 9) as u32,
        layout.madt_offset as u32,
        layout.madt_size as u32,
    ));

    // HPET checksum
    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.hpet_offset + 9) as u32,
        layout.hpet_offset as u32,
        layout.hpet_size as u32,
    ));

    // SPCR checksum
    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.spcr_offset + 9) as u32,
        layout.spcr_offset as u32,
        layout.spcr_size as u32,
    ));

    // RSDT checksum
    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.rsdt_offset + 9) as u32,
        layout.rsdt_offset as u32,
        layout.rsdt_size as u32,
    ));

    // XSDT checksum
    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.xsdt_offset + 9) as u32,
        layout.xsdt_offset as u32,
        layout.xsdt_size as u32,
    ));

    // Note: FACS has no standard ACPI checksum (no SDT header).

    loader
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the sum of all bytes in a range is 0 mod 256.
    fn verify_checksum(data: &[u8]) -> bool {
        data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0
    }

    #[test]
    fn facs_valid() {
        let facs = build_facs();
        assert_eq!(facs.len(), FACS_SIZE);
        assert_eq!(&facs[0..4], b"FACS");
        let len = u32::from_le_bytes([facs[4], facs[5], facs[6], facs[7]]);
        assert_eq!(len, FACS_SIZE as u32);
    }

    #[test]
    fn dsdt_checksum() {
        let dsdt = build_dsdt();
        assert_eq!(&dsdt[0..4], b"DSDT");
        assert!(verify_checksum(&dsdt));
    }

    #[test]
    fn dsdt_has_aml_body() {
        let dsdt = build_dsdt();
        // The new DSDT should be larger than just the header
        assert!(
            dsdt.len() > ACPI_HDR_SIZE,
            "DSDT should contain AML bytecode, but is only {} bytes",
            dsdt.len(),
        );
        // Verify header length field matches actual length
        let hdr_len = u32::from_le_bytes([
            dsdt[4], dsdt[5], dsdt[6], dsdt[7],
        ]) as usize;
        assert_eq!(hdr_len, dsdt.len());
    }

    #[test]
    fn dsdt_contains_scope_op() {
        let dsdt = build_dsdt();
        let body = &dsdt[ACPI_HDR_SIZE..];
        // The AML body should contain a ScopeOp (0x10) for \_SB
        assert!(
            body.contains(&0x10),
            "DSDT body should contain ScopeOp",
        );
    }

    #[test]
    fn dsdt_contains_device_ops() {
        let dsdt = build_dsdt();
        let body = &dsdt[ACPI_HDR_SIZE..];
        // DeviceOp is ExtPrefix(0x5B) + 0x82
        let has_device = body.windows(2).any(|w| w == [0x5B, 0x82]);
        assert!(
            has_device,
            "DSDT body should contain at least one DeviceOp",
        );
    }

    #[test]
    fn dsdt_contains_pci_eisa_id() {
        let dsdt = build_dsdt();
        // PNP0A03 EISA ID = 0x030AD041 in LE bytes: [0x41, 0xD0, 0x0A, 0x03]
        let eisa_bytes = 0x030AD041u32.to_le_bytes();
        let has_pci_id = dsdt.windows(4).any(|w| w == eisa_bytes);
        assert!(
            has_pci_id,
            "DSDT should contain PNP0A03 (PCI host bridge) EISA ID",
        );
    }

    #[test]
    fn dsdt_contains_uart_eisa_id() {
        let dsdt = build_dsdt();
        // PNP0501 EISA ID = 0x0105D041
        let eisa_bytes = 0x0105D041u32.to_le_bytes();
        let count = dsdt.windows(4).filter(|w| *w == eisa_bytes).count();
        // COM1 and COM2 both use PNP0501
        assert!(
            count >= 2,
            "DSDT should contain at least 2 PNP0501 (UART) EISA IDs, found {}",
            count,
        );
    }

    #[test]
    fn dsdt_contains_hpet_address() {
        let dsdt = build_dsdt();
        let hpet_bytes = HPET_ADDR.to_le_bytes();
        let has_hpet = dsdt.windows(4).any(|w| w == hpet_bytes);
        assert!(
            has_hpet,
            "DSDT should contain HPET address 0x{:08X}",
            HPET_ADDR,
        );
    }

    #[test]
    fn dsdt_contains_com1_port() {
        let dsdt = build_dsdt();
        // COM1 at 0x3F8
        let port_bytes = 0x03F8u16.to_le_bytes();
        let has_com1 = dsdt.windows(2).any(|w| w == port_bytes);
        assert!(
            has_com1,
            "DSDT should contain COM1 I/O port 0x3F8",
        );
    }

    #[test]
    fn fadt_checksum() {
        let fadt = build_fadt(0x1000, 0x2000);
        assert_eq!(fadt.len(), FADT_SIZE as usize);
        assert_eq!(&fadt[0..4], b"FACP");
        assert!(verify_checksum(&fadt));
    }

    #[test]
    fn fadt_pm_timer_port() {
        let fadt = build_fadt(0x1000, 0x2000);
        // PM_TMR_BLK is at offset 76
        let pmtmr = u32::from_le_bytes([
            fadt[76], fadt[77], fadt[78], fadt[79],
        ]);
        assert_eq!(pmtmr, IO_PMTMR);
        assert_eq!(pmtmr, 0x408);
    }

    #[test]
    fn fadt_facs_dsdt_pointers() {
        let fadt = build_fadt(0xAAAA, 0xBBBB);
        // FACS at offset 36
        let facs_ptr = u32::from_le_bytes([
            fadt[36], fadt[37], fadt[38], fadt[39],
        ]);
        assert_eq!(facs_ptr, 0xAAAA);
        // DSDT at offset 40
        let dsdt_ptr = u32::from_le_bytes([
            fadt[40], fadt[41], fadt[42], fadt[43],
        ]);
        assert_eq!(dsdt_ptr, 0xBBBB);
    }

    #[test]
    fn madt_checksum() {
        let madt = build_madt(4);
        assert_eq!(&madt[0..4], b"APIC");
        assert!(verify_checksum(&madt));
    }

    #[test]
    fn madt_cpu_entries() {
        let madt = build_madt(4);
        // After header (36) + MADT-specific (8), LAPIC entries start
        let lapic_start = 44;
        for i in 0..4u8 {
            let base = lapic_start + i as usize * 8;
            assert_eq!(madt[base], MADT_TYPE_LOCAL_APIC);
            assert_eq!(madt[base + 1], 8); // length
            assert_eq!(madt[base + 2], i); // processor ID
            assert_eq!(madt[base + 3], i); // APIC ID
        }
    }

    #[test]
    fn madt_ioapic_entry() {
        let madt = build_madt(1);
        // After header + MADT-specific (44) + 1 LAPIC (8) = 52
        let ioapic_offset = 52;
        assert_eq!(madt[ioapic_offset], MADT_TYPE_IO_APIC);
        assert_eq!(madt[ioapic_offset + 1], 12);
        let addr = u32::from_le_bytes([
            madt[ioapic_offset + 4],
            madt[ioapic_offset + 5],
            madt[ioapic_offset + 6],
            madt[ioapic_offset + 7],
        ]);
        assert_eq!(addr, IOAPIC_ADDR);
    }

    #[test]
    fn hpet_checksum() {
        let hpet = build_hpet();
        assert_eq!(hpet.len(), HPET_TABLE_SIZE as usize);
        assert_eq!(&hpet[0..4], b"HPET");
        assert!(verify_checksum(&hpet));
    }

    #[test]
    fn rsdt_checksum() {
        let rsdt = build_rsdt(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(&rsdt[0..4], b"RSDT");
        assert!(verify_checksum(&rsdt));
        // Should have 3 pointers after header
        assert_eq!(rsdt.len(), ACPI_HDR_SIZE + 12);
    }

    #[test]
    fn xsdt_checksum() {
        let xsdt = build_xsdt(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(&xsdt[0..4], b"XSDT");
        assert!(verify_checksum(&xsdt));
        // Should have 3 pointers (8 bytes each) after header
        assert_eq!(xsdt.len(), ACPI_HDR_SIZE + 24);
    }

    #[test]
    fn rsdp_checksums() {
        let rsdp = build_rsdp(0x1000, 0x2000);
        assert_eq!(rsdp.len(), RSDP_SIZE);
        assert_eq!(&rsdp[0..8], b"RSD PTR ");

        // First 20 bytes must checksum to 0
        let sum20: u8 = rsdp[..20]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum20, 0);

        // All 36 bytes must checksum to 0
        let sum36: u8 = rsdp
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum36, 0);
    }

    #[test]
    fn rsdp_revision() {
        let rsdp = build_rsdp(0, 0);
        assert_eq!(rsdp[15], 2); // ACPI 2.0+
    }

    #[test]
    fn e820_small_vm() {
        // 256 MiB VM: all low memory, no high memory
        let mem = 256 * 1024 * 1024;
        let data = build_e820(mem);
        let entry_size = 20;

        // Should have 3 entries: low RAM, reserved, low RAM above 1M
        assert_eq!(data.len(), 3 * entry_size);

        // First entry: [0, 0xA0000) RAM
        let addr0 = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let size0 = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let type0 = u32::from_le_bytes(data[16..20].try_into().unwrap());
        assert_eq!(addr0, 0);
        assert_eq!(size0, 0xA_0000);
        assert_eq!(type0, E820_TYPE_RAM);

        // Second entry: [0xA0000, 0x100000) Reserved
        let addr1 = u64::from_le_bytes(data[20..28].try_into().unwrap());
        let type1 = u32::from_le_bytes(data[36..40].try_into().unwrap());
        assert_eq!(addr1, 0xA_0000);
        assert_eq!(type1, E820_TYPE_RESERVED);

        // Third entry: [0x100000, lowmem) RAM
        let addr2 = u64::from_le_bytes(data[40..48].try_into().unwrap());
        let size2 = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let type2 = u32::from_le_bytes(data[56..60].try_into().unwrap());
        assert_eq!(addr2, 0x10_0000);
        assert_eq!(size2, mem as u64 - 0x10_0000);
        assert_eq!(type2, E820_TYPE_RAM);
    }

    #[test]
    fn e820_large_vm() {
        // 4 GiB VM: has high memory above 4 GiB
        let mem = 4 * 1024 * 1024 * 1024usize;
        let data = build_e820(mem);
        let entry_size = 20;

        // Should have 4 entries
        assert_eq!(data.len(), 4 * entry_size);

        // Last entry should be high memory
        let last_off = 3 * entry_size;
        let addr = u64::from_le_bytes(
            data[last_off..last_off + 8].try_into().unwrap()
        );
        let size = u64::from_le_bytes(
            data[last_off + 8..last_off + 16].try_into().unwrap()
        );
        let type_ = u32::from_le_bytes(
            data[last_off + 16..last_off + 20].try_into().unwrap()
        );
        assert_eq!(addr, FOUR_GIB);
        assert_eq!(size, mem as u64 - LOWMEM_LIMIT);
        assert_eq!(type_, E820_TYPE_RAM);
    }

    #[test]
    fn e820_no_overlaps() {
        let mem = 2 * 1024 * 1024 * 1024usize;
        let data = build_e820(mem);
        let entry_size = 20;
        let count = data.len() / entry_size;

        for i in 1..count {
            let prev_addr = u64::from_le_bytes(
                data[(i - 1) * entry_size..(i - 1) * entry_size + 8]
                    .try_into().unwrap()
            );
            let prev_size = u64::from_le_bytes(
                data[(i - 1) * entry_size + 8..(i - 1) * entry_size + 16]
                    .try_into().unwrap()
            );
            let cur_addr = u64::from_le_bytes(
                data[i * entry_size..i * entry_size + 8]
                    .try_into().unwrap()
            );
            assert!(
                prev_addr + prev_size <= cur_addr,
                "E820 entries overlap: entry {} ends at {:#x} but entry {} starts at {:#x}",
                i - 1, prev_addr + prev_size, i, cur_addr,
            );
        }
    }

    #[test]
    fn full_table_generation() {
        let tables = generate_acpi_tables(4);
        assert!(!tables.data.is_empty());
        assert_eq!(tables.fwcfg_entries.len(), 2);
        assert_eq!(tables.fwcfg_entries[0].0, "etc/acpi/rsdp");
        assert_eq!(tables.fwcfg_entries[1].0, "etc/acpi/tables");
    }

    #[test]
    fn full_table_rsdp_valid() {
        let tables = generate_acpi_tables(2);

        // RSDP is at the start of the blob
        let rsdp = &tables.data[..RSDP_SIZE];
        assert_eq!(&rsdp[0..8], b"RSD PTR ");

        // Verify checksums
        let sum20: u8 = rsdp[..20]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum20, 0);
        let sum36: u8 = rsdp
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum36, 0);
    }

    // ── Table loader protocol tests ──────────────────────────────

    #[test]
    fn layout_rsdp_separate_from_tables() {
        let layout = generate_acpi_layout(2);
        // RSDP is 36 bytes
        assert_eq!(layout.rsdp.len(), RSDP_SIZE);
        assert_eq!(&layout.rsdp[0..8], b"RSD PTR ");
        // Tables blob should NOT start with RSDP
        assert_ne!(&layout.tables[0..8], b"RSD PTR ");
        // Tables blob should start with FACS
        assert_eq!(&layout.tables[0..4], b"FACS");
    }

    #[test]
    fn layout_table_signatures() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        // Verify each table starts with the correct signature
        assert_eq!(&t[layout.facs_offset..layout.facs_offset + 4], b"FACS");
        assert_eq!(&t[layout.dsdt_offset..layout.dsdt_offset + 4], b"DSDT");
        assert_eq!(&t[layout.fadt_offset..layout.fadt_offset + 4], b"FACP");
        assert_eq!(&t[layout.madt_offset..layout.madt_offset + 4], b"APIC");
        assert_eq!(&t[layout.hpet_offset..layout.hpet_offset + 4], b"HPET");
        assert_eq!(&t[layout.rsdt_offset..layout.rsdt_offset + 4], b"RSDT");
        assert_eq!(&t[layout.xsdt_offset..layout.xsdt_offset + 4], b"XSDT");
    }

    #[test]
    fn layout_fadt_has_blob_offsets() {
        let layout = generate_acpi_layout(1);
        let t = &layout.tables;

        // FADT.firmware_ctrl (offset 36) should be facs_offset
        let facs_ptr = u32::from_le_bytes(
            t[layout.fadt_offset + 36..layout.fadt_offset + 40]
                .try_into().unwrap()
        );
        assert_eq!(facs_ptr as usize, layout.facs_offset);

        // FADT.dsdt (offset 40) should be dsdt_offset
        let dsdt_ptr = u32::from_le_bytes(
            t[layout.fadt_offset + 40..layout.fadt_offset + 44]
                .try_into().unwrap()
        );
        assert_eq!(dsdt_ptr as usize, layout.dsdt_offset);

        // FADT.x_firmware_ctrl (offset 132) should be facs_offset
        let x_facs = u64::from_le_bytes(
            t[layout.fadt_offset + 132..layout.fadt_offset + 140]
                .try_into().unwrap()
        );
        assert_eq!(x_facs as usize, layout.facs_offset);

        // FADT.x_dsdt (offset 140) should be dsdt_offset
        let x_dsdt = u64::from_le_bytes(
            t[layout.fadt_offset + 140..layout.fadt_offset + 148]
                .try_into().unwrap()
        );
        assert_eq!(x_dsdt as usize, layout.dsdt_offset);
    }

    #[test]
    fn layout_rsdt_has_blob_offsets() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        // RSDT entries at offset 36 (after header): 3 x u32
        let entry0 = u32::from_le_bytes(
            t[layout.rsdt_offset + 36..layout.rsdt_offset + 40]
                .try_into().unwrap()
        );
        let entry1 = u32::from_le_bytes(
            t[layout.rsdt_offset + 40..layout.rsdt_offset + 44]
                .try_into().unwrap()
        );
        let entry2 = u32::from_le_bytes(
            t[layout.rsdt_offset + 44..layout.rsdt_offset + 48]
                .try_into().unwrap()
        );
        assert_eq!(entry0 as usize, layout.fadt_offset);
        assert_eq!(entry1 as usize, layout.madt_offset);
        assert_eq!(entry2 as usize, layout.hpet_offset);
    }

    #[test]
    fn layout_xsdt_has_blob_offsets() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        // XSDT entries at offset 36 (after header): 3 x u64
        let entry0 = u64::from_le_bytes(
            t[layout.xsdt_offset + 36..layout.xsdt_offset + 44]
                .try_into().unwrap()
        );
        let entry1 = u64::from_le_bytes(
            t[layout.xsdt_offset + 44..layout.xsdt_offset + 52]
                .try_into().unwrap()
        );
        let entry2 = u64::from_le_bytes(
            t[layout.xsdt_offset + 52..layout.xsdt_offset + 60]
                .try_into().unwrap()
        );
        assert_eq!(entry0 as usize, layout.fadt_offset);
        assert_eq!(entry1 as usize, layout.madt_offset);
        assert_eq!(entry2 as usize, layout.hpet_offset);
    }

    #[test]
    fn layout_rsdp_has_blob_offsets() {
        let layout = generate_acpi_layout(2);

        // RSDP.rsdt_addr (offset 16) should be rsdt_offset
        let rsdt_addr = u32::from_le_bytes(
            layout.rsdp[16..20].try_into().unwrap()
        );
        assert_eq!(rsdt_addr as usize, layout.rsdt_offset);

        // RSDP.xsdt_addr (offset 24) should be xsdt_offset
        let xsdt_addr = u64::from_le_bytes(
            layout.rsdp[24..32].try_into().unwrap()
        );
        assert_eq!(xsdt_addr as usize, layout.xsdt_offset);
    }

    #[test]
    fn layout_checksums_are_zeroed() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        // RSDP checksums should be 0 (firmware will compute them)
        assert_eq!(layout.rsdp[8], 0, "RSDP v1 checksum should be 0");
        assert_eq!(layout.rsdp[32], 0, "RSDP v2 checksum should be 0");

        // SDT table checksums at offset 9 should be 0
        assert_eq!(t[layout.dsdt_offset + 9], 0, "DSDT checksum should be 0");
        assert_eq!(t[layout.fadt_offset + 9], 0, "FADT checksum should be 0");
        assert_eq!(t[layout.madt_offset + 9], 0, "MADT checksum should be 0");
        assert_eq!(t[layout.hpet_offset + 9], 0, "HPET checksum should be 0");
        assert_eq!(t[layout.rsdt_offset + 9], 0, "RSDT checksum should be 0");
        assert_eq!(t[layout.xsdt_offset + 9], 0, "XSDT checksum should be 0");
    }

    #[test]
    fn loader_entry_size_is_128() {
        assert_eq!(LOADER_ENTRY_SIZE, 128);
        assert_eq!(
            std::mem::size_of::<[u8; LOADER_ENTRY_SIZE]>(),
            128,
        );
    }

    #[test]
    fn loader_alloc_cmd_format() {
        let cmd = loader_alloc_cmd("etc/acpi/tables", 0x40, ZONE_HIGH);
        assert_eq!(cmd.len(), 128);

        // Command type
        let cmd_type = u32::from_le_bytes(cmd[0..4].try_into().unwrap());
        assert_eq!(cmd_type, LOADER_CMD_ALLOC);

        // Name
        assert_eq!(&cmd[4..4 + 15], b"etc/acpi/tables");
        assert_eq!(cmd[4 + 15], 0); // NUL terminated

        // Alignment
        let align = u32::from_le_bytes(cmd[60..64].try_into().unwrap());
        assert_eq!(align, 0x40);

        // Zone
        assert_eq!(cmd[64], ZONE_HIGH);

        // Rest should be zero
        for &b in &cmd[65..] {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn loader_add_pointer_cmd_format() {
        let cmd = loader_add_pointer_cmd(
            "etc/acpi/rsdp", "etc/acpi/tables", 16, 4,
        );
        assert_eq!(cmd.len(), 128);

        let cmd_type = u32::from_le_bytes(cmd[0..4].try_into().unwrap());
        assert_eq!(cmd_type, LOADER_CMD_ADD_POINTER);

        // dest_name
        assert_eq!(&cmd[4..4 + 13], b"etc/acpi/rsdp");
        // src_name
        assert_eq!(&cmd[60..60 + 15], b"etc/acpi/tables");

        // offset
        let off = u32::from_le_bytes(cmd[116..120].try_into().unwrap());
        assert_eq!(off, 16);

        // size
        assert_eq!(cmd[120], 4);
    }

    #[test]
    fn loader_add_checksum_cmd_format() {
        let cmd = loader_add_checksum_cmd("etc/acpi/rsdp", 8, 0, 20);
        assert_eq!(cmd.len(), 128);

        let cmd_type = u32::from_le_bytes(cmd[0..4].try_into().unwrap());
        assert_eq!(cmd_type, LOADER_CMD_ADD_CHECKSUM);

        // name
        assert_eq!(&cmd[4..4 + 13], b"etc/acpi/rsdp");

        // checksum offset
        let cksum_off = u32::from_le_bytes(cmd[60..64].try_into().unwrap());
        assert_eq!(cksum_off, 8);

        // start
        let start = u32::from_le_bytes(cmd[64..68].try_into().unwrap());
        assert_eq!(start, 0);

        // length
        let length = u32::from_le_bytes(cmd[68..72].try_into().unwrap());
        assert_eq!(length, 20);
    }

    #[test]
    fn loader_total_size_is_multiple_of_128() {
        let layout = generate_acpi_layout(4);
        let loader = generate_table_loader(&layout);
        assert_eq!(loader.len() % LOADER_ENTRY_SIZE, 0);
        assert!(!loader.is_empty());
    }

    #[test]
    fn loader_has_expected_command_count() {
        let layout = generate_acpi_layout(2);
        let loader = generate_table_loader(&layout);
        let count = loader.len() / LOADER_ENTRY_SIZE;

        // Expected commands:
        //   2 ALLOC (tables, rsdp)
        //   2 ADD_POINTER (RSDP -> RSDT, RSDP -> XSDT)
        //   4 ADD_POINTER (FADT -> FACS x2, FADT -> DSDT x2)
        //   4 ADD_POINTER (RSDT entries: FADT, MADT, HPET, SPCR)
        //   4 ADD_POINTER (XSDT entries: FADT, MADT, HPET, SPCR)
        //   2 ADD_CHECKSUM (RSDP v1, RSDP v2)
        //   7 ADD_CHECKSUM (FADT, DSDT, MADT, HPET, SPCR, RSDT, XSDT)
        // Total: 25
        assert_eq!(count, 25, "expected 25 loader commands, got {}", count);
    }

    #[test]
    fn loader_allocs_come_first() {
        let layout = generate_acpi_layout(1);
        let loader = generate_table_loader(&layout);

        // First two entries should be ALLOC commands
        let cmd0 = u32::from_le_bytes(
            loader[0..4].try_into().unwrap()
        );
        let cmd1 = u32::from_le_bytes(
            loader[128..132].try_into().unwrap()
        );
        assert_eq!(cmd0, LOADER_CMD_ALLOC);
        assert_eq!(cmd1, LOADER_CMD_ALLOC);

        // Third entry should NOT be an ALLOC
        let cmd2 = u32::from_le_bytes(
            loader[256..260].try_into().unwrap()
        );
        assert_ne!(cmd2, LOADER_CMD_ALLOC);
    }

    #[test]
    #[should_panic(expected = "loader name too long")]
    fn loader_name_too_long_panics() {
        let long_name = "a".repeat(56); // exactly 56 = too long (max 55 + NUL)
        loader_alloc_cmd(&long_name, 1, ZONE_HIGH);
    }

    #[test]
    fn loader_simulated_firmware_pointer_patching() {
        // Simulate what the firmware does: allocate blobs, then
        // patch pointers by adding the base address.
        let layout = generate_acpi_layout(2);

        // Simulate firmware allocating tables blob at some address
        let tables_base: u64 = 0x7FFE_0000;
        let mut tables = layout.tables.clone();

        // Patch FADT.firmware_ctrl: currently holds facs_offset,
        // firmware adds tables_base
        let off = layout.fadt_offset + 36;
        let val = u32::from_le_bytes(
            tables[off..off + 4].try_into().unwrap()
        );
        assert_eq!(val as usize, layout.facs_offset);
        let patched = val as u64 + tables_base;
        tables[off..off + 4].copy_from_slice(
            &(patched as u32).to_le_bytes()
        );

        // The patched value should be the GPA of FACS
        assert_eq!(
            patched,
            tables_base + layout.facs_offset as u64,
        );

        // Same for RSDT entry 0 (FADT)
        let off = layout.rsdt_offset + ACPI_HDR_SIZE;
        let val = u32::from_le_bytes(
            tables[off..off + 4].try_into().unwrap()
        );
        assert_eq!(val as usize, layout.fadt_offset);
        let patched = val as u64 + tables_base;
        assert_eq!(
            patched,
            tables_base + layout.fadt_offset as u64,
        );
    }
}
