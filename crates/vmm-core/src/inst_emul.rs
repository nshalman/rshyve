// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! x86 instruction emulation for VM_EXITCODE_INST_EMUL.
//!
//! When the bhyve kernel cannot fully emulate an instruction that
//! accesses unmapped guest physical memory (e.g., MMIO or unmapped
//! NPT regions), it exits to userspace with the instruction bytes.
//!
//! This module decodes the instruction, determines what memory
//! operation it performs, emulates that operation (reading/writing
//! guest memory or MMIO), updates guest registers, and advances RIP.
//!
//! # Security
//!
//! The instruction bytes come from guest-controlled memory. The
//! decoder must handle arbitrary byte sequences without panicking.
//! All register reads/writes go through the kernel vCPU interface.
//! All memory accesses go through the PhysMap or MMIO bus.

use std::io::{Error, ErrorKind, Result};

use crate::mem::PhysMap;
use crate::mmio::MmioBus;
use crate::vcpu::Vcpu;

use bhyve_api::vm_reg_name;

/// Emulate an instruction that triggered VM_EXITCODE_INST_EMUL.
///
/// # Arguments
/// - `vcpu`: the vCPU that exited
/// - `rip`: guest instruction pointer at the time of exit
/// - `inst`: instruction bytes (up to 15)
/// - `num_valid`: number of valid bytes in `inst`
/// - `physmap`: guest physical memory map
/// - `mmio_bus`: MMIO bus for device dispatch
///
/// # Returns
/// - `Ok(true)` if the instruction was successfully emulated
/// - `Ok(false)` if the instruction is not supported
/// - `Err(e)` if a register read/write failed
pub fn emulate_inst(
    vcpu: &Vcpu,
    rip: u64,
    inst: &[u8; 15],
    num_valid: u8,
    physmap: &PhysMap,
    mmio_bus: &MmioBus,
) -> Result<bool> {
    let bytes = &inst[..num_valid as usize];
    if bytes.is_empty() {
        return Ok(false);
    }

    // Simple x86-64 instruction decoder for common MMIO patterns.
    // We handle the subset of instructions that UEFI firmware
    // commonly uses for memory-mapped I/O and stack operations
    // when the target GPA has no backing memory.
    let mut ctx = DecodeCtx::new(vcpu, rip, bytes)?;
    ctx.decode_and_emulate(physmap, mmio_bus)
}

/// Decoding context with guest register state.
struct DecodeCtx<'a> {
    vcpu: &'a Vcpu,
    rip: u64,
    bytes: &'a [u8],
    pos: usize,
    // Prefix state
    rex: u8,
    has_rex: bool,
    operand_size_override: bool,
    addr_size_override: bool,
    rep_prefix: bool,
    // Register cache
    rsp: u64,
    rflags: u64,
}

