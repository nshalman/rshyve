// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The framebuffer VRAM forms on the wire.
//!
//! v8 to v10 builds sent the VRAM as zstd or as LZ4 under one version
//! string (see [`FbufMigrateVram`]). The device makes LZ4 only. Here a
//! source adds the zstd form for a peer before v12, and a destination
//! turns a zstd-only VRAM into LZ4 before the device sees it.

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStatePayload, FbufMigrateVram,
};

use crate::codec::MigrateError;
use crate::limits::ZSTD_LEVEL;
use crate::protocol::{at_least, PROTOCOL_RON_V12};

/// Add the zstd form to each VRAM when `protocol` is older than v12.
///
/// A VRAM whose zstd form is over the cap is left behind, as the
/// device does with LZ4: the destination shows black until the guest
/// draws again.
pub(crate) fn fit_for_peer(
    protocol: &str,
    emulated: &mut [DeviceStatePayload],
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    if at_least(protocol, PROTOCOL_RON_V12) {
        return Ok(());
    }
    for vram in vrams(emulated) {
        let Some(v) = vram.as_mut() else { continue };
        let raw = lz4_decompress(v)?;
        let zstd = zstd::bulk::compress(&raw, ZSTD_LEVEL).map_err(|e| {
            MigrateError::State(format!("fbuf: compress VRAM: {e}"))
        })?;
        if zstd.len() > FbufMigrateVram::WIRE_CAP {
            slog::warn!(log,
                "fbuf: VRAM does not compress small enough for an older \
                 peer, not carried";
                "used" => v.len, "compressed" => zstd.len());
            *vram = None;
            continue;
        }
        v.zstd = zstd;
    }
    Ok(())
}

/// Leave each VRAM with only its LZ4 form, from either form the source
/// sent.
pub(crate) fn accept(
    emulated: &mut [DeviceStatePayload],
) -> Result<(), MigrateError> {
    for v in vrams(emulated).filter_map(Option::as_mut) {
        let zstd = std::mem::take(&mut v.zstd);
        if !v.lz4.is_empty() || zstd.is_empty() {
            continue;
        }
        let len = checked_len(v)?;
        // The capacity bounds what a hostile frame can make us allocate.
        let raw = zstd::bulk::decompress(&zstd, len).map_err(|e| {
            MigrateError::State(format!("fbuf: VRAM does not decompress: {e}"))
        })?;
        if raw.len() != len {
            return Err(MigrateError::State(format!(
                "fbuf: VRAM is {} bytes, the header says {len}",
                raw.len()
            )));
        }
        v.lz4 = lz4_flex::block::compress(&raw);
    }
    Ok(())
}

fn vrams(
    emulated: &mut [DeviceStatePayload],
) -> impl Iterator<Item = &mut Option<FbufMigrateVram>> {
    emulated.iter_mut().filter_map(|p| match &mut p.state {
        Some(DeviceMigrateState::Fbuf(fbuf)) => Some(&mut fbuf.vram),
        _ => None,
    })
}

fn checked_len(v: &FbufMigrateVram) -> Result<usize, MigrateError> {
    if v.len > FbufMigrateVram::MAX_LEN {
        return Err(MigrateError::State(format!(
            "fbuf: VRAM of {} bytes is past the {} the device has",
            v.len,
            FbufMigrateVram::MAX_LEN
        )));
    }
    Ok(v.len as usize)
}

fn lz4_decompress(v: &FbufMigrateVram) -> Result<Vec<u8>, MigrateError> {
    lz4_flex::block::decompress(&v.lz4, checked_len(v)?).map_err(|e| {
        MigrateError::State(format!("fbuf: VRAM does not decompress: {e}"))
    })
}

#[cfg(test)]
mod tests;
