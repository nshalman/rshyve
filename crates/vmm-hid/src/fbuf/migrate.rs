// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Framebuffer migration: the PCI header, the mode, and the VRAM.
//!
//! The VRAM is a devmem segment, not guest RAM, so the RAM passes do
//! not carry it. It goes with the device state, after the vCPUs pause.
//! VNC clients are not carried: they reconnect to the destination.

use std::sync::atomic::Ordering;

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, FbufMigrateState, FbufMigrateVram,
};
use vmm_devices::pci::device::replay_migrate_pci_state;

use super::{
    pack_resolution, resolution_fits, unpack_height, unpack_width, Framebuffer,
    BAR1_SIZE,
};

/// The largest compressed VRAM the device state carries.
///
/// The whole device state is one message of at most 8 MiB, and RON
/// writes these bytes as base64. A VRAM that does not compress to this
/// is left behind, and the destination shows black until the guest
/// draws again.
const VRAM_WIRE_CAP: usize = 2 * 1024 * 1024;

const PAGE: usize = 4096;

impl Framebuffer {
    /// Called with the vCPUs paused, so the VRAM does not change under
    /// the copy.
    pub(super) fn export_state(
        &self,
    ) -> Result<DeviceMigrateState, DeviceStateError> {
        let pci = self
            .pci_state
            .lock()
            .expect("fbuf: pci_state lock")
            .migrate_state();
        let res = self.resolution.load(Ordering::Acquire);
        let vram = self.export_vram()?;
        Ok(DeviceMigrateState::Fbuf(FbufMigrateState {
            pci,
            width: unpack_width(res),
            height: unpack_height(res),
            vram,
        }))
    }

    fn export_vram(&self) -> Result<Option<FbufMigrateVram>, DeviceStateError> {
        let mut buf = vec![0u8; self.fb.len()];
        self.fb.view().copy_out(&mut buf).map_err(|e| {
            DeviceStateError::Export(format!("fbuf: read VRAM: {e}"))
        })?;
        let used = used_len(&buf);
        let lz4 = lz4_flex::block::compress(&buf[..used]);
        if lz4.len() > VRAM_WIRE_CAP {
            slog::warn!(self.log,
                "fbuf: VRAM does not compress small enough, not carried";
                "used" => used, "compressed" => lz4.len());
            return Ok(None);
        }
        slog::info!(self.log, "fbuf: VRAM exported";
            "used" => used, "compressed" => lz4.len());
        Ok(Some(FbufMigrateVram {
            len: u32::try_from(used).expect("VRAM is 16 MiB"),
            lz4,
        }))
    }

    /// Called on a paused destination, before the guest runs.
    pub(super) fn restore_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::Fbuf(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "fbuf",
                got: state.kind(),
            });
        };
        if mig.width == 0
            || mig.height == 0
            || !resolution_fits(mig.width, mig.height)
        {
            return Err(DeviceStateError::invalid(format!(
                "fbuf: mode {}x{} does not fit the VRAM",
                mig.width, mig.height
            )));
        }
        let mut vram = match &mig.vram {
            Some(v) => decode_vram(v)?,
            None => {
                slog::warn!(
                    self.log,
                    "fbuf: the source carried no VRAM, the screen starts black"
                );
                Vec::new()
            }
        };
        // The whole segment, so an import into a used device leaves no
        // stale pixels past the carried prefix.
        vram.resize(self.fb.len(), 0);
        self.fb.view().write_bytes(&vram).map_err(|e| {
            DeviceStateError::invalid(format!("fbuf: write VRAM: {e}"))
        })?;
        self.resolution
            .store(pack_resolution(mig.width, mig.height), Ordering::Release);

        let bar_writes = self
            .pci_state
            .lock()
            .expect("fbuf: pci_state lock")
            .bar_replay_writes(&mig.pci.bar_addrs);
        replay_migrate_pci_state(self, &bar_writes, &mig.pci);
        Ok(())
    }
}

/// Bytes up to the last page with a nonzero byte.
fn used_len(buf: &[u8]) -> usize {
    let last = buf.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    last.div_ceil(PAGE).saturating_mul(PAGE).min(buf.len())
}