impl<'a> DecodeCtx<'a> {
    fn new(vcpu: &'a Vcpu, rip: u64, bytes: &'a [u8]) -> Result<Self> {
        let rsp = vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RSP)?;
        let rflags = vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RFLAGS)?;
        Ok(Self {
            vcpu,
            rip,
            bytes,
            pos: 0,
            rex: 0,
            has_rex: false,
            operand_size_override: false,
            addr_size_override: false,
            rep_prefix: false,
            rsp,
            rflags,
        })
    }

    fn next_byte(&mut self) -> Option<u8> {
        if self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            self.pos += 1;
            Some(b)
        } else {
            None
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Operand size in bytes (depends on REX.W and 0x66 prefix).
    fn operand_size(&self) -> usize {
        if self.has_rex && (self.rex & 0x08) != 0 {
            8 // REX.W = 64-bit
        } else if self.operand_size_override {
            2 // 0x66 prefix = 16-bit
        } else {
            4 // default 32-bit in 64-bit mode
        }
    }

    fn decode_and_emulate(
        &mut self,
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<bool> {
        // Parse prefixes
        loop {
            match self.peek_byte() {
                Some(0x66) => {
                    self.operand_size_override = true;
                    self.next_byte();
                }
                Some(0x67) => {
                    self.addr_size_override = true;
                    self.next_byte();
                }
                Some(0xF3) => {
                    self.rep_prefix = true;
                    self.next_byte();
                }
                Some(0xF2) => {
                    // REPNE prefix — consume but ignore
                    self.next_byte();
                }
                // Segment overrides (ignored in 64-bit mode for flat segments)
                Some(0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65) => {
                    self.next_byte();
                }
                Some(b) if (0x40..=0x4F).contains(&b) => {
                    // REX prefix
                    self.rex = b;
                    self.has_rex = true;
                    self.next_byte();
                }
                _ => break,
            }
        }

        let opcode = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };

        match opcode {
            // CALL rel32
            0xE8 => self.emulate_call_rel32(physmap, mmio_bus),
            // RET
            0xC3 => self.emulate_ret(physmap, mmio_bus),
            // PUSH r64
            0x50..=0x57 => {
                let reg = (opcode - 0x50) | if self.has_rex && (self.rex & 0x01) != 0 { 8 } else { 0 };
                self.emulate_push_reg(reg, physmap, mmio_bus)
            }
            // POP r64
            0x58..=0x5F => {
                let reg = (opcode - 0x58) | if self.has_rex && (self.rex & 0x01) != 0 { 8 } else { 0 };
                self.emulate_pop_reg(reg, physmap, mmio_bus)
            }
            // MOV r/m, r (0x89) and MOV r, r/m (0x8B)
            0x89 | 0x8B => self.emulate_mov_modrm(opcode, physmap, mmio_bus),
            // MOV r/m8, r8 (0x88) and MOV r8, r/m8 (0x8A)
            0x88 | 0x8A => self.emulate_mov_modrm_byte(opcode, physmap, mmio_bus),
            // MOV r/m, imm32 (0xC7 /0)
            0xC7 => self.emulate_mov_imm(physmap, mmio_bus),
            // MOV r/m8, imm8 (0xC6 /0)
            0xC6 => self.emulate_mov_imm8(physmap, mmio_bus),
            // Two-byte opcodes
            0x0F => {
                let op2 = match self.next_byte() {
                    Some(b) => b,
                    None => return Ok(false),
                };
                match op2 {
                    // MOVZX r, r/m8
                    0xB6 => self.emulate_movzx_byte(physmap, mmio_bus),
                    // MOVZX r, r/m16
                    0xB7 => self.emulate_movzx_word(physmap, mmio_bus),
                    _ => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }

    /// CALL rel32: push return address, jump to target.
    fn emulate_call_rel32(
        &mut self,
        physmap: &PhysMap,
        _mmio_bus: &MmioBus,
    ) -> Result<bool> {
        // Read the 32-bit relative offset
        if self.pos + 4 > self.bytes.len() {
            return Ok(false);
        }
        let rel32 = i32::from_le_bytes([
            self.bytes[self.pos],
            self.bytes[self.pos + 1],
            self.bytes[self.pos + 2],
            self.bytes[self.pos + 3],
        ]);
        let inst_len = self.pos + 4; // total instruction length

        // Return address = RIP + inst_len
        let ret_addr = self.rip.wrapping_add(inst_len as u64);
        // Target = RIP + inst_len + rel32
        let target = ret_addr.wrapping_add(rel32 as i64 as u64);

        // Push return address onto stack
        let new_rsp = self.rsp.wrapping_sub(8);
        self.write_mem(new_rsp, &ret_addr.to_le_bytes(), physmap)?;

        // Update RSP and RIP
        self.vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RSP, new_rsp)?;
        self.vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RIP, target)?;

        Ok(true)
    }

    /// RET: pop return address, jump to it.
    fn emulate_ret(
        &mut self,
        physmap: &PhysMap,
        _mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let mut buf = [0u8; 8];
        self.read_mem(self.rsp, &mut buf, physmap)?;
        let ret_addr = u64::from_le_bytes(buf);

        let new_rsp = self.rsp.wrapping_add(8);
        self.vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RSP, new_rsp)?;
        self.vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RIP, ret_addr)?;

        Ok(true)
    }

    /// PUSH r64
    fn emulate_push_reg(
        &mut self,
        reg_num: u8,
        physmap: &PhysMap,
        _mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let val = self.read_gpr(reg_num)?;
        let new_rsp = self.rsp.wrapping_sub(8);
        self.write_mem(new_rsp, &val.to_le_bytes(), physmap)?;
        self.vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RSP, new_rsp)?;
        self.advance_rip()
    }

    /// POP r64
    fn emulate_pop_reg(
        &mut self,
        reg_num: u8,
        physmap: &PhysMap,
        _mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let mut buf = [0u8; 8];
        self.read_mem(self.rsp, &mut buf, physmap)?;
        let val = u64::from_le_bytes(buf);
        self.write_gpr(reg_num, val)?;
        let new_rsp = self.rsp.wrapping_add(8);
        self.vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RSP, new_rsp)?;
        self.advance_rip()
    }

    /// MOV r/m, r (0x89) or MOV r, r/m (0x8B)
    fn emulate_mov_modrm(
        &mut self,
        opcode: u8,
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let modrm = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };
        let size = self.operand_size();
        let reg = self.modrm_reg(modrm);
        let (addr, _) = match self.decode_modrm_addr(modrm) {
            Some(v) => v,
            None => return Ok(false),
        };

        if opcode == 0x89 {
            // MOV r/m, r — write register to memory
            let val = self.read_gpr(reg)?;
            let data = val.to_le_bytes();
            self.write_mem_or_mmio(addr, &data[..size], physmap, mmio_bus)?;
        } else {
            // MOV r, r/m — read memory to register
            let mut buf = [0u8; 8];
            self.read_mem_or_mmio(addr, &mut buf[..size], physmap, mmio_bus)?;
            let val = u64::from_le_bytes(buf);
            self.write_gpr(reg, val)?;
        }
        self.advance_rip()
    }

    /// MOV r/m8, r8 (0x88) or MOV r8, r/m8 (0x8A)
    fn emulate_mov_modrm_byte(
        &mut self,
        opcode: u8,
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let modrm = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };
        let reg = self.modrm_reg(modrm);
        let (addr, _) = match self.decode_modrm_addr(modrm) {
            Some(v) => v,
            None => return Ok(false),
        };

        if opcode == 0x88 {
            let val = self.read_gpr(reg)? as u8;
            self.write_mem_or_mmio(addr, &[val], physmap, mmio_bus)?;
        } else {
            let mut buf = [0u8; 1];
            self.read_mem_or_mmio(addr, &mut buf, physmap, mmio_bus)?;
            // Only update the low byte of the register
            let old = self.read_gpr(reg)?;
            let new = (old & !0xFF) | buf[0] as u64;
            self.write_gpr(reg, new)?;
        }
        self.advance_rip()
    }

    /// MOV r/m, imm32 (0xC7 /0)
    fn emulate_mov_imm(
        &mut self,
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let modrm = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };
        let size = self.operand_size();
        let (addr, _) = match self.decode_modrm_addr(modrm) {
            Some(v) => v,
            None => return Ok(false),
        };

        // Read immediate value
        let imm_size = if size == 8 { 4 } else { size }; // 64-bit mode still uses 32-bit sign-extended imm
        if self.pos + imm_size > self.bytes.len() {
            return Ok(false);
        }
        let mut imm_bytes = [0u8; 8];
        imm_bytes[..imm_size].copy_from_slice(&self.bytes[self.pos..self.pos + imm_size]);
        self.pos += imm_size;

        // Sign-extend if REX.W and imm is 32-bit
        if size == 8 {
            let val = i32::from_le_bytes([imm_bytes[0], imm_bytes[1], imm_bytes[2], imm_bytes[3]]);
            imm_bytes = (val as i64 as u64).to_le_bytes();
        }

        self.write_mem_or_mmio(addr, &imm_bytes[..size], physmap, mmio_bus)?;
        self.advance_rip()
    }

    /// MOV r/m8, imm8 (0xC6 /0)
    fn emulate_mov_imm8(
        &mut self,
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let modrm = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };
        let (addr, _) = match self.decode_modrm_addr(modrm) {
            Some(v) => v,
            None => return Ok(false),
        };
        let imm = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };
        self.write_mem_or_mmio(addr, &[imm], physmap, mmio_bus)?;
        self.advance_rip()
    }

    /// MOVZX r, r/m8 (0x0F 0xB6)
    fn emulate_movzx_byte(
        &mut self,
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let modrm = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };
        let reg = self.modrm_reg(modrm);
        let (addr, _) = match self.decode_modrm_addr(modrm) {
            Some(v) => v,
            None => return Ok(false),
        };
        let mut buf = [0u8; 1];
        self.read_mem_or_mmio(addr, &mut buf, physmap, mmio_bus)?;
        self.write_gpr(reg, buf[0] as u64)?;
        self.advance_rip()
    }

    /// MOVZX r, r/m16 (0x0F 0xB7)
    fn emulate_movzx_word(
        &mut self,
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<bool> {
        let modrm = match self.next_byte() {
            Some(b) => b,
            None => return Ok(false),
        };
        let reg = self.modrm_reg(modrm);
        let (addr, _) = match self.decode_modrm_addr(modrm) {
            Some(v) => v,
            None => return Ok(false),
        };
        let mut buf = [0u8; 2];
        self.read_mem_or_mmio(addr, &mut buf, physmap, mmio_bus)?;
        self.write_gpr(reg, u16::from_le_bytes(buf) as u64)?;
        self.advance_rip()
    }

    // ---------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------

    /// Extract the reg field from ModR/M byte, with REX.R extension.
    fn modrm_reg(&self, modrm: u8) -> u8 {
        let base = (modrm >> 3) & 0x07;
        if self.has_rex && (self.rex & 0x04) != 0 {
            base | 0x08 // REX.R extends reg field
        } else {
            base
        }
    }

    /// Decode ModR/M + SIB + displacement to get the effective address.
    /// Returns (address, is_register_operand).
    fn decode_modrm_addr(&mut self, modrm: u8) -> Option<(u64, bool)> {
        let mode = modrm >> 6;
        let rm = modrm & 0x07;
        let rm_ext = if self.has_rex && (self.rex & 0x01) != 0 {
            rm | 0x08
        } else {
            rm
        };

        match mode {
            0b11 => {
                // Register-to-register (no memory access)
                // Return the register value as "address"
                let val = self.read_gpr(rm_ext).ok()?;
                Some((val, true))
            }
            0b00 => {
                if rm == 0b100 {
                    // SIB byte follows
                    self.decode_sib(0)
                } else if rm == 0b101 {
                    // RIP-relative (disp32)
                    if self.pos + 4 > self.bytes.len() {
                        return None;
                    }
                    let disp = i32::from_le_bytes([
                        self.bytes[self.pos],
                        self.bytes[self.pos + 1],
                        self.bytes[self.pos + 2],
                        self.bytes[self.pos + 3],
                    ]);
                    self.pos += 4;
                    // RIP-relative: addr = RIP + inst_len + disp
                    // But inst_len isn't fully known yet... use current pos
                    let addr = self.rip
                        .wrapping_add(self.pos as u64)
                        .wrapping_add(disp as i64 as u64);
                    Some((addr, false))
                } else {
                    let base = self.read_gpr(rm_ext).ok()?;
                    Some((base, false))
                }
            }
            0b01 => {
                // [reg + disp8]
                if rm == 0b100 {
                    // SIB + disp8
                    let result = self.decode_sib(0)?;
                    if self.pos >= self.bytes.len() {
                        return None;
                    }
                    let disp = self.bytes[self.pos] as i8;
                    self.pos += 1;
                    Some((result.0.wrapping_add(disp as i64 as u64), false))
                } else {
                    if self.pos >= self.bytes.len() {
                        return None;
                    }
                    let disp = self.bytes[self.pos] as i8;
                    self.pos += 1;
                    let base = self.read_gpr(rm_ext).ok()?;
                    Some((base.wrapping_add(disp as i64 as u64), false))
                }
            }
            0b10 => {
                // [reg + disp32]
                if rm == 0b100 {
                    let result = self.decode_sib(0)?;
                    if self.pos + 4 > self.bytes.len() {
                        return None;
                    }
                    let disp = i32::from_le_bytes([
                        self.bytes[self.pos],
                        self.bytes[self.pos + 1],
                        self.bytes[self.pos + 2],
                        self.bytes[self.pos + 3],
                    ]);
                    self.pos += 4;
                    Some((result.0.wrapping_add(disp as i64 as u64), false))
                } else {
                    if self.pos + 4 > self.bytes.len() {
                        return None;
                    }
                    let disp = i32::from_le_bytes([
                        self.bytes[self.pos],
                        self.bytes[self.pos + 1],
                        self.bytes[self.pos + 2],
                        self.bytes[self.pos + 3],
                    ]);
                    self.pos += 4;
                    let base = self.read_gpr(rm_ext).ok()?;
                    Some((base.wrapping_add(disp as i64 as u64), false))
                }
            }
            _ => None,
        }
    }

    /// Decode SIB byte: base + (index * scale).
    fn decode_sib(&mut self, _disp_size: usize) -> Option<(u64, bool)> {
        let sib = self.next_byte()?;
        let scale = 1u64 << (sib >> 6);
        let index = (sib >> 3) & 0x07;
        let base = sib & 0x07;

        let index_ext = if self.has_rex && (self.rex & 0x02) != 0 {
            index | 0x08
        } else {
            index
        };
        let base_ext = if self.has_rex && (self.rex & 0x01) != 0 {
            base | 0x08
        } else {
            base
        };

        // Index of 4 (RSP) means no index
        let index_val = if index == 0x04 {
            0
        } else {
            self.read_gpr(index_ext).ok()? * scale
        };

        // Base of 5 with mod=00 means disp32 (no base register)
        let base_val = if base == 0x05 {
            // This depends on mod field (already consumed) — for mod=00,
            // base=5 means [disp32 + index*scale]. Read disp32.
            if self.pos + 4 > self.bytes.len() {
                return None;
            }
            let disp = i32::from_le_bytes([
                self.bytes[self.pos],
                self.bytes[self.pos + 1],
                self.bytes[self.pos + 2],
                self.bytes[self.pos + 3],
            ]);
            self.pos += 4;
            disp as i64 as u64
        } else {
            self.read_gpr(base_ext).ok()?
        };

        Some((base_val.wrapping_add(index_val), false))
    }

    /// Read a general-purpose register by number (0=RAX, 1=RCX, ...).
    fn read_gpr(&self, num: u8) -> Result<u64> {
        let reg = gpr_to_reg(num)?;
        self.vcpu.get_reg(reg)
    }

    /// Write a general-purpose register by number.
    fn write_gpr(&self, num: u8, val: u64) -> Result<()> {
        let reg = gpr_to_reg(num)?;
        self.vcpu.set_reg(reg, val)
    }

    /// Advance RIP past the current instruction.
    fn advance_rip(&self) -> Result<bool> {
        let new_rip = self.rip.wrapping_add(self.pos as u64);
        self.vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RIP, new_rip)?;
        Ok(true)
    }

    /// Write to guest memory (RAM or MMIO).
    fn write_mem_or_mmio(
        &self,
        addr: u64,
        data: &[u8],
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<()> {
        // Try RAM first
        if let Some(sub) = physmap.lookup(addr, data.len()) {
            return sub.write_bytes(data);
        }
        // Fall back to MMIO
        let val = match data.len() {
            1 => data[0] as u64,
            2 => u16::from_le_bytes([data[0], data[1]]) as u64,
            4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64,
            8 => u64::from_le_bytes(data.try_into().unwrap()),
            _ => 0,
        };
        mmio_bus.handle_write(addr, data.len() as u8, val);
        Ok(())
    }

    /// Read from guest memory (RAM or MMIO).
    fn read_mem_or_mmio(
        &self,
        addr: u64,
        buf: &mut [u8],
        physmap: &PhysMap,
        mmio_bus: &MmioBus,
    ) -> Result<()> {
        // Try RAM first
        if let Some(sub) = physmap.lookup(addr, buf.len()) {
            return sub.read_bytes(buf);
        }
        // Fall back to MMIO
        let val = mmio_bus.handle_read(addr, buf.len() as u8);
        let bytes = val.to_le_bytes();
        buf.copy_from_slice(&bytes[..buf.len()]);
        Ok(())
    }

    /// Write to guest memory (must be RAM — no MMIO fallback).
    fn write_mem(
        &self,
        addr: u64,
        data: &[u8],
        physmap: &PhysMap,
    ) -> Result<()> {
        if let Some(sub) = physmap.lookup(addr, data.len()) {
            sub.write_bytes(data)
        } else {
            Err(Error::new(
                ErrorKind::AddrNotAvailable,
                format!("write to unmapped GPA {:#x} len {}", addr, data.len()),
            ))
        }
    }

    /// Read from guest memory (must be RAM — no MMIO fallback).
    fn read_mem(
        &self,
        addr: u64,
        buf: &mut [u8],
        physmap: &PhysMap,
    ) -> Result<()> {
        if let Some(sub) = physmap.lookup(addr, buf.len()) {
            sub.read_bytes(buf)
        } else {
            Err(Error::new(
                ErrorKind::AddrNotAvailable,
                format!("read from unmapped GPA {:#x} len {}", addr, buf.len()),
            ))
        }
    }
}

