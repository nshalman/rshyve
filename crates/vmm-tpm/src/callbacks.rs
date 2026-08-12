// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! libtpms callback shim: file-backed NV storage + locality reporting.
//!
//! libtpms calls into the host whenever it needs to read or write the
//! TPM's non-volatile state (PERMANENT, VOLATILE, SAVESTATE blobs)
//! and to ask the active locality for an incoming command. We map
//! each NV blob to a file in a per-VM state directory.
//!
//! The callbacks are C function pointers, registered once at TPM
//! init via `TPMLIB_RegisterCallbacks`. libtpms invokes them
//! synchronously from inside `TPMLIB_Process` on whatever thread
//! issued the command (the vCPU thread, via the CRB MMIO handler).
//!
//! libtpms is a single-instance library — one vmm process serves
//! one VM, so this is fine. Per-VM state lives behind a `OnceLock`.

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Write};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use slog::{debug, warn, Logger};
use vmm_tpm_sys::TPM_RESULT;

/// libtpms callback table layout. Mirrors `struct libtpms_callbacks`
/// in `tpm_library.h.in`. Order and field count must match exactly;
/// the library tells us the size it expects via `sizeOfStruct` so we
/// fill that on the way in.
#[repr(C)]
pub struct LibtpmsCallbacks {
    pub size_of_struct: i32,
    pub tpm_nvram_init: Option<unsafe extern "C" fn() -> TPM_RESULT>,
    pub tpm_nvram_loaddata: Option<
        unsafe extern "C" fn(
            data: *mut *mut u8,
            length: *mut u32,
            tpm_number: u32,
            name: *const c_char,
        ) -> TPM_RESULT,
    >,
    pub tpm_nvram_storedata: Option<
        unsafe extern "C" fn(
            data: *const u8,
            length: u32,
            tpm_number: u32,
            name: *const c_char,
        ) -> TPM_RESULT,
    >,
    pub tpm_nvram_deletename: Option<
        unsafe extern "C" fn(
            tpm_number: u32,
            name: *const c_char,
            must_exist: u8, // TPM_BOOL
        ) -> TPM_RESULT,
    >,
    pub tpm_io_init: Option<unsafe extern "C" fn() -> TPM_RESULT>,
    pub tpm_io_getlocality: Option<
        unsafe extern "C" fn(
            locality: *mut u32,
            tpm_number: u32,
        ) -> TPM_RESULT,
    >,
    pub tpm_io_getphysicalpresence: Option<
        unsafe extern "C" fn(
            present: *mut u8,
            tpm_number: u32,
        ) -> TPM_RESULT,
    >,
}

/// libtpms response codes — only the few we care about.
const TPM_FAIL: TPM_RESULT = 9;
/// libtpms uses `TPM_RETRY` from `tpm_nvram_loaddata` to signal
/// "no stored blob; first-time startup". The library responds by
/// manufacturing fresh state on the next `MainInit`. Any other
/// non-zero code is treated as a hard failure and aborts startup.
const TPM_RETRY: TPM_RESULT = 0x800;

extern "C" {
    fn TPMLIB_RegisterCallbacks(cbs: *mut LibtpmsCallbacks) -> TPM_RESULT;
}

/// Per-process callback state. Set once during TPM device construction.
struct CallbackState {
    state_dir: PathBuf,
    /// Currently-active locality (0-4). The CRB frontend updates this
    /// when the guest acquires/releases a locality. libtpms reads it
    /// via `tpm_io_getlocality` for every command.
    locality: AtomicU32,
    log: Logger,
}

static STATE: OnceLock<CallbackState> = OnceLock::new();