fn decode_vram(vram: &FbufMigrateVram) -> Result<Vec<u8>, DeviceStateError> {
    let len = vram.len as usize;
    if len > BAR1_SIZE as usize {
        return Err(DeviceStateError::invalid(format!(
            "fbuf: VRAM of {len} bytes is past the {BAR1_SIZE} the device has"
        )));
    }
    // The size bounds what a hostile frame can make us allocate.
    let out = lz4_flex::block::decompress(&vram.lz4, len).map_err(|e| {
        DeviceStateError::invalid(format!(
            "fbuf: VRAM does not decompress: {e}"
        ))
    })?;
    if out.len() != len {
        return Err(DeviceStateError::invalid(format!(
            "fbuf: VRAM is {} bytes, the header says {len}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::tests::make_test_dev;
    use super::*;
    use vmm_devices::pci::device::PciDevice;

    const BAR0: u64 = 0xE000_0000;
    const BAR1: u64 = 0xE100_0000;

    fn program(dev: &Framebuffer) {
        dev.cfg_write(0x10, 4, BAR0 as u32);
        dev.cfg_write(0x14, 4, BAR1 as u32);
        dev.cfg_write(
            0x04,
            2,
            u32::from(vmm_devices::pci::bits::RegCmd::MMIO_EN.bits()),
        );
    }

    fn ron_round_trip(state: &DeviceMigrateState) -> DeviceMigrateState {
        let text = ron::ser::to_string(state).expect("serialize");
        ron::de::from_str(&text).expect("deserialize")
    }

    #[test]
    fn state_and_vram_survive_the_wire() {
        let src = make_test_dev();
        program(&src);
        src.bar0_write(super::super::memreg::WIDTH, 1280 | (800 << 16), 4);
        let pixels: Vec<u8> = (0..3 * PAGE + 17).map(|i| i as u8 | 1).collect();
        src.fb.view().write_bytes(&pixels).expect("draw");

        let exported = src.export_state().expect("export");
        let DeviceMigrateState::Fbuf(mig) = &exported else {
            panic!("not fbuf state");
        };
        assert_eq!(mig.vram.as_ref().expect("vram").len as usize, 4 * PAGE);
        let text = ron::ser::to_string(&exported).expect("serialize");
        // Base64, not a list of numbers.
        assert!(text.len() < 4 * PAGE, "{} bytes of RON", text.len());

        let dst = make_test_dev();
        dst.fb.view().write_bytes(&[0xAA; 8 * PAGE]).expect("stale");
        dst.restore_state(&ron_round_trip(&exported))
            .expect("restore");

        assert_eq!((dst.width(), dst.height()), (1280, 800));
        let mut back = vec![0u8; 8 * PAGE];
        dst.fb.view().read_bytes(&mut back).expect("read");
        assert_eq!(&back[..pixels.len()], &pixels[..]);
        assert!(back[pixels.len()..].iter().all(|&b| b == 0));
        assert_eq!(dst.cfg_read(0x10, 4), BAR0 as u32);
        assert_eq!(dst.cfg_read(0x14, 4), BAR1 as u32);
        // BAR0 decodes again, so the guest reads its mode back.
        assert_eq!(
            dst.bus_mmio.handle_read(BAR0 + 4, 4),
            u64::from(1280u32 | (800 << 16))
        );
    }

    #[test]
    fn a_blank_vram_is_carried_as_nothing() {
        let src = make_test_dev();
        let DeviceMigrateState::Fbuf(mig) = src.export_state().expect("x")
        else {
            panic!("not fbuf state");
        };
        assert_eq!(mig.vram.expect("vram").len, 0);
    }

    #[test]
    fn a_missing_vram_still_restores_the_mode() {
        let dst = make_test_dev();
        let state = DeviceMigrateState::Fbuf(FbufMigrateState {
            width: 800,
            height: 600,
            vram: None,
            ..Default::default()
        });
        dst.restore_state(&state).expect("restore");
        assert_eq!((dst.width(), dst.height()), (800, 600));
    }

    #[test]
    fn a_hostile_state_is_refused() {
        let dst = make_test_dev();
        let bad_mode = DeviceMigrateState::Fbuf(FbufMigrateState {
            width: 4096,
            height: 4096,
            ..Default::default()
        });
        assert!(dst.restore_state(&bad_mode).is_err());

        let big = lz4_flex::block::compress(&vec![0u8; 2 * PAGE]);
        for vram in [
            FbufMigrateVram {
                len: BAR1_SIZE + 1,
                lz4: big.clone(),
            },
            // The header says less than the frame holds.
            FbufMigrateVram {
                len: PAGE as u32,
                lz4: big,
            },
            FbufMigrateVram {
                len: 16,
                lz4: vec![1, 2, 3],
            },
        ] {
            let state = DeviceMigrateState::Fbuf(FbufMigrateState {
                width: 640,
                height: 480,
                vram: Some(vram),
                ..Default::default()
            });
            assert!(dst.restore_state(&state).is_err());
        }
        // Nothing of the refused states was applied.
        assert_eq!((dst.width(), dst.height()), (1024, 768));
    }

    #[test]
    fn used_len_rounds_up_to_a_page() {
        let mut buf = vec![0u8; 4 * PAGE];
        assert_eq!(used_len(&buf), 0);
        buf[PAGE] = 1;
        assert_eq!(used_len(&buf), 2 * PAGE);
        buf[4 * PAGE - 1] = 1;
        assert_eq!(used_len(&buf), 4 * PAGE);
    }
}
