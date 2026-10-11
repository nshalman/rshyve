// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use vmm_core::common::{RWOp, WriteOp};
use vmm_devices::acpi_gpe::{GpeBit, HotplugEventSink};
use vmm_devices::pci::{BarN, PciDevice};
use vmm_devices::{IndicatedState, Lifecycle};

use super::*;

/// A device with no behaviour, so a test can watch what the engine does
/// to it rather than what it does itself.
#[derive(Default)]
struct StubPci {
    regions_detached: AtomicUsize,
    trace: Option<Arc<Trace>>,
}

impl StubPci {
    fn traced(trace: &Arc<Trace>) -> Self {
        Self {
            regions_detached: AtomicUsize::new(0),
            trace: Some(Arc::clone(trace)),
        }
    }
}

impl PciDevice for StubPci {
    fn cfg_read(&self, _offset: u8, _len: u8) -> u32 {
        // A real vendor id, so the slot does not read as empty.
        0x1AF4_1001
    }
    fn cfg_write(&self, _offset: u8, _len: u8, _val: u32) {}
    fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    fn detach_regions(&self) {
        self.regions_detached.fetch_add(1, Ordering::AcqRel);
        Trace::record(&self.trace, "detach-regions");
    }
}

/// An ordered record of what the engine did, so a test can assert the
/// order and not just the counts.
#[derive(Default)]
struct Trace(Mutex<Vec<&'static str>>);

impl Trace {
    fn record(trace: &Option<Arc<Self>>, step: &'static str) {
        if let Some(trace) = trace {
            trace.0.lock().expect("trace lock").push(step);
        }
    }

    fn steps(&self) -> Vec<&'static str> {
        self.0.lock().expect("trace lock").clone()
    }
}

/// A backend that reports whether its work has drained.
struct StubLifecycle {
    quiesces: bool,
    paused: AtomicBool,
    halted: AtomicBool,
    resumes: AtomicUsize,
    /// What `lifecycle_state` reports. `None` is a device that tracks
    /// no state, which is what most backends do.
    state: Option<IndicatedState>,
    /// A halt that does not return, as viona's `VNA_IOC_DELETE` does
    /// when a ring worker will not stop. Cleared so the test thread is
    /// not left spinning.
    halt_wedges: AtomicBool,
}

impl StubLifecycle {
    fn new(quiesces: bool) -> Arc<Self> {
        Arc::new(Self {
            quiesces,
            paused: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            resumes: AtomicUsize::new(0),
            state: None,
            halt_wedges: AtomicBool::new(false),
        })
    }

    /// A device whose release never returns.
    fn wedged_halt() -> Arc<Self> {
        Arc::new(Self {
            quiesces: true,
            paused: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            resumes: AtomicUsize::new(0),
            state: None,
            halt_wedges: AtomicBool::new(true),
        })
    }

    /// A device an operator has already paused, through the control
    /// socket. Resuming it would start it behind their back.
    fn already_paused() -> Arc<Self> {
        Arc::new(Self {
            quiesces: false,
            paused: AtomicBool::new(true),
            halted: AtomicBool::new(false),
            resumes: AtomicUsize::new(0),
            state: Some(IndicatedState::Pause),
            halt_wedges: AtomicBool::new(false),
        })
    }

    fn is_running(&self) -> bool {
        !self.paused.load(Ordering::Acquire)
    }
}

