// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The memory hotplug slot selector.

use crate::lifecycle::{
    DeviceMigrateState, DeviceStateError, Lifecycle, MemHotplugMigrateState,
};

use super::MemHotplug;

impl Lifecycle for MemHotplug {
    fn type_name(&self) -> &'static str {
        "mem-hotplug"
    }

    fn migrates_by_id(&self) -> bool {
        true
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        let inner = self.inner.lock().expect("mem hotplug lock poisoned");
        // MAX_SLOTS fits a byte, so every selected slot does.
        let selected = inner.selected.and_then(|i| u8::try_from(i).ok());
        Ok(Some(DeviceMigrateState::MemHotplug(
            MemHotplugMigrateState { selected },
        )))
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::MemHotplug(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "mem-hotplug",
                got: state.kind(),
            });
        };
        let mut inner = self.inner.lock().expect("mem hotplug lock poisoned");
        let selected = mig.selected.map(usize::from);
        if let Some(slot) = selected {
            if slot >= inner.slots.len() {
                return Err(DeviceStateError::invalid(format!(
                    "mem hotplug: slot {slot} of {}",
                    inner.slots.len()
                )));
            }
        }
        inner.selected = selected;
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

    fn block() -> Arc<MemHotplug> {
        MemHotplug::new(
            4,
            Arc::new(NoSink),
            Logger::root(slog::Discard, slog::o!()),
        )
    }

    #[test]
    fn the_selected_slot_survives_and_a_bad_one_is_refused() {
        let src = block();
        src.inner.lock().expect("lock").selected = Some(2);
        let state = src.export_migrate_state().expect("export").expect("x");
        let dst = block();
        dst.restore_migrate_state(&state).expect("restore");
        assert_eq!(dst.inner.lock().expect("lock").selected, Some(2));

        let bad = DeviceMigrateState::MemHotplug(MemHotplugMigrateState {
            selected: Some(4),
        });
        assert!(dst.restore_migrate_state(&bad).is_err());
        assert_eq!(dst.inner.lock().expect("lock").selected, Some(2));
    }
}
