// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Raw FFI bindings for the vendored libtpms (TPM 2.0).
//!
//! Exposes only the entry points vmm-tpm needs. We do not use bindgen
//! because the libtpms public API is small and stable (the
//! `TPMLIB_*` family in `libtpms/tpm_library.h`).
//!
//! The shared-library version of libtpms uses TLS for some of its
//! internal command processing state, which means the same library
//! instance cannot serve more than one VM in the same process. We
//! always link statically and instantiate per-process; if vmmnew
//! ever runs multiple VMs in one process, this module needs revisiting.

#![allow(non_camel_case_types, non_snake_case)]

use std::os::raw::{c_char, c_uint, c_void};

// `build.rs` includes this file rather than importing it, so its tests
// only run from here.
#[cfg(test)]
mod platform_flags;

/// `TPM_RESULT` from libtpms: 32-bit response code. 0 == success.
pub type TPM_RESULT = u32;

pub const TPM_SUCCESS: TPM_RESULT = 0;

/// TPM library version selector for `TPMLIB_ChooseTPMVersion`.
///
/// Mirrors the C enum in `tpm_library.h.in` exactly — the
/// discriminants are 0 and 1 (default C enum numbering), not 1 and 2.
/// Getting this wrong yields `TPMLIB_ChooseTPMVersion → 9` (bad parameter)
/// because the value never matches the library's switch.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum TPMLIB_TPMVersion {
    TPMLIB_TPM_VERSION_1_2 = 0,
    TPMLIB_TPM_VERSION_2 = 1,
}

extern "C" {
    /// Select TPM 1.2 vs TPM 2.0. Must be called before MainInit.
    pub fn TPMLIB_ChooseTPMVersion(version: TPMLIB_TPMVersion) -> TPM_RESULT;

    /// Initialize the TPM library state. After this returns success,
    /// the library is ready to accept commands via `TPMLIB_Process`.
    pub fn TPMLIB_MainInit() -> TPM_RESULT;

    /// Tear down library state and free resources.
    pub fn TPMLIB_Terminate();

    /// Process a TPM command. `command`/`command_size` are the input
    /// bytes (TPM 2.0 framed command). On success, `*resp` is set to a
    /// libtpms-allocated buffer (free with libc `free`), `*resp_size`
    /// to the bytes written, and `*resp_bufsize` reflects the buffer
    /// capacity the library used.
    pub fn TPMLIB_Process(
        resp: *mut *mut u8,
        resp_size: *mut u32,
        resp_bufsize: *mut u32,
        command: *const u8,
        command_size: u32,
    ) -> TPM_RESULT;

    /// Apply runtime profile (locality, established flag, …) before
    /// MainInit. Pass NULL for default behavior.
    pub fn TPMLIB_SetProfile(profile: *const c_char) -> TPM_RESULT;

    /// Register host-side callbacks: NV storage, locality changes, etc.
    /// `cbs` is a pointer to `libtpms_callbacks` (see libtpms.h).
    pub fn TPMLIB_RegisterCallbacks(cbs: *mut c_void) -> TPM_RESULT;

    /// Get the current TPM state blob (for migration / persistence).
    /// `state_type` selects which blob (PERMANENT/VOLATILE/SAVESTATE).
    pub fn TPMLIB_GetState(
        state_type: c_uint,
        buffer: *mut *mut u8,
        buffer_size: *mut u32,
    ) -> TPM_RESULT;

    /// Restore a previously-saved state blob.
    pub fn TPMLIB_SetState(
        state_type: c_uint,
        buffer: *const u8,
        buffer_size: u32,
    ) -> TPM_RESULT;
}

/// State blob kinds for `TPMLIB_GetState` / `TPMLIB_SetState`.
/// See libtpms `TPMLIB_StateType` enum.
pub const TPMLIB_STATE_PERMANENT: c_uint = 0x01;
pub const TPMLIB_STATE_VOLATILE: c_uint = 0x02;
pub const TPMLIB_STATE_SAVE_STATE: c_uint = 0x04;