impl Lifecycle for StubLifecycle {
    fn type_name(&self) -> &'static str {
        "stub"
    }
    fn lifecycle_state(&self) -> Option<IndicatedState> {
        self.state
    }
    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }
    fn resume(&self) {
        assert!(
            self.paused.swap(false, Ordering::AcqRel),
            "resume of a device that was not paused",
        );
        self.resumes.fetch_add(1, Ordering::AcqRel);
    }
    fn is_quiesced(&self) -> bool {
        self.quiesces
    }
    fn halt(&self) {
        self.halted.store(true, Ordering::Release);
        while self.halt_wedges.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Counts the vCPU pauses the engine takes.
#[derive(Default)]
struct RecordingPause {
    pauses: AtomicUsize,
    resumes: AtomicUsize,
    fails: bool,
    trace: Option<Arc<Trace>>,
    /// Set when a pause found this lock held.
    topology: Option<TopologyLock>,
    paused_under_topology: AtomicBool,
}

impl VmPause for RecordingPause {
    fn pause(&self) -> anyhow::Result<()> {
        self.pauses.fetch_add(1, Ordering::AcqRel);
        if self.topology.as_ref().is_some_and(TopologyLock::is_held) {
            self.paused_under_topology.store(true, Ordering::Release);
        }
        if self.fails {
            anyhow::bail!("pause refused");
        }
        Trace::record(&self.trace, "vm-pause");
        Ok(())
    }
    fn resume(&self) -> anyhow::Result<()> {
        self.resumes.fetch_add(1, Ordering::AcqRel);
        Trace::record(&self.trace, "vm-resume");
        Ok(())
    }
}

/// Records the general purpose events the register file raises.
#[derive(Default)]
struct RecordingSink {
    raised: Mutex<Vec<GpeBit>>,
}

impl HotplugEventSink for RecordingSink {
    fn raise(&self, bit: GpeBit) {
        lock(&self.raised).push(bit);
    }
}

fn null_log() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

struct Fixture {
    core: Core,
    registry: Arc<DeviceRegistry>,
    bus: Arc<PciBus>,
    regs: Arc<PciHotplug>,
    vm: Arc<RecordingPause>,
    /// The same gate the engine pauses through, so a test can hold the
    /// pause the way an operator's `pause` command does.
    gate: Arc<VmPauseGate>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_pause(RecordingPause::default())
    }

    fn with_pause(pause: RecordingPause) -> Self {
        let registry = Arc::new(DeviceRegistry::new());
        let bus = PciBus::new();
        let regs =
            PciHotplug::new(Arc::new(RecordingSink::default()), null_log());
        let vm = Arc::new(pause);
        let gate = VmPauseGate::new(Arc::clone(&vm) as Arc<dyn VmPause>);
        Self {
            core: Core {
                registry: Arc::clone(&registry),
                bus: Arc::clone(&bus),
                regs: Arc::clone(&regs),
                vm: Arc::clone(&gate) as Arc<dyn VmPause>,
                // Short, so the abort path does not slow the suite.
                budget: Duration::from_millis(20),
                halt_slack: Duration::from_millis(20),
                closed: AtomicBool::new(false),
                drains: AtomicBool::new(true),
                topology: TopologyLock::default(),
                log: null_log(),
            },
            registry,
            bus,
            regs,
            vm,
            gate,
        }
    }

    /// A catalog that attaches a stub, and a counter of its calls.
    fn builder(
        &self,
        device: Arc<StubPci>,
        lifecycle: Option<Arc<StubLifecycle>>,
        calls: Arc<AtomicUsize>,
    ) -> impl FnMut(&str) -> anyhow::Result<CreatedPciDevice> + '_ {
        let bus = Arc::clone(&self.bus);
        move |spec: &str| {
            calls.fetch_add(1, Ordering::AcqRel);
            let bdf =
                parse_bdf(devspec::parts(spec).slot).expect("a parsed slot");
            // The boot-time catalogs attach through the chipset, which
            // is this call.
            bus.try_attach(bdf, device.clone() as Arc<dyn PciDevice>)?;
            Ok((
                Some(device.clone() as PciDeviceHandle),
                lifecycle.clone().map(|l| l as Arc<dyn Lifecycle>),
            ))
        }
    }

    fn add(&self, spec: &str) -> Result<String, HotplugError> {
        self.add_with(spec, StubLifecycle::new(true)).0
    }

    /// Add a device, and hand back the handles a test asserts on.
    fn add_with(
        &self,
        spec: &str,
        lifecycle: Arc<StubLifecycle>,
    ) -> (Result<String, HotplugError>, Arc<StubPci>, Arc<AtomicUsize>) {
        let device = Arc::new(StubPci::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut build = self.builder(
            Arc::clone(&device),
            Some(Arc::clone(&lifecycle)),
            Arc::clone(&calls),
        );
        let result = self.core.add(spec, &mut build);
        (result, device, calls)
    }

    /// The slot bits the guest would read out of PCIU.
    fn slot_up_bits(&self) -> u32 {
        let mut ro = vmm_core::common::ReadOp::new(4);
        self.regs.pio_rw(0, RWOp::Read(&mut ro));
        let buf = ro.buf();
        u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
    }

    /// The slot bits the guest would read out of PCID.
    fn slot_down_bits(&self) -> u32 {
        let mut ro = vmm_core::common::ReadOp::new(4);
        self.regs.pio_rw(4, RWOp::Read(&mut ro));
        let buf = ro.buf();
        u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
    }

    /// What a guest `_EJ0` method does: write the slot's B0EJ bit.
    fn guest_ejects(&self, slot: u8) {
        let bits = 1u32 << slot;
        let wo = WriteOp::from_buf(&bits.to_le_bytes());
        self.regs.pio_rw(8, RWOp::Write(&wo));
    }

    fn slot_is_empty(&self, slot: u8) -> bool {
        let bdf = Bdf::new(0, slot, 0).expect("a slot on the bus");
        self.bus.config_read(&bdf, 0, 4) == NO_DEVICE
    }
}

