// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lock every change to the device set is made under.

use std::sync::{Arc, Mutex, MutexGuard};

/// Shared by the hotplug engine, which adds and ejects devices, and the
/// control plane, which pauses, resumes and migrates over the set. A
/// caller that reads the set under it sees one set from start to end.
#[derive(Clone, Default)]
pub struct TopologyLock(Arc<Mutex<()>>);

impl TopologyLock {
    /// The lock guards no data, so a poisoned one is taken as it is.
    pub fn lock(&self) -> MutexGuard<'_, ()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether another holder has it now. For tests that check a step
    /// ran under the lock.
    pub fn is_held(&self) -> bool {
        self.0.try_lock().is_err()
    }
}
