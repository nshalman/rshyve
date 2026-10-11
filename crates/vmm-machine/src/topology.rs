// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lock every change to the device set is made under.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

/// Shared by the hotplug engine, which adds and ejects devices, and the
/// control plane, which pauses, resumes and migrates over the set. A
/// caller that reads the set under it sees one set from start to end.
#[derive(Clone, Default)]
pub struct TopologyLock(Arc<Topology>);

#[derive(Default)]
struct Topology {
    lock: Mutex<()>,
    ejects_held: AtomicBool,
}

impl TopologyLock {
    /// The lock guards no data, so a poisoned one is taken as it is.
    pub fn lock(&self) -> MutexGuard<'_, ()> {
        self.0.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keep guest ejects queued until this is called again with
    /// `false`. Set under the lock, so an eject already running ends
    /// before the caller goes on.
    pub fn hold_ejects(&self, _locked: &MutexGuard<'_, ()>, held: bool) {
        self.0.ejects_held.store(held, Ordering::Release);
    }

    /// Read under the lock: an eject must not start while this is set.
    pub fn ejects_held(&self) -> bool {
        self.0.ejects_held.load(Ordering::Acquire)
    }

    /// Whether another holder has it now. For tests that check a step
    /// ran under the lock.
    pub fn is_held(&self) -> bool {
        matches!(self.0.lock.try_lock(), Err(TryLockError::WouldBlock))
    }
}
