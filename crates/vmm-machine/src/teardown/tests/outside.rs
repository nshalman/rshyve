// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Teardown and a durable flush a stopped migration left behind.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::outside::flush_durable;
use crate::registry::{DeviceRegistry, RegisteredDevice};

/// Records each call as `<name>:<step>`. A durable flush of a device
/// built with `hold` waits until the test drops the sender.
struct Witness {
    name: &'static str,
    steps: Steps,
    hold: Option<Mutex<mpsc::Receiver<()>>>,
    entered: Mutex<Option<mpsc::Sender<()>>>,
}

impl Witness {
    fn new(name: &'static str, steps: &Steps) -> Self {
        Self {
            name,
            steps: Arc::clone(steps),
            hold: None,
            entered: Mutex::new(None),
        }
    }

    fn record(&self, step: &str) {
        let line: &'static str =
            Box::leak(format!("{}:{step}", self.name).into_boxed_str());
        push(&self.steps, line);
    }
}

impl vmm_devices::Lifecycle for Witness {
    fn type_name(&self) -> &'static str {
        self.name
    }

    fn pause(&self) {
        self.record("pause");
    }

    fn halt(&self) {
        self.record("halt");
    }

    fn flush_backing(
        &self,
        intent: vmm_devices::FlushIntent,
    ) -> Result<(), vmm_devices::FlushError> {
        if intent != vmm_devices::FlushIntent::Durable {
            self.record("flush");
            return Ok(());
        }
        self.record("durable");
        if let Some(entered) = self.entered.lock().expect("entered").take() {
            let _ = entered.send(());
        }
        if let Some(hold) = &self.hold {
            let _released = hold.lock().expect("hold").recv().is_err();
        }
        self.record("durable-done");
        Ok(())
    }
}

struct Scene {
    steps: Steps,
    registry: Arc<DeviceRegistry>,
    release: mpsc::Sender<()>,
    entered: mpsc::Receiver<()>,
}

/// Two devices; the first one's durable flush waits for the test.
fn scene() -> Scene {
    let steps = Steps::default();
    let (release, hold) = mpsc::channel();
    let (entered_tx, entered) = mpsc::channel();
    let first = Witness {
        hold: Some(Mutex::new(hold)),
        entered: Mutex::new(Some(entered_tx)),
        ..Witness::new("a", &steps)
    };
    let registry = Arc::new(DeviceRegistry::new());
    for (id, device) in [
        ("a", Arc::new(first) as Arc<dyn vmm_devices::Lifecycle>),
        ("b", Arc::new(Witness::new("b", &steps))),
    ] {
        registry
            .insert(RegisteredDevice::new(id, None, None, Some(device), None))
            .expect("registered");
    }
    Scene {
        steps,
        registry,
        release,
        entered,
    }
}

fn teardown(registry: &DeviceRegistry, steps: &Steps, gave_up: &AtomicBool) {
    let arm_steps = Arc::clone(steps);
    let destroy_steps = Arc::clone(steps);
    shutdown_after_exit(
        VcpuSet::Fixed(Vec::new()),
        &crate::teardown::registry_named_devices(registry),
        registry.outside_work(),
        None,
        move || {
            push(&destroy_steps, "destroy");
            Ok(())
        },
        Watchdog {
            arm: move |_| push(&arm_steps, "arm"),
            give_up: || gave_up.store(true, Ordering::Release),
        },
        &null_log(),
    );
}

#[test]
fn teardown_waits_for_an_abandoned_flush_and_the_flush_stops_early() {
    let scene = scene();
    let abandoned = Arc::new(AtomicBool::new(false));
    let helper = {
        let registry = Arc::clone(&scene.registry);
        let abandoned = Arc::clone(&abandoned);
        thread::spawn(move || {
            flush_durable(&registry, || abandoned.load(Ordering::Acquire))
        })
    };
    scene
        .entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the flush reached the first device");
    // The stop gives up on the flush, then teardown starts while the
    // first device is still flushing.
    abandoned.store(true, Ordering::Release);
    let gave_up = AtomicBool::new(false);
    thread::scope(|s| {
        let tearing =
            s.spawn(|| teardown(&scene.registry, &scene.steps, &gave_up));
        let armed = Instant::now() + Duration::from_secs(10);
        while !steps_of(&scene.steps).contains(&"arm") {
            assert!(Instant::now() < armed, "teardown never armed");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!tearing.is_finished(), "teardown ran beside the flush");
        drop(scene.release);
    });

    assert!(matches!(
        helper.join().expect("helper"),
        Err(crate::OutsideFlushError::Abandoned)
    ));
    assert!(!gave_up.load(Ordering::Acquire));
    assert_eq!(
        steps_of(&scene.steps),
        [
            "a:durable",
            "arm",
            "a:durable-done",
            "arm",
            "a:pause",
            "b:pause",
            "a:flush",
            "b:flush",
            "a:halt",
            "b:halt",
            "destroy",
        ]
    );
}

#[test]
fn a_flush_that_starts_after_teardown_touches_nothing() {
    let scene = scene();
    let gave_up = AtomicBool::new(false);
    teardown(&scene.registry, &scene.steps, &gave_up);
    let before = steps_of(&scene.steps);

    assert!(matches!(
        flush_durable(&scene.registry, || false),
        Err(crate::OutsideFlushError::TearingDown)
    ));
    assert_eq!(steps_of(&scene.steps), before);
}

#[test]
fn a_flush_that_never_ends_sends_teardown_to_the_hard_exit() {
    let outside = OutsideWork::default();
    let _stuck = outside.enter().expect("open");
    let gave_up = AtomicBool::new(false);

    let go_on = wait_outside(
        &outside,
        Duration::from_millis(50),
        Watchdog {
            arm: |_| panic!("re-armed for work that never ended"),
            give_up: || gave_up.store(true, Ordering::Release),
        },
        &null_log(),
    );

    assert!(!go_on, "teardown went on beside running device work");
    assert!(gave_up.load(Ordering::Acquire));
}