// ── Spec parsing ────────────────────────────────────────────────────

#[test]
fn the_hot_add_id_matches_the_boot_path() {
    // One device gets one id however it arrived, or an operator could
    // not name a hot-added device the way device-list reports it.
    assert_eq!(devspec::device_id("4,virtio-blk,/disk"), "virtio-blk@4");
    assert_eq!(devspec::device_id(" 4 ,virtio-blk"), "virtio-blk@4");
    assert_eq!(devspec::device_id("4"), "4");
}

#[test]
fn the_fixed_chipset_slots_are_refused() {
    for slot in [0u8, 1] {
        let spec = format!("{slot},virtio-blk,/disk");
        assert_eq!(
            hotplug_bdf(&spec),
            Err(HotplugError::FixedSlot(slot)),
            "slot {slot} must keep the chipset device it holds",
        );
    }
    for slot in 2..32u8 {
        let spec = format!("{slot},virtio-blk,/disk");
        assert!(hotplug_bdf(&spec).is_ok(), "slot {slot} is hot-pluggable");
    }
}

#[test]
fn an_address_the_register_file_cannot_describe_is_refused() {
    // PCIU, PCID and B0EJ hold one bit per slot of bus 0, and only
    // function 0 gets an _EJ0 method.
    for spec in ["1:4,virtio-blk,/d", "4:1,virtio-blk,/d", "32,virtio-blk"] {
        assert!(
            matches!(hotplug_bdf(spec), Err(HotplugError::BadSpec(_))),
            "'{spec}' must be refused",
        );
    }
}

#[test]
fn a_spec_without_a_driver_is_refused() {
    for spec in ["4", "4,", "4, "] {
        assert!(matches!(hotplug_bdf(spec), Err(HotplugError::BadSpec(_))));
    }
}

// ── Add ─────────────────────────────────────────────────────────────

#[test]
fn a_hot_add_registers_the_device_and_tells_the_guest() {
    let fixture = Fixture::new();

    let id = fixture.add("5,virtio-blk,/disk").expect("slot 5 is free");

    assert_eq!(id, "virtio-blk@5");
    let device = fixture.registry.get_by_id(&id).expect("registered");
    assert_eq!(device.bdf, Bdf::new(0, 5, 0));
    assert_eq!(device.spec.as_deref(), Some("5,virtio-blk,/disk"));
    assert_eq!(device.state, SlotState::Present);
    assert!(device.hotpluggable, "a boot device cannot be removed");
    assert!(!fixture.slot_is_empty(5), "the device is not on the bus");
    // The guest is told last, through PCIU.
    assert_eq!(fixture.slot_up_bits(), 1 << 5);
}