/// Free a libtpms-allocated buffer (response from `TPMLIB_Process`,
/// state blob from `TPMLIB_GetState`, etc.). libtpms uses the system
/// allocator, so this is just a thin wrapper around libc `free`.
///
/// # Safety
/// `buf` must be either null or a pointer libtpms returned to us.
pub unsafe fn tpmlib_free(buf: *mut c_void) {
    if !buf.is_null() {
        unsafe { libc::free(buf) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: choose TPM 2.0, init, run TPM2_GetRandom for 4 bytes,
    /// terminate. Verifies the entire FFI / build chain is intact.
    #[test]
    fn smoke_get_random() {
        let home = nv_directory();
        let start = std::env::current_dir().expect("read the directory");
        std::env::set_current_dir(&home).expect("enter the NV directory");

        unsafe {
            assert_eq!(
                TPMLIB_ChooseTPMVersion(
                    TPMLIB_TPMVersion::TPMLIB_TPM_VERSION_2
                ),
                TPM_SUCCESS,
                "ChooseTPMVersion"
            );
            // With no callbacks registered, TPMLIB_MainInit uses the
            // libtpms platform layer, which keeps NV in the `NVChip`
            // file this test runs beside.
            assert_eq!(TPMLIB_MainInit(), TPM_SUCCESS, "MainInit");

            // TPM2_Startup(TPM_SU_CLEAR) — every TPM needs this before
            // accepting commands. 12-byte command:
            //   tag=8001 size=0000000C cc=00000144 startup=0000
            let startup: [u8; 12] = [
                0x80, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x44,
                0x00, 0x00,
            ];
            let mut resp: *mut u8 = std::ptr::null_mut();
            let mut resp_size: u32 = 0;
            let mut resp_buf: u32 = 4096;
            let r = TPMLIB_Process(
                &mut resp,
                &mut resp_size,
                &mut resp_buf,
                startup.as_ptr(),
                startup.len() as u32,
            );
            assert_eq!(r, TPM_SUCCESS, "Startup process");
            assert!(!resp.is_null() && resp_size >= 10);
            tpmlib_free(resp as *mut _);

            // TPM2_GetRandom(4) — 12-byte command:
            //   tag=8001 size=0000000C cc=0000017B bytes=0004
            let cmd: [u8; 12] = [
                0x80, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x7B,
                0x00, 0x04,
            ];
            let mut resp: *mut u8 = std::ptr::null_mut();
            let mut resp_size: u32 = 0;
            let mut resp_buf: u32 = 4096;
            let r = TPMLIB_Process(
                &mut resp,
                &mut resp_size,
                &mut resp_buf,
                cmd.as_ptr(),
                cmd.len() as u32,
            );
            assert_eq!(r, TPM_SUCCESS, "GetRandom process");
            assert!(!resp.is_null());
            // Response: tag(2) size(4) rc(4) randomSize(2) random(4) = 16 bytes
            assert!(resp_size >= 16, "short response: {resp_size}");
            let bytes = std::slice::from_raw_parts(resp, resp_size as usize);
            // Bytes 6..10 == response code, must be 0
            assert_eq!(&bytes[6..10], &[0, 0, 0, 0], "non-success rc");
            tpmlib_free(resp as *mut _);

            TPMLIB_Terminate();
        }

        std::env::set_current_dir(&start).expect("leave the NV directory");
        std::fs::remove_dir_all(&home).expect("remove the NV directory");
    }

    /// A private directory to run libtpms in.
    ///
    /// The TPM 2 platform layer opens its NV file, `NVChip`, by a
    /// relative path. Left alone it writes 176 KiB into the source
    /// tree, and the next run reads that file back and so never
    /// exercises first boot.
    ///
    /// The current directory belongs to the process, not to one test,
    /// but every other test in this binary is a pure function over
    /// strings.
    fn nv_directory() -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir()
            .join(format!("vmm-tpm-sys-smoke-{pid}-{unique}"));
        // create_dir, not create_dir_all: it fails when anything
        // already holds the name, so a symlink planted in a shared
        // /tmp cannot redirect the NV file.
        std::fs::create_dir(&dir).expect("make a private NV directory");
        dir
    }
}