/// Install our callback table in libtpms. Call once before
/// `TPMLIB_MainInit`. Returns an error if libtpms rejects the table
/// (usually a struct-size mismatch — verify `LibtpmsCallbacks` has
/// the same field count and layout as the C declaration).
pub fn register(state_dir: PathBuf, log: Logger) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(&state_dir).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "creating TPM state dir {}: {}",
                state_dir.display(),
                e
            ),
        )
    })?;

    STATE
        .set(CallbackState {
            state_dir,
            locality: AtomicU32::new(0),
            log,
        })
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "TPM callbacks already registered",
            )
        })?;

    let mut cbs = LibtpmsCallbacks {
        size_of_struct: std::mem::size_of::<LibtpmsCallbacks>() as i32,
        tpm_nvram_init: Some(cb_nvram_init),
        tpm_nvram_loaddata: Some(cb_nvram_loaddata),
        tpm_nvram_storedata: Some(cb_nvram_storedata),
        tpm_nvram_deletename: Some(cb_nvram_deletename),
        tpm_io_init: Some(cb_io_init),
        tpm_io_getlocality: Some(cb_io_getlocality),
        tpm_io_getphysicalpresence: Some(cb_io_getphysicalpresence),
    };

    let rc = unsafe { TPMLIB_RegisterCallbacks(&mut cbs) };
    if rc != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("TPMLIB_RegisterCallbacks failed: {:#x}", rc),
        ));
    }
    Ok(())
}

/// Update the active locality. Called by the CRB frontend when the
/// guest writes the LOC_CTRL register. libtpms reads this back via
/// `tpm_io_getlocality` on every command.
pub fn set_locality(locality: u8) {
    if let Some(s) = STATE.get() {
        s.locality.store(locality as u32, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------
// Callback implementations
// ---------------------------------------------------------------

unsafe extern "C" fn cb_nvram_init() -> TPM_RESULT {
    // The state dir was created in `register`; nothing more to do.
    0
}

unsafe extern "C" fn cb_io_init() -> TPM_RESULT {
    0
}

unsafe extern "C" fn cb_io_getphysicalpresence(
    present: *mut u8,
    _tpm_number: u32,
) -> TPM_RESULT {
    // Cloud VMs have no physical presence button. Always false.
    if !present.is_null() {
        unsafe { *present = 0 };
    }
    0
}

unsafe extern "C" fn cb_io_getlocality(
    locality: *mut u32,
    _tpm_number: u32,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    if !locality.is_null() {
        unsafe { *locality = s.locality.load(Ordering::SeqCst) };
    }
    0
}

unsafe extern "C" fn cb_nvram_loaddata(
    data: *mut *mut u8,
    length: *mut u32,
    _tpm_number: u32,
    name: *const c_char,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    let path = match nv_path(s, name) {
        Some(p) => p,
        None => return TPM_FAIL,
    };

    match std::fs::read(&path) {
        Ok(buf) => {
            // libtpms takes ownership; allocate via libc malloc so it
            // can free with libc free. Vec's allocator is not
            // necessarily compatible with libc free.
            let len = buf.len();
            if len == 0 {
                debug!(s.log, "tpm nv loaddata: empty file"; "path" => %path.display());
                unsafe {
                    *data = std::ptr::null_mut();
                    *length = 0;
                }
                return TPM_RETRY;
            }
            let p = unsafe { libc::malloc(len) } as *mut u8;
            if p.is_null() {
                return TPM_FAIL;
            }
            unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), p, len) };
            unsafe {
                *data = p;
                *length = len as u32;
            }
            debug!(s.log, "tpm nv loaddata"; "name" => display_name(name), "bytes" => len);
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(s.log, "tpm nv loaddata: no file (fresh state)"; "name" => display_name(name));
            // Tell libtpms: nothing stored. Library manufactures
            // fresh TPM state on the next MainInit.
            unsafe {
                *data = std::ptr::null_mut();
                *length = 0;
            }
            TPM_RETRY
        }
        Err(e) => {
            warn!(s.log, "tpm nv loaddata failed";
                "path" => %path.display(),
                "error" => %e
            );
            TPM_FAIL
        }
    }
}

