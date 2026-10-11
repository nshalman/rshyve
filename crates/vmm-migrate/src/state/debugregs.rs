// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checks on the debug registers a source sends.
//!
//! The host kernel loads DR0-DR3 and DR6 with `mov` before it enters
//! the guest, and DR7 goes through the VMCS or the VMCB. A set bit in
//! 63:32 of DR6 or DR7 is a #GP in the host on that `mov`, or a failed
//! entry, so it is refused. DR0-DR3 take any 64-bit value, canonical or
//! not, so a guest can hold one that is not and still migrate. The
//! reserved bits in 31:0 cannot fault; they are set to the values the
//! Intel SDM (vol. 3, 18.2) and the AMD APM (vol. 2, 13.1) give.

use bhyve_api::vm_reg_name;

use super::checks::RegWrite;
use crate::codec::MigrateError;

/// DR6 bits 4-10 and 17-31 read as 1. Bits 11 (BLD) and 16 (RTM) are
/// active-low status bits, so either value is real.
const DR6_ONES: u64 = 0xFFFE_07F0;
/// DR6 bit 12 reads as 0.
const DR6_ZEROS: u64 = 1 << 12;
/// DR7 bit 10 reads as 1.
const DR7_ONES: u64 = 1 << 10;
/// DR7 bits 12, 14 and 15 read as 0 on both vendors.
const DR7_ZEROS: u64 = (1 << 12) | (1 << 14) | (1 << 15);
/// DR7 bit 11 is RTM on Intel and reserved on AMD.
const DR7_RTM: u64 = 1 << 11;

/// Refuse debug register values that fault in the host.
pub(super) fn check_debug_regs(
    vcpuid: i32,
    writes: &[RegWrite],
) -> Result<(), MigrateError> {
    for write in writes {
        let RegWrite::Reg(reg, value) = *write else {
            continue;
        };
        let bad = matches!(
            reg,
            vm_reg_name::VM_REG_GUEST_DR6 | vm_reg_name::VM_REG_GUEST_DR7
        ) && value >> 32 != 0;
        if bad {
            return Err(MigrateError::State(format!(
                "vcpu {vcpuid}: {reg:?} {value:#x} is not a value the \
                 host can load"
            )));
        }
    }
    Ok(())
}

/// Set the reserved low bits of DR6 and DR7 to their architectural
/// values. Every other write passes through.
pub(super) fn normalize(write: RegWrite, amd_host: bool) -> RegWrite {
    match write {
        RegWrite::Reg(vm_reg_name::VM_REG_GUEST_DR6, v) => RegWrite::Reg(
            vm_reg_name::VM_REG_GUEST_DR6,
            (v | DR6_ONES) & !DR6_ZEROS,
        ),
        RegWrite::Reg(vm_reg_name::VM_REG_GUEST_DR7, v) => {
            let zeros = if amd_host {
                DR7_ZEROS | DR7_RTM
            } else {
                DR7_ZEROS
            };
            RegWrite::Reg(
                vm_reg_name::VM_REG_GUEST_DR7,
                (v | DR7_ONES) & !zeros,
            )
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(reg: vm_reg_name, value: u64) -> RegWrite {
        RegWrite::Reg(reg, value)
    }

    fn value(write: RegWrite) -> u64 {
        match write {
            RegWrite::Reg(_, v) => v,
            RegWrite::Seg(..) => panic!("not a register"),
        }
    }

    #[test]
    fn hostile_high_bits_in_dr6_and_dr7_are_refused() {
        for (r, v) in [
            (vm_reg_name::VM_REG_GUEST_DR6, 1u64 << 32),
            (vm_reg_name::VM_REG_GUEST_DR6, u64::MAX),
            (vm_reg_name::VM_REG_GUEST_DR7, 0xFFFF_FFFF_0000_0400),
            (vm_reg_name::VM_REG_GUEST_DR7, 1 << 63),
        ] {
            let error = check_debug_regs(0, &[reg(r, v)])
                .expect_err("a value that faults in the host");
            assert!(error.to_string().contains("host can load"), "{error}");
        }
    }

    #[test]
    fn any_breakpoint_address_is_taken() {
        // MOV to DR0-DR3 does not check for a canonical address.
        for v in [0xFFFF_8000_0000_1000, 0x0000_8000_0000_0000, u64::MAX] {
            for r in
                [vm_reg_name::VM_REG_GUEST_DR0, vm_reg_name::VM_REG_GUEST_DR3]
            {
                check_debug_regs(0, &[reg(r, v)]).expect("any address");
            }
        }
    }

    #[test]
    fn the_reserved_low_bits_get_their_architectural_values() {
        let dr6 = vm_reg_name::VM_REG_GUEST_DR6;
        let dr7 = vm_reg_name::VM_REG_GUEST_DR7;
        // The reset values pass unchanged.
        assert_eq!(value(normalize(reg(dr6, 0xFFFF_0FF0), false)), 0xFFFF_0FF0);
        assert_eq!(value(normalize(reg(dr7, 0x400), true)), 0x400);
        assert_eq!(value(normalize(reg(dr6, 0), false)), 0xFFFE_07F0);
        assert_eq!(value(normalize(reg(dr6, 0x1000), false)), 0xFFFE_07F0);
        assert_eq!(value(normalize(reg(dr7, 0xFFFF_FFFF), false)), 0xFFFF_2FFF);
        assert_eq!(value(normalize(reg(dr7, 0xFFFF_FFFF), true)), 0xFFFF_27FF);
        let rax = reg(vm_reg_name::VM_REG_GUEST_RAX, 0x1000);
        assert_eq!(value(normalize(rax, true)), 0x1000);
    }
}