#[test]
fn a_fixed_slot_is_refused_before_anything_is_allocated() {
    // WHY: the build opens the backing file and takes a memory segment.
    // A refusal after that leaks both.
    let fixture = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut build =
        fixture.builder(Arc::new(StubPci::default()), None, Arc::clone(&calls));

    let err = fixture
        .core
        .add("1,virtio-blk,/disk", &mut build)
        .expect_err("slot 1 holds the LPC bridge");

    assert_eq!(err, HotplugError::FixedSlot(1));
    assert_eq!(calls.load(Ordering::Acquire), 0, "the catalog ran");
    assert!(fixture.registry.is_empty());
}

#[test]
fn an_occupied_slot_is_refused_before_anything_is_allocated() {
    let fixture = Fixture::new();
    fixture.add("5,virtio-blk,/first").expect("slot 5 is free");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut build =
        fixture.builder(Arc::new(StubPci::default()), None, Arc::clone(&calls));

    let err = fixture
        .core
        .add("5,nvme,/second", &mut build)
        .expect_err("slot 5 is taken");

    assert_eq!(
        err,
        HotplugError::SlotTaken(Bdf::new(0, 5, 0).expect("slot 5"))
    );
    assert_eq!(calls.load(Ordering::Acquire), 0, "the catalog ran");
}

#[test]
fn a_slot_the_chipset_holds_is_refused() {
    // The chipset attaches devices the registry never sees, so the
    // registry alone is not proof that a slot is free.
    let fixture = Fixture::new();
    let bdf = Bdf::new(0, 6, 0).expect("slot 6");
    fixture
        .bus
        .try_attach(bdf, Arc::new(StubPci::default()) as Arc<dyn PciDevice>)
        .expect("the bus is empty");

    let err = fixture
        .add("6,virtio-blk,/disk")
        .expect_err("the bus holds slot 6");

    assert_eq!(err, HotplugError::SlotTaken(bdf));
}

#[test]
fn a_duplicate_id_is_refused() {
    let fixture = Fixture::new();
    fixture
        .registry
        .insert(RegisteredDevice::new(
            "virtio-blk@5",
            None,
            None,
            None,
            None,
        ))
        .expect("a backend with no BDF");

    let err = fixture
        .add("5,virtio-blk,/disk")
        .expect_err("the id is taken");

    assert_eq!(err, HotplugError::DuplicateId("virtio-blk@5".to_string()));
}

#[test]
fn a_failed_build_leaves_nothing_behind() {
    let fixture = Fixture::new();
    let mut build = |_spec: &str| -> anyhow::Result<CreatedPciDevice> {
        anyhow::bail!("failed to open disk")
    };

    let err = fixture
        .core
        .add("5,virtio-blk,/missing", &mut build)
        .expect_err("the disk is missing");

    assert!(
        matches!(err, HotplugError::Create(ref m) if m.contains("open disk")),
        "got {err}",
    );
    assert!(fixture.registry.is_empty());
    assert!(fixture.slot_is_empty(5));
    assert_eq!(fixture.slot_up_bits(), 0, "the guest was told about it");
}

#[test]
fn a_driver_that_builds_no_pci_device_cannot_be_added() {
    // hostbridge and lpc, and any name the catalog does not know. A
    // record for a slot the guest reads as empty helps nobody.
    let fixture = Fixture::new();
    let mut build = |_spec: &str| Ok((None, None));

    let err = fixture
        .core
        .add("31,lpc", &mut build)
        .expect_err("the chipset already owns lpc");

    assert_eq!(err, HotplugError::NothingBuilt("lpc".to_string()));
    assert!(fixture.registry.is_empty());

    // A backend with no PCI face is refused for the same reason.
    let mut backend = |_spec: &str| {
        Ok((None, Some(StubLifecycle::new(true) as Arc<dyn Lifecycle>)))
    };
    assert!(fixture.core.add("5,virtio-blk,/d", &mut backend).is_err());
    assert!(fixture.registry.is_empty());
}

