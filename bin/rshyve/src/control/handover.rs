// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Disks that change hands in a live migration, and the source the
//! orchestrator gives its guest back to.
//!
//! A MantaBlock ring disk is detached on the source right after the
//! commit and attached anew on the destination before any device
//! resumes. Spec `61` and docs/migration-gate.md.

use std::sync::atomic::Ordering;
use std::time::Instant;

use slog::{error, info};
use vmm_devices::lifecycle::BackingAttach;

use super::run_state::{HeldFailed, StepFailed};
use super::{pause_device_set, VmController, DEVICE_PAUSE_BUDGET};

/// What `migrate-status` reports for the ring disks.
pub(super) const RINGS_ATTACHED: &str = "attached";
pub(super) const RINGS_ATTACHING: &str = "attaching";
pub(super) const RINGS_DETACHED: &str = "detached";
pub(super) const RINGS_ATTACH_FAILED: &str = "attach-failed";
pub(super) const RINGS_DETACH_FAILED: &str = "detach-failed";

impl VmController {
    /// Take a ring engine that cannot prove a fresh attach is new, for
    /// this migration and the attaches that finish it.
    pub(super) fn set_allow_unproven(&self, allowed: bool) {
        self.allow_unproven.store(allowed, Ordering::Release);
    }

    pub(super) fn migration_status(
        &self,
    ) -> Option<vmm_migrate::MigrationStatus> {
        self.migrate_status
            .lock()
            .ok()?
            .as_ref()?
            .lock()
            .ok()
            .map(|s| s.clone())
    }

    /// Whether this VM has a disk that changes hands.
    fn has_handover_disks(&self) -> bool {
        self.named_devices()
            .iter()
            .any(|d| d.device.migration_spec_options().is_some())
    }

    fn set_rings(&self, state: &'static str) {
        if let Ok(mut rings) = self.rings.lock() {
            *rings = Some(state);
        }
    }

    /// The ring disks' state for `migrate-status`, or `None` for a VM
    /// that has none.
    pub(super) fn rings_state(&self) -> Option<&'static str> {
        if !self.has_handover_disks() {
            return None;
        }
        let recorded = self.rings.lock().ok().and_then(|r| *r);
        Some(recorded.unwrap_or(RINGS_ATTACHED))
    }

    /// Give every disk up after the commit. Runs on the migration
    /// thread; nothing can undo the commit, so a failure is reported.
    pub(super) fn detach_backing(&self) {
        let mut failed = Vec::new();
        for named in self.named_devices() {
            if let Err(e) = named.device.detach_backing() {
                failed.push(format!("{}: {e}", label(named)));
            }
        }
        if !self.has_handover_disks() {
            return;
        }
        if failed.is_empty() {
            info!(self.log, "ring disks detached after the commit");
            self.set_rings(RINGS_DETACHED);
        } else {
            error!(self.log, "a ring disk did not confirm its Detach";
                "failed" => failed.join("; "));
            self.set_rings(RINGS_DETACH_FAILED);
        }
    }

    /// Attach every disk that is detached, waiting up to `until` for
    /// each engine to serve.
    pub(super) fn attach_backing(&self, until: Instant) -> Result<(), String> {
        let how = BackingAttach {
            until,
            allow_unproven: self.allow_unproven.load(Ordering::Acquire),
        };
        let detached: Vec<_> = self
            .named_devices()
            .into_iter()
            .filter(|d| !d.device.backing_attached())
            .collect();
        if detached.is_empty() {
            return Ok(());
        }
        self.set_rings(RINGS_ATTACHING);
        for named in &detached {
            if let Err(e) = named.device.attach_backing(&how) {
                self.set_rings(RINGS_ATTACH_FAILED);
                return Err(format!("{}: {e}", label(named)));
            }
        }
        info!(self.log, "ring disks attached"; "disks" => detached.len());
        self.set_rings(RINGS_ATTACHED);
        Ok(())
    }

    /// Run the guest here again on the orchestrator's order. Every
    /// disk attaches before any device resumes: a device resume alone
    /// can submit the writes the guest left queued.
    pub(super) fn resume_held_source(
        &self,
        until: Instant,
    ) -> Result<(), String> {
        let resumed = self.run.resume_held(|held| {
            self.attach_backing(until).map_err(StepFailed::Undone)?;
            self.resume_devices_after_migration();
            if let Err(e) = self.release_pause() {
                error!(self.log, "resume failed"; "error" => %e);
                if let Err(stuck) =
                    pause_device_set(&self.named_devices(), DEVICE_PAUSE_BUDGET)
                {
                    error!(self.log, "devices run under a held pause";
                        "stuck" => ?stuck);
                    return Err(StepFailed::Faulted(
                        "resume failed and the devices would not park \
                         again; only a stop is taken"
                            .to_string(),
                    ));
                }
                return Err(StepFailed::Undone(format!("resume: {e}")));
            }
            *held = false;
            Ok(())
        });
        resumed.map_err(|e| match e {
            HeldFailed::NotHeld => "not a source waiting for the \
                orchestrator's order: it never got the gate's go and an \
                unconfirmed commit, or its destination confirmed the start"
                .to_string(),
            HeldFailed::Running => {
                "the migration is still running; retry when it ends".to_string()
            }
            HeldFailed::Step(why) => why,
        })
    }
}

fn label(named: &vmm_devices::quiesce::NamedDevice) -> String {
    named
        .id
        .clone()
        .unwrap_or_else(|| named.device.type_name().to_string())
}
