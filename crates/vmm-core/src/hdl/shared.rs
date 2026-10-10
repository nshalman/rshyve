// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One ring gate per VM in this process, however many handles open it.
//!
//! The write lock is the VM's, not the handle's. A second handle with
//! a gate of its own would take the write lock while the first gate's
//! rings run.

use std::fs::File;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex, Weak};

use crate::rings::{PausedRings, RingGate};

/// What every handle on one VM shares.
pub(super) struct VmRings {
    pub(super) gate: Arc<RingGate>,
    /// Keeps the rings stopped from `VM_PAUSE` to `VM_RESUME`.
    pub(super) paused: PausedRings,
}

/// The VM's name and the device number of its node. The device number
/// tells apart a VM destroyed and made again under the same name while
/// an old handle is still open.
#[derive(Clone, PartialEq, Eq)]
struct Key {
    name: String,
    rdev: u64,
}

static VMS: Mutex<Vec<(Key, Weak<VmRings>)>> = Mutex::new(Vec::new());

/// The shared state for the VM `fd` opens.
pub(super) fn for_vm(name: &str, fd: BorrowedFd<'_>) -> Arc<VmRings> {
    let key = Key {
        name: name.to_string(),
        rdev: rdev(fd),
    };
    let mut vms = VMS.lock().unwrap_or_else(|e| e.into_inner());
    vms.retain(|(_, vm)| vm.strong_count() > 0);
    // A node that could not be read (rdev 0) matches by name: two
    // gates for one VM are worse than one gate for a name reused.
    let same = |k: &Key| {
        k.name == key.name
            && (k.rdev == key.rdev || k.rdev == 0 || key.rdev == 0)
    };
    if let Some(vm) = vms
        .iter()
        .find(|(k, _)| same(k))
        .and_then(|(_, vm)| vm.upgrade())
    {
        return vm;
    }
    let vm = Arc::new(VmRings {
        gate: RingGate::new(),
        paused: PausedRings::default(),
    });
    vms.push((key, Arc::downgrade(&vm)));
    vm
}

/// 0 when the node cannot be read; the name alone then keys the VM.
fn rdev(fd: BorrowedFd<'_>) -> u64 {
    fd.try_clone_to_owned()
        .map(File::from)
        .and_then(|f| f.metadata())
        .map_or(0, |m| m.rdev())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn two_handles_on_one_vm_share_one_gate() {
        let node = tempfile::tempfile().expect("node");
        let other = tempfile::tempfile().expect("other");
        let a = for_vm("shared-gate-test", node.as_fd());
        let b = for_vm("shared-gate-test", node.as_fd());
        assert!(Arc::ptr_eq(&a.gate, &b.gate));
        let c = for_vm("shared-gate-test-2", other.as_fd());
        assert!(!Arc::ptr_eq(&a.gate, &c.gate));
        // Once every handle is gone, the next one starts fresh.
        let gate = Arc::downgrade(&a.gate);
        drop((a, b));
        assert!(gate.upgrade().is_none());
    }
}