unsafe extern "C" fn cb_nvram_storedata(
    data: *const u8,
    length: u32,
    _tpm_number: u32,
    name: *const c_char,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    let path = match nv_path(s, name) {
        Some(p) => p,
        None => return TPM_FAIL,
    };

    let bytes = if data.is_null() || length == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, length as usize) }
    };

    // Rename prevents partial blobs, but file and directory syncs are needed
    // to survive host failure. Lost TPM NV state can make a BitLocker VMK
    // unrecoverable.
    if let Err(e) = durable_replace(&path, bytes) {
        warn!(s.log, "tpm nv storedata failed";
            "path" => %path.display(),
            "error" => %e
        );
        return TPM_FAIL;
    }
    debug!(s.log, "tpm nv storedata"; "name" => display_name(name), "bytes" => length);
    0
}

unsafe extern "C" fn cb_nvram_deletename(
    _tpm_number: u32,
    name: *const c_char,
    must_exist: u8,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    let path = match nv_path(s, name) {
        Some(p) => p,
        None => return TPM_FAIL,
    };
    match std::fs::remove_file(&path) {
        Ok(()) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // libtpms passes mustExist when it expects the file to be
            // there; a missing file then is a real error. Otherwise
            // not finding the file is fine.
            if must_exist != 0 {
                TPM_FAIL
            } else {
                0
            }
        }
        Err(e) => {
            warn!(s.log, "tpm nv deletename failed";
                "path" => %path.display(),
                "error" => %e
            );
            TPM_FAIL
        }
    }
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn durable_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("creating TPM NV temp file {}: {e}", tmp.display()),
        )
    })?;
    file.write_all(bytes).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("writing TPM NV temp file {}: {e}", tmp.display()),
        )
    })?;
    file.sync_all().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("syncing TPM NV temp file {}: {e}", tmp.display()),
        )
    })?;
    drop(file);

    std::fs::rename(&tmp, path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "renaming TPM NV temp file {} to {}: {e}",
                tmp.display(),
                path.display(),
            ),
        )
    })?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = File::open(parent).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("opening TPM NV state directory {}: {e}", parent.display()),
        )
    })?;
    directory.sync_all().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("syncing TPM NV state directory {}: {e}", parent.display()),
        )
    })
}

fn nv_path(s: &CallbackState, name: *const c_char) -> Option<PathBuf> {
    if name.is_null() {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(name) };
    let n = cstr.to_str().ok()?;
    // libtpms uses names like "tpm2-00.permall", "tpm2-00.volatilestate",
    // "tpm2-00.savestate". Use the name verbatim as the file basename.
    Some(s.state_dir.join(n))
}

fn display_name(name: *const c_char) -> String {
    if name.is_null() {
        return "<null>".into();
    }
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

// Compile-time assertion: callback struct size matches the C layout
// libtpms expects. C struct = 1 int + 7 function pointers, with the
// int padded to 8 bytes, total 64 bytes on x86-64. Mismatch here
// silently corrupts callbacks at runtime, so make it a build error.
const _: () = assert!(
    std::mem::size_of::<LibtpmsCallbacks>() == 64,
    "LibtpmsCallbacks must be exactly 64 bytes on 64-bit targets — \
     check field count and order against libtpms tpm_library.h"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        fn new() -> Self {
            let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmm-tpm-callbacks-test-{}-{sequence}",
                std::process::id(),
            ));
            std::fs::create_dir(&path).expect("create scratch directory");
            Self { path }
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    #[test]
    fn durable_replace_writes_and_replaces_contents() {
        let scratch = ScratchDir::new();
        let path = scratch.path.join("tpm2-00.permall");

        // This exercises both sync calls and verifies replacement completes
        // without a leftover temp file. It cannot prove observation atomicity
        // or persistence across a sudden host power loss.
        durable_replace(&path, b"first state").expect("write initial state");
        assert_eq!(
            std::fs::read(&path).expect("read initial state"),
            b"first state"
        );

        durable_replace(&path, b"replacement state").expect("replace state");
        assert_eq!(
            std::fs::read(&path).expect("read replacement state"),
            b"replacement state",
        );
        assert!(!path.with_extension("tmp").exists());
    }
}
