// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CPU hotplug latches. A guest paused between the selector write
//! and the status read would otherwise read slot 0 on the destination,
//! and could bring up a CPU that is not there.

use crate::lifecycle::{
    CpuHotplugMigrateState, DeviceMigrateState, DeviceStateError, Lifecycle,
};

use super::{CpuHotplug, CMD_MAX, LOCK_POISONED};

impl Lifecycle for CpuHotplug {
    fn type_name(&self) -> &'static str {
        "cpu-hotplug"
    }

    fn migrates_by_id(&self) -> bool {
        true
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        let inner = self.inner.lock().expect(LOCK_POISONED);
        Ok(Some(DeviceMigrateState::CpuHotplug(
            CpuHotplugMigrateState {
                selector: inner.selector,
                command: inner.command,
            },
        )))
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::CpuHotplug(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "cpu-hotplug",
                got: state.kind(),
            });
        };
        if mig.command >= CMD_MAX {
            return Err(DeviceStateError::invalid(format!(
                "cpu hotplug: command {}",
                mig.command
            )));
        }
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        // The selector goes through the guest path, so the slot it
        // names is checked here once, as on every guest write.
        inner.select(mig.selector);
        inner.command = mig.command;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use slog::Logger;

    use super::*;
    use crate::acpi_gpe::{GpeBit, HotplugEventSink};

    struct NoSink;

    impl HotplugEventSink for NoSink {
        fn raise(&self, _bit: GpeBit) {}
    }

    fn block() -> Arc<CpuHotplug> {
        let log = Logger::root(slog::Discard, slog::o!());
        let cpus = CpuHotplug::new(4, Arc::new(NoSink), log);
        cpus.set_boot_cpus(2);
        cpus
    }

    #[test]
    fn the_selected_slot_survives_the_wire() {
        let src = block();
        src.inner.lock().expect("lock").select(3);
        src.inner.lock().expect("lock").command = 3;
        let state = src.export_migrate_state().expect("export").expect("x");

        let dst = block();
        dst.restore_migrate_state(&state).expect("restore");
        let inner = dst.inner.lock().expect("lock");
        assert_eq!(
            (inner.selector, inner.index, inner.command),
            (3, Some(3), 3)
        );
    }

    #[test]
    fn a_selector_past_the_slots_selects_nothing() {
        let dst = block();
        let state = DeviceMigrateState::CpuHotplug(CpuHotplugMigrateState {
            selector: 99,
            command: 0,
        });
        dst.restore_migrate_state(&state).expect("restore");
        assert_eq!(dst.inner.lock().expect("lock").index, None);

        let bad = DeviceMigrateState::CpuHotplug(CpuHotplugMigrateState {
            selector: 0,
            command: CMD_MAX,
        });
        assert!(dst.restore_migrate_state(&bad).is_err());
    }
}
