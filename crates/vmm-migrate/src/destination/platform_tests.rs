// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The checks on devices with no PCI address, and on the kinds that
//! always carry a state.

use std::sync::atomic::{AtomicUsize, Ordering};

use vmm_devices::lifecycle::{
    DeviceStatePayload, UartMigrateState, VirtioMigrateState, XhciMigrateState,
};

use super::*;

fn uart(id: &str) -> PlatformIdentity {
    PlatformIdentity {
        id: id.to_string(),
        kind: "lpc-uart".to_string(),
    }
}

fn uart_state(id: &str) -> PlatformStatePayload {
    PlatformStatePayload {
        id: id.to_string(),
        state: DeviceMigrateState::Uart(UartMigrateState::default()),
    }
}

fn config(
    devices: Vec<DeviceIdentity>,
    platform: Vec<PlatformIdentity>,
) -> DestConfig {
    DestConfig {
        num_cpus: 1,
        mem_size: 4096,
        cpu_baseline: CpuBaseline::Sse42,
        devices,
        platform,
        deadlines: Deadlines::default(),
        hyperv: false,
        allow_cpu_feature_mismatch: false,
    }
}

/// Hooks that count the platform restores and refuse nothing.
fn hooks(restored: &'static AtomicUsize) -> DestHooks {
    DestHooks {
        restore: Box::new(|_, _| Ok(())),
        restore_platform: Box::new(move |id, state| {
            assert!(id.starts_with("lpc/"));
            assert!(matches!(state, DeviceMigrateState::Uart(_)));
            restored.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        attach: Box::new(|| Ok(())),
        resume: Box::new(|| {}),
        hyperv: Box::new(|_| false),
        pause_vm: Box::new(|| Ok(())),
        resume_vm: Box::new(|| Ok(())),
        gate: Box::new(crate::NoStop),
    }
}

fn state(
    emulated: Vec<DeviceStatePayload>,
    platform: Vec<PlatformStatePayload>,
) -> DeviceState {
    DeviceState {
        vcpus: Vec::new(),
        devices: Vec::new(),
        emulated,
        hyperv: None,
        platform,
    }
}

fn discard() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn both_uarts() -> Vec<PlatformIdentity> {
    vec![uart("lpc/com1"), uart("lpc/com2")]
}

#[test]
fn every_uart_is_restored_under_its_id() {
    static RESTORED: AtomicUsize = AtomicUsize::new(0);
    let payload = state(
        Vec::new(),
        vec![uart_state("lpc/com2"), uart_state("lpc/com1")],
    );
    restore_devices(
        &payload,
        &config(Vec::new(), both_uarts()),
        &hooks(&RESTORED),
        &discard(),
    )
    .expect("restore");
    assert_eq!(RESTORED.load(Ordering::SeqCst), 2);
}

#[test]
fn a_missing_extra_or_doubled_platform_state_is_refused() {
    let local = both_uarts();
    let missing = [uart_state("lpc/com1")];
    let error =
        check_platform_payload(&missing, &local).expect_err("com2 missing");
    assert!(error.to_string().contains("lpc/com2"), "{error}");

    let extra = [
        uart_state("lpc/com1"),
        uart_state("lpc/com2"),
        uart_state("lpc/com3\n"),
    ];
    let error = check_platform_payload(&extra, &local).expect_err("extra");
    assert!(error.to_string().contains("com3\\n"), "escaped: {error}");

    let doubled = [
        uart_state("lpc/com1"),
        uart_state("lpc/com1"),
        uart_state("lpc/com2"),
    ];
    check_platform_payload(&doubled, &local).expect_err("doubled");
}

#[test]
fn a_platform_set_that_differs_is_refused_at_the_preamble() {
    check_topology(&both_uarts(), &both_uarts()).expect("same set");
    let error = check_topology(&both_uarts(), &[uart("lpc/com1")])
        .expect_err("one UART fewer");
    assert!(error.to_string().contains("lpc-uart@lpc/com2"), "{error}");
}

#[test]
fn an_xhci_controller_with_no_state_is_refused() {
    static RESTORED: AtomicUsize = AtomicUsize::new(0);
    let bdf = WireBdf {
        bus: 0,
        dev: 30,
        func: 1,
    };
    let local = vec![DeviceIdentity {
        bdf,
        kind: protocol::XHCI_KIND.to_string(),
    }];
    let none = state(vec![DeviceStatePayload { bdf, state: None }], Vec::new());
    let error = restore_devices(
        &none,
        &config(local.clone(), Vec::new()),
        &hooks(&RESTORED),
        &discard(),
    )
    .expect_err("no xhci state");
    assert!(error.to_string().contains("no xhci state"), "{error}");

    let some = state(
        vec![DeviceStatePayload {
            bdf,
            state: Some(DeviceMigrateState::Xhci(XhciMigrateState::default())),
        }],
        Vec::new(),
    );
    restore_devices(
        &some,
        &config(local, Vec::new()),
        &hooks(&RESTORED),
        &discard(),
    )
    .expect("restore");
}

#[test]
fn a_virtio_device_with_no_state_is_refused() {
    static RESTORED: AtomicUsize = AtomicUsize::new(0);
    let bdf = WireBdf {
        bus: 0,
        dev: 4,
        func: 0,
    };
    let local = vec![DeviceIdentity {
        bdf,
        kind: "virtio-blk".to_string(),
    }];
    let none = state(vec![DeviceStatePayload { bdf, state: None }], Vec::new());
    let error = restore_devices(
        &none,
        &config(local.clone(), Vec::new()),
        &hooks(&RESTORED),
        &discard(),
    )
    .expect_err("no virtio state");
    assert!(error.to_string().contains("no virtio-blk state"), "{error}");

    let some = state(
        vec![DeviceStatePayload {
            bdf,
            state: Some(DeviceMigrateState::Virtio(
                VirtioMigrateState::default(),
            )),
        }],
        Vec::new(),
    );
    restore_devices(
        &some,
        &config(local, Vec::new()),
        &hooks(&RESTORED),
        &discard(),
    )
    .expect("restore");
}

#[test]
fn a_hyperv_vm_needs_the_enlightenment_in_the_payload() {
    static RESTORED: AtomicUsize = AtomicUsize::new(0);
    let mut local = config(Vec::new(), Vec::new());
    local.hyperv = true;
    let error = restore_devices(
        &state(Vec::new(), Vec::new()),
        &local,
        &hooks(&RESTORED),
        &discard(),
    )
    .expect_err("no Hyper-V state");
    assert!(error.to_string().contains("--hyperv"), "{error}");

    // And the other way: the hook answers that this VM has none.
    let mut payload = state(Vec::new(), Vec::new());
    payload.hyperv = Some(HypervMigrateState::default());
    restore_devices(
        &payload,
        &config(Vec::new(), Vec::new()),
        &hooks(&RESTORED),
        &discard(),
    )
    .expect_err("Hyper-V state for a VM without it");
}

#[test]
fn a_kind_mismatch_in_a_payload_is_refused() {
    // The device answers for its own kind; the hook here plays one.
    let bdf = WireBdf {
        bus: 0,
        dev: 4,
        func: 0,
    };
    let local = vec![DeviceIdentity {
        bdf,
        kind: "virtio-blk".to_string(),
    }];
    static RESTORED: AtomicUsize = AtomicUsize::new(0);
    let mut wrong = hooks(&RESTORED);
    wrong.restore = Box::new(|_, state| match state {
        DeviceMigrateState::Virtio(_) => Ok(()),
        other => Err(DeviceStateError::WrongKind {
            want: "virtio",
            got: other.kind(),
        }),
    });
    let payload = state(
        vec![DeviceStatePayload {
            bdf,
            state: Some(DeviceMigrateState::Xhci(XhciMigrateState::default())),
        }],
        Vec::new(),
    );
    let error = restore_devices(
        &payload,
        &config(local, Vec::new()),
        &wrong,
        &discard(),
    )
    .expect_err("xhci state for a virtio device");
    assert!(error.to_string().contains("xhci"), "{error}");
}