/// Map GPR number to vm_reg_name.
/// x86-64 encoding: 0=RAX, 1=RCX, 2=RDX, 3=RBX, 4=RSP, 5=RBP,
/// 6=RSI, 7=RDI, 8=R8, ..., 15=R15
fn gpr_to_reg(num: u8) -> Result<vm_reg_name> {
    match num {
        0 => Ok(vm_reg_name::VM_REG_GUEST_RAX),
        1 => Ok(vm_reg_name::VM_REG_GUEST_RCX),
        2 => Ok(vm_reg_name::VM_REG_GUEST_RDX),
        3 => Ok(vm_reg_name::VM_REG_GUEST_RBX),
        4 => Ok(vm_reg_name::VM_REG_GUEST_RSP),
        5 => Ok(vm_reg_name::VM_REG_GUEST_RBP),
        6 => Ok(vm_reg_name::VM_REG_GUEST_RSI),
        7 => Ok(vm_reg_name::VM_REG_GUEST_RDI),
        8 => Ok(vm_reg_name::VM_REG_GUEST_R8),
        9 => Ok(vm_reg_name::VM_REG_GUEST_R9),
        10 => Ok(vm_reg_name::VM_REG_GUEST_R10),
        11 => Ok(vm_reg_name::VM_REG_GUEST_R11),
        12 => Ok(vm_reg_name::VM_REG_GUEST_R12),
        13 => Ok(vm_reg_name::VM_REG_GUEST_R13),
        14 => Ok(vm_reg_name::VM_REG_GUEST_R14),
        15 => Ok(vm_reg_name::VM_REG_GUEST_R15),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("invalid GPR number: {}", num),
        )),
    }
}