#[test]
fn a_rolled_back_add_stops_the_device_and_leaves_the_bus() {
    // The path a registry refusal takes. A device left on the bus with
    // no record would answer config reads that nothing can undo, and
    // its workers would hold the backing file open.
    let fixture = Fixture::new();
    let device = Arc::new(StubPci::default());
    let lifecycle = StubLifecycle::new(true);
    let bdf = Bdf::new(0, 5, 0).expect("slot 5");
    fixture
        .bus
        .try_attach(bdf, Arc::clone(&device) as Arc<dyn PciDevice>)
        .expect("the bus is empty");

    fixture.core.roll_back(
        bdf,
        Some(&(Arc::clone(&device) as PciDeviceHandle)),
        Some(&(Arc::clone(&lifecycle) as Arc<dyn Lifecycle>)),
    );

    assert!(fixture.slot_is_empty(5));
    assert_eq!(device.regions_detached.load(Ordering::Acquire), 1);
    assert!(
        lifecycle.halted.load(Ordering::Acquire),
        "the workers were left running"
    );
    assert_eq!(fixture.vm.pauses.load(Ordering::Acquire), 1);
    assert_eq!(fixture.vm.resumes.load(Ordering::Acquire), 1);
}

#[test]
fn the_vm_is_paused_around_the_region_unregister() {
    // WHY: a vCPU can be part-way through a BAR access on the device
    // that is leaving. Unregistering the region outside the pause races
    // that access against a bus map that no longer holds the handler.
    let trace = Arc::new(Trace::default());
    let fixture = Fixture::with_pause(RecordingPause {
        trace: Some(Arc::clone(&trace)),
        ..RecordingPause::default()
    });
    let device = Arc::new(StubPci::traced(&trace));
    let bdf = Bdf::new(0, 5, 0).expect("slot 5");
    fixture
        .bus
        .try_attach(bdf, Arc::clone(&device) as Arc<dyn PciDevice>)
        .expect("the bus is empty");

    fixture
        .core
        .detach_paused(bdf, Some(&(device as PciDeviceHandle)))
        .expect("the VM pauses");

    assert_eq!(trace.steps(), ["vm-pause", "detach-regions", "vm-resume"]);
    assert!(fixture.slot_is_empty(5));
}

/// A vCPU can be part-way through a BAR access on the device that is
/// leaving, so the detach needs the pause and must not proceed without.
#[test]
fn a_failed_pause_leaves_the_device_on_the_bus() {
    let fixture = Fixture::with_pause(RecordingPause {
        fails: true,
        ..RecordingPause::default()
    });
    let bdf = Bdf::new(0, 5, 0).expect("slot 5");
    fixture
        .bus
        .try_attach(bdf, Arc::new(StubPci::default()) as Arc<dyn PciDevice>)
        .expect("the bus is empty");

    fixture
        .core
        .detach_paused(bdf, None)
        .expect_err("a refused pause must refuse the detach");

    assert!(!fixture.slot_is_empty(5), "the device must keep its slot");
    // Resuming after a pause that never took would start the VM behind
    // an operator who paused it.
    assert_eq!(fixture.vm.resumes.load(Ordering::Acquire), 0);
}

/// The guest picks the moment it runs `_EJ0`. An operator pause inside
/// that window used to be answered `EALREADY` and reported as failed.
#[test]
fn an_operator_pause_and_an_eject_share_one_kernel_pause() {
    let trace = Arc::new(Trace::default());
    let fixture = Fixture::with_pause(RecordingPause {
        trace: Some(Arc::clone(&trace)),
        ..RecordingPause::default()
    });
    let id = fixture.add("5,virtio-blk,/added").expect("added");
    fixture.core.request_remove(&id).expect("requested");

    // The operator pauses, and holds it across the whole eject.
    fixture
        .gate
        .pause()
        .expect("the operator pause is accepted");
    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    assert!(fixture.slot_is_empty(5), "the eject must still finish");
    assert_eq!(fixture.vm.pauses.load(Ordering::Acquire), 1);
    // The VM stays stopped: the operator has not resumed it yet.
    assert_eq!(fixture.vm.resumes.load(Ordering::Acquire), 0);
    assert!(fixture.gate.is_held());

    fixture.gate.resume().expect("the operator resumes");
    assert_eq!(fixture.vm.resumes.load(Ordering::Acquire), 1);
    // One pause and one resume reached the kernel for the two holders.
    assert_eq!(trace.steps(), ["vm-pause", "vm-resume"]);
}

mod lifecycle;
mod remove;
