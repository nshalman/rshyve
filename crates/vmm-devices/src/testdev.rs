// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A device that fails a migration on purpose, so a lab or CI run can
//! test a retry and the guest memory after it. Propolis has the same in
//! `lib/propolis/src/hw/testdev.rs`.
//!
//! rshyve adds it only behind a hidden lab flag. Both ends need it, or
//! the device set check refuses the migration.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use slog::Logger;

use crate::lifecycle::{
    DeviceMigrateState, DeviceStateError, Lifecycle, TestFaultMigrateState,
};

/// The device's `type_name`, and the kind its payload carries.
pub const KIND: &str = "migrate-test-fault";

/// Its registry id, which names it on the wire.
pub const ID: &str = "test/migrate-fault";

/// Fails its first `fail_exports` exports and its first `fail_imports`
/// imports, then works.
pub struct MigrateFault {
    fail_exports: usize,
    fail_imports: usize,
    exports: AtomicUsize,
    imports: AtomicUsize,
    log: Logger,
}

impl MigrateFault {
    pub fn new(
        fail_exports: usize,
        fail_imports: usize,
        log: &Logger,
    ) -> Arc<Self> {
        let log = log.new(slog::o!("dev" => KIND));
        slog::warn!(log, "this VM fails migrations on purpose";
            "fail_exports" => fail_exports, "fail_imports" => fail_imports);
        Arc::new(Self {
            fail_exports,
            fail_imports,
            exports: AtomicUsize::new(0),
            imports: AtomicUsize::new(0),
            log,
        })
    }
}

impl Lifecycle for MigrateFault {
    fn type_name(&self) -> &'static str {
        KIND
    }

    fn migrates_by_id(&self) -> bool {
        true
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        let n = self.exports.fetch_add(1, Ordering::AcqRel);
        if n < self.fail_exports {
            slog::warn!(self.log, "failing an export"; "export" => n + 1);
            return Err(DeviceStateError::Export(format!(
                "injected failure, export {} of {}",
                n + 1,
                self.fail_exports
            )));
        }
        Ok(Some(DeviceMigrateState::TestFault(
            TestFaultMigrateState {},
        )))
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::TestFault(TestFaultMigrateState {}) = state
        else {
            return Err(DeviceStateError::WrongKind {
                want: KIND,
                got: state.kind(),
            });
        };
        let n = self.imports.fetch_add(1, Ordering::AcqRel);
        if n < self.fail_imports {
            slog::warn!(self.log, "failing an import"; "import" => n + 1);
            return Err(DeviceStateError::invalid(format!(
                "injected failure, import {} of {}",
                n + 1,
                self.fail_imports
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn it_fails_the_first_exports_and_then_works() {
        let dev = MigrateFault::new(2, 0, &log());
        assert!(dev.export_migrate_state().is_err());
        assert!(dev.export_migrate_state().is_err());
        let state = dev.export_migrate_state().expect("third").expect("x");
        dev.restore_migrate_state(&state)
            .expect("no import failures");
    }

    #[test]
    fn it_fails_the_first_imports_and_then_works() {
        let dev = MigrateFault::new(0, 1, &log());
        let state = dev.export_migrate_state().expect("export").expect("x");
        assert!(dev.restore_migrate_state(&state).is_err());
        dev.restore_migrate_state(&state).expect("second import");
    }

    #[test]
    fn it_refuses_another_kind() {
        let dev = MigrateFault::new(0, 0, &log());
        let other = DeviceMigrateState::MemHotplug(Default::default());
        let error = dev.restore_migrate_state(&other).expect_err("kind");
        assert!(matches!(error, DeviceStateError::WrongKind { .. }));
        // A refused kind is not counted as an import.
        assert_eq!(dev.imports.load(Ordering::Acquire), 0);
    }
}
