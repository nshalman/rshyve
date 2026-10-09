// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The devices with no PCI address that a migration carries, named on
//! the wire by their registry ids.

use std::sync::Arc;

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, PlatformIdentity,
    PlatformStatePayload,
};
use vmm_devices::Lifecycle;
use vmm_machine::DeviceRegistry;

pub(crate) type PlatformDevice = (PlatformIdentity, Arc<dyn Lifecycle>);

/// Read from the live registry on each call, like the PCI set.
pub(crate) fn devices(registry: &DeviceRegistry) -> Vec<PlatformDevice> {
    registry
        .list()
        .into_iter()
        .filter(|slot| slot.bdf.is_none())
        .filter_map(|slot| {
            let device = slot.lifecycle?;
            device.migrates_by_id().then(|| {
                let ident = PlatformIdentity {
                    id: slot.id,
                    kind: device.type_name().to_string(),
                };
                (ident, device)
            })
        })
        .collect()
}

pub(crate) fn identities(registry: &DeviceRegistry) -> Vec<PlatformIdentity> {
    devices(registry)
        .into_iter()
        .map(|(ident, _)| ident)
        .collect()
}

/// Every one of them must export a state: the destination refuses a
/// payload that leaves one out.
pub(crate) fn export(
    registry: &DeviceRegistry,
) -> Result<Vec<PlatformStatePayload>, DeviceStateError> {
    devices(registry)
        .into_iter()
        .map(|(ident, device)| {
            let state = device.export_migrate_state()?.ok_or_else(|| {
                DeviceStateError::Export(format!(
                    "{} exported no state",
                    ident.id
                ))
            })?;
            Ok(PlatformStatePayload {
                id: ident.id,
                state,
            })
        })
        .collect()
}

pub(crate) fn restore(
    devices: &[PlatformDevice],
    id: &str,
    state: &DeviceMigrateState,
) -> Result<(), DeviceStateError> {
    let (_, device) = devices
        .iter()
        .find(|(ident, _)| ident.id == id)
        .ok_or_else(|| {
            DeviceStateError::Invalid(format!("no device under {id:?}"))
        })?;
    device.restore_migrate_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::intr_pins::NoOpPin;
    use vmm_devices::uart::LpcUart;
    use vmm_machine::RegisteredDevice;

    struct Plain;

    impl Lifecycle for Plain {
        fn type_name(&self) -> &'static str {
            "varstore"
        }
    }

    fn registry() -> DeviceRegistry {
        let registry = DeviceRegistry::new();
        for (id, device) in [
            (
                "lpc/com2",
                LpcUart::new(Arc::new(NoOpPin)) as Arc<dyn Lifecycle>,
            ),
            ("lpc/com1", LpcUart::new(Arc::new(NoOpPin)) as _),
            ("varstore", Arc::new(Plain) as _),
        ] {
            vmm_machine::register_platform(&registry, id, device)
                .expect("register");
        }
        registry
    }

    #[test]
    fn only_devices_that_migrate_by_id_are_named() {
        let mut ids: Vec<String> =
            identities(&registry()).into_iter().map(|i| i.id).collect();
        ids.sort();
        assert_eq!(ids, ["lpc/com1", "lpc/com2"]);
        let registry = registry();
        registry
            .insert(RegisteredDevice::new(
                "uart@5",
                vmm_machine::parse_bdf("5"),
                None,
                Some(LpcUart::new(Arc::new(NoOpPin)) as Arc<dyn Lifecycle>),
                None,
            ))
            .expect("insert");
        assert_eq!(identities(&registry).len(), 2, "a PCI address wins");
    }

    #[test]
    fn export_then_restore_by_id() {
        let src = registry();
        let payload = export(&src).expect("export");
        assert_eq!(payload.len(), 2);

        let dst = devices(&registry());
        for entry in &payload {
            restore(&dst, &entry.id, &entry.state).expect("restore");
        }
        let error = restore(&dst, "lpc/com3", &payload[0].state)
            .expect_err("no such id");
        assert!(error.to_string().contains("lpc/com3"), "{error}");
    }
}
