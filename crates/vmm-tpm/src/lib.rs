// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Virtual TPM 2.0 device for vmmnew.
//!
//! Backend: vendored libtpms (TCG-compliant TPM 2.0 emulator) running
//! in-process via the `vmm_tpm_sys` FFI.
//! Frontend: TCG PTP 2.0 Command Response Buffer (CRB) over MMIO at
//! 0xFED40000, advertised via a TPM2 ACPI table.
//!
//! End-to-end: a Windows 11 / Server 2022 (or modern Linux) guest
//! parses the TPM2 ACPI table, finds the Control Area, drives the
//! CRB, and gets responses backed by libtpms with NV state persisted
//! to a per-VM directory on the host filesystem.

pub mod acpi;
mod callbacks;
pub mod crb;

use std::path::PathBuf;
use std::sync::Arc;

use slog::{info, Logger};

pub use crate::acpi::build_tpm2_table;
pub use crate::crb::{Crb, CRB_BASE, CRB_REGION_LEN};

use vmm_tpm_sys::{
    TPMLIB_ChooseTPMVersion, TPMLIB_MainInit,
    TPMLIB_TPMVersion, TPM_SUCCESS,
};

/// Top-level TPM device. Owns the CRB frontend; libtpms backend is
/// initialized once at construction time (process-global state).
pub struct Tpm {
    pub crb: Arc<Crb>,
}

impl Tpm {
    /// Set up libtpms (TPM 2.0, file-backed NV in `state_dir`) and
    /// instantiate the CRB frontend. Call exactly once per process —
    /// libtpms is single-instance.
    ///
    /// `state_dir` will be created if it does not exist.
    pub fn new(state_dir: PathBuf, log: Logger) -> Result<Self, TpmError> {
        callbacks::register(state_dir.clone(), log.clone())
            .map_err(TpmError::CallbackInit)?;

        let rc = unsafe {
            TPMLIB_ChooseTPMVersion(TPMLIB_TPMVersion::TPMLIB_TPM_VERSION_2)
        };
        if rc != TPM_SUCCESS {
            return Err(TpmError::Libtpms("ChooseTPMVersion", rc));
        }
        let rc = unsafe { TPMLIB_MainInit() };
        if rc != TPM_SUCCESS {
            return Err(TpmError::Libtpms("MainInit", rc));
        }
        info!(log, "vTPM 2.0 ready"; "state_dir" => %state_dir.display());

        Ok(Self {
            crb: Arc::new(Crb::new(log)),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TpmError {
    #[error("vTPM callback registration failed: {0}")]
    CallbackInit(#[source] std::io::Error),
    #[error("libtpms {0} returned rc={1:#x}")]
    Libtpms(&'static str, u32),
}
