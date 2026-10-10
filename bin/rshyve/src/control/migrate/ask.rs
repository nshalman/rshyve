// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the migration requests ask for, checked before anything runs.

use std::path::PathBuf;
use std::time::Duration;

/// An absolute path for a socket the agent listens on.
pub(super) fn agent_socket(path: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(path);
    if !p.is_absolute() || path.contains('\0') {
        return Err(format!("'{path}' is not an absolute socket path"));
    }
    Ok(p)
}

/// The gate's wait, from the request. Refused rather than cut down, so
/// an orchestrator learns its timeout was not used.
pub(super) fn gate_budget(ms: Option<u64>) -> Result<Duration, String> {
    let Some(ms) = ms else {
        return Ok(vmm_migrate::gate::DEFAULT_BUDGET);
    };
    let budget = Duration::from_millis(ms);
    if budget.is_zero() || budget > vmm_migrate::gate::MAX_BUDGET {
        return Err(format!(
            "gate_timeout_ms must be 1 to {}",
            vmm_migrate::gate::MAX_BUDGET.as_millis()
        ));
    }
    Ok(budget)
}

/// How long a disk may take to attach before a guest runs again. No
/// longer than the source waits for the start.
pub(super) const RING_ATTACH_DEFAULT: Duration =
    vmm_migrate::wire::MAX_DEST_ATTACH;

/// What `migrate-source` asks for beyond the target.
pub(in crate::control) struct SourceAsk {
    pub(in crate::control) zfs_barrier: Option<String>,
    pub(in crate::control) gate: Option<String>,
    pub(in crate::control) gate_timeout_ms: Option<u64>,
    pub(in crate::control) allow_unproven: bool,
}
