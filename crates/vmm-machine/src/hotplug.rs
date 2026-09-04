// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Run-time PCI hot-add and hot-remove.
//!
//! The register files in [`vmm_devices::hotplug`] run on a vCPU thread
//! inside an I/O exit, so they only record what the guest asked for.
//! This is the other half: one thread that owns every add and every
//! removal, so no teardown ever runs on a vCPU thread and no vCPU ever
//! blocks on a backing file.
//!
//! # Add
//!
//! An operator names a `-s` spec. The slot is checked before anything
//! is allocated, the device is built through the binary's own catalog,
//! and only then does the guest hear about it.
//!
//! # Remove
//!
//! A removal is a REQUEST, never an order. `request_remove` marks the
//! slot and asks the guest to run `_EJ0`; it tears nothing down and
//! returns at once. The guest answers when its driver has let go. A
//! guest that never answers leaves the slot in
//! [`SlotState::RemovePending`] for the life of the VM, which is the
//! correct outcome: the device is still in use. Nothing here forces a
//! removal, and no backing file is closed before the guest is done with
//! it.
//!
//! # Seams
//!
//! [`HotplugEngine::start`] takes the binary's catalog closure, the same
//! one the boot-time `-s` pass uses, so a control request can express
//! nothing that argv cannot. The PCI bus and the VM handle are read out
//! of the [`OwnedPciCtx`] rather than passed in, so the bus a device is
//! detached from is always the bus its catalog attached it to.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use slog::{debug, error, info, warn, Logger};

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MMIO_HOLE_BASE, MMIO_HOLE_END};
use vmm_devices::hotplug::mem::MAX_SLOTS as MEM_SLOTS;
use vmm_devices::hotplug::pci::{is_hotpluggable_slot, PciHotplug};
use vmm_devices::lifecycle::prepare_unplug;
use vmm_devices::pci::{Bdf, PciBus};
use vmm_devices::IndicatedState;

use crate::parse::parse_bdf;
use crate::pci::{
    CreatedPciDevice, OwnedPciCtx, PciDeviceCtx, PciDeviceHandle,
};
use crate::registry::{
    DeviceRegistry, RegisteredDevice, RegistryError, SlotState,
};
use crate::vcpus::VcpuRegistry;

use self::cpu::{CpuHotplugEngine, CpuHotplugError, CpuOnline};
use self::mem::{MemHotplugEngine, MemHotplugError, MemWindow};

/// How long a device gets to drain before an eject is abandoned.
///
/// A device that is still doing DMA must keep its slot, so the budget
/// expiring aborts the removal instead of forcing it.
const UNPLUG_BUDGET: Duration = Duration::from_secs(5);

/// How often the engine looks for an eject the guest has run.
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

/// A PCI config read of an empty slot: the bus float.
const NO_DEVICE: u32 = 0xFFFF_FFFF;

/// Why a hotplug request was refused.
///
/// Hand-written rather than derived: this crate carries no `thiserror`
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotplugError {
    /// The VM did not ask for the ACPI hotplug interface.
    Disabled,
    /// The spec is not `slot,driver[,config]`, or names an address a
    /// hot-add cannot use.
    BadSpec(String),
    /// Slot 0 is the host bridge and slot 1 is the LPC bridge. Neither
    /// gets an `_EJ0` method, so neither can be plugged.
    FixedSlot(u8),
    /// Something already holds the slot.
    SlotTaken(Bdf),
    /// Another device holds the id this spec would take.
    DuplicateId(String),
    /// The catalog refused the spec or could not build the device.
    Create(String),
    /// The catalog built no PCI device. `hostbridge` and `lpc` do that,
    /// because the chipset already owns them, and so does a driver name
    /// the catalog does not know.
    NothingBuilt(String),
    /// The registry refused the record.
    Registry(RegistryError),
    /// No device holds this id.
    NoSuchDevice(String),
    /// The device came from argv. Only a hot-added device can be
    /// removed, because only its slot was ever advertised as ejectable.
    NotHotpluggable(String),
    /// The slot is already on its way out.
    NotRemovable { id: String, state: SlotState },
}

impl fmt::Display for HotplugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotplugError::Disabled => {
                f.write_str("this VM was not started with --hotplug")
            }
            HotplugError::BadSpec(reason) => write!(f, "bad spec: {reason}"),
            HotplugError::FixedSlot(slot) => {
                write!(f, "PCI slot {slot} is fixed and cannot be plugged")
            }
            HotplugError::SlotTaken(bdf) => {
                write!(f, "PCI slot {bdf} is already in use")
            }
            HotplugError::DuplicateId(id) => {
                write!(f, "device id {id} is already registered")
            }
            HotplugError::Create(reason) => {
                write!(f, "cannot create the device: {reason}")
            }
            HotplugError::NothingBuilt(driver) => {
                write!(f, "driver {driver} builds no device to put in a slot")
            }
            HotplugError::Registry(e) => write!(f, "{e}"),
            HotplugError::NoSuchDevice(id) => {
                write!(f, "no device with id {id}")
            }
            HotplugError::NotHotpluggable(id) => {
                write!(f, "device {id} was not hot-added and cannot be removed")
            }
            HotplugError::NotRemovable { id, state } => {
                write!(f, "device {id} is {state}")
            }
        }
    }
}

impl std::error::Error for HotplugError {}

/// Stops every vCPU while the PCI bus changes shape.
///
/// A trait, not the handle itself, so the teardown order can be tested
/// without a live VM.
pub trait VmPause: Send + Sync {
    fn pause(&self) -> anyhow::Result<()>;
    fn resume(&self) -> anyhow::Result<()>;
}

impl VmPause for VmmHdl {
    fn pause(&self) -> anyhow::Result<()> {
        VmmHdl::pause(self).map_err(anyhow::Error::from)
    }

    fn resume(&self) -> anyhow::Result<()> {
        VmmHdl::resume(self).map_err(anyhow::Error::from)
    }
}

/// Builds one device from its spec. The engine's seam onto a binary's
/// catalog.
type BuildDevice<'a> = dyn FnMut(&str) -> anyhow::Result<CreatedPciDevice> + 'a;

/// The binary's catalog, as the engine holds it.
pub type HotplugFactory = Box<
    dyn FnMut(&str, &PciDeviceCtx<'_>) -> anyhow::Result<CreatedPciDevice>
        + Send,
>;

/// Everything an add or a removal touches, with no live VM in it.
///
/// Split out from [`HotplugEngine`] because [`OwnedPciCtx`] needs a
/// `Machine`, and this half must stay testable.
struct Core {
    registry: Arc<DeviceRegistry>,
    bus: Arc<PciBus>,
    regs: Arc<PciHotplug>,
    vm: Arc<dyn VmPause>,
    budget: Duration,
    log: Logger,
}

impl Core {
    /// Add one device, then tell the guest.
    ///
    /// `build` runs only after the slot is known to be free, so a
    /// refused request opens no file, allocates no memory segment and
    /// takes no MSI-X vector.
    fn add(
        &self,
        spec: &str,
        build: &mut BuildDevice<'_>,
    ) -> Result<String, HotplugError> {
        let bdf = hotplug_bdf(spec)?;
        let id = device_id(spec);
        self.check_free(&id, bdf)?;

        let (pci, lifecycle) =
            build(spec).map_err(|e| HotplugError::Create(format!("{e:#}")))?;
        // A slot holds a PCI device. Without one there is nothing for
        // the guest's device check to find, so the record would name a
        // slot that reads as empty.
        if pci.is_none() {
            // Dropping the handle does not join a worker, so a backend
            // that got as far as starting one would hold its backing
            // file open for the life of the VM.
            if let Some(lifecycle) = &lifecycle {
                if let Err(e) = prepare_unplug(lifecycle, self.budget) {
                    warn!(self.log, "a refused backend did not stop";
                        "spec" => spec, "error" => format!("{e:?}"));
                }
            }
            return Err(HotplugError::NothingBuilt(
                driver_of(spec).to_string(),
            ));
        }

        let mut record = RegisteredDevice::new(
            id.clone(),
            Some(bdf),
            pci.clone(),
            lifecycle.clone(),
            Some(spec.to_string()),
        );
        record.hotpluggable = true;
        if let Err(e) = self.registry.insert(record) {
            // A half-added device is worse than a refused one: roll the
            // whole thing back before the guest is ever told.
            warn!(self.log, "rolling back a hot-add the registry refused";
                "spec" => spec, "error" => %e);
            self.roll_back(bdf, pci.as_ref(), lifecycle.as_ref());
            return Err(HotplugError::Registry(e));
        }

        // Last: the guest must not find the device before the VMM has a
        // record of it.
        self.regs.notify_added(bdf.dev());
        info!(self.log, "device added"; "id" => &id, "bdf" => %bdf);
        Ok(id)
    }

    /// Ask the guest to give a device up. Tears nothing down.
    fn request_remove(&self, id: &str) -> Result<(), HotplugError> {
        let device = self
            .registry
            .get_by_id(id)
            .ok_or_else(|| HotplugError::NoSuchDevice(id.to_string()))?;
        let Some(bdf) = device.bdf.filter(|_| device.hotpluggable) else {
            return Err(HotplugError::NotHotpluggable(id.to_string()));
        };

        match device.state {
            SlotState::Present => self
                .registry
                .set_state(id, SlotState::Present, SlotState::RemovePending)
                .map_err(HotplugError::Registry)?,
            // The guest may have missed the first request, or its
            // driver may not have been bound yet. Asking again raises
            // the same event for the same slot and changes nothing
            // else.
            SlotState::RemovePending => {}
            state => {
                return Err(HotplugError::NotRemovable {
                    id: id.to_string(),
                    state,
                })
            }
        }

        self.regs.notify_removed(bdf.dev());
        info!(self.log, "device removal requested"; "id" => id, "bdf" => %bdf);
        Ok(())
    }

    /// Act on every eject the guest has run since the last pass.
    fn drain_ejects(&self) {
        for slot in self.regs.take_eject_requests() {
            self.eject(slot);
        }
    }

    /// Finish one eject the guest has already run.
    fn eject(&self, slot: u8) {
        let Some(bdf) = Bdf::new(0, slot, 0) else {
            return;
        };
        let Some(device) = self.registry.get_by_bdf(bdf) else {
            // The register file only reports slots it was told hold a
            // device, so this is a slot whose teardown already ran.
            debug!(self.log, "eject names an empty slot"; "slot" => slot);
            return;
        };

        if device.state != SlotState::RemovePending {
            // The guest ran `_EJ0` on its own. A device leaves only when
            // an operator asks, so put the slot back in front of the
            // guest instead of tearing it out.
            debug!(self.log, "refused an eject no operator asked for";
                "id" => &device.id, "state" => %device.state);
            self.regs.notify_added(slot);
            return;
        }

        let Some(lifecycle) = device.lifecycle.clone() else {
            // passthru has no Lifecycle, so nothing can report that its
            // DMA has stopped. It keeps its slot.
            warn!(self.log, "eject aborted: the device cannot be quiesced";
                "id" => &device.id);
            self.abort_eject(&device.id, slot, None);
            return;
        };

        // Read before the quiesce, not after: `prepare_unplug` pauses
        // the device itself unless it is paused already, so this is the
        // only point that can tell whose pause it is. Resuming one the
        // operator paused would start it behind them, and `Indicator`
        // panics on a resume from any state but `Pause`.
        let paused_here =
            lifecycle.lifecycle_state() != Some(IndicatedState::Pause);

        if let Err(e) = prepare_unplug(&lifecycle, self.budget) {
            // Never forced. The device keeps running, keeps its slot and
            // keeps its backing file. `prepare_unplug` already paused
            // it, so the abort has to undo that: a device left paused
            // never serves the guest again, which is a silent hang
            // rather than a refused removal.
            warn!(self.log, "eject aborted: the device did not quiesce";
                "id" => &device.id, "error" => format!("{e:?}"));
            self.abort_eject(
                &device.id,
                slot,
                paused_here.then_some(&lifecycle),
            );
            return;
        }

        // The compare-and-swap comes after the quiesce, not before it:
        // `SlotState` has no edge back out of `Ejecting`, so a slot
        // moved first could never be walked back when the quiesce
        // fails. It still guards the one step that cannot be undone,
        // which is the detach below.
        if let Err(e) = self.registry.set_state(
            &device.id,
            SlotState::RemovePending,
            SlotState::Ejecting,
        ) {
            error!(self.log, "eject stopped: the slot moved";
                "id" => &device.id, "error" => %e);
            return;
        }

        self.detach_paused(bdf, device.pci.as_ref());
        if let Err(e) = self.registry.set_state(
            &device.id,
            SlotState::Ejecting,
            SlotState::Absent,
        ) {
            error!(self.log, "eject finished on a slot that moved";
                "id" => &device.id, "error" => %e);
        }
        // Dropping the record drops the last handles, which closes the
        // backing file.
        self.registry.remove_by_id(&device.id);
        info!(self.log, "device removed"; "id" => &device.id, "bdf" => %bdf);
    }

    /// Give a device back after a removal that will not happen.
    ///
    /// The device keeps its slot, so three things have to be undone:
    /// the pause `prepare_unplug` took, the slot state the operator's
    /// request set, and the `ejectable` bit `take_eject_requests`
    /// cleared. Without the last one no later `_EJ0` would be accepted
    /// and the device could never be removed at all.
    fn abort_eject(
        &self,
        id: &str,
        slot: u8,
        resume: Option<&Arc<dyn vmm_devices::Lifecycle>>,
    ) {
        if let Some(lifecycle) = resume {
            lifecycle.resume();
        }
        if let Err(e) = self.registry.set_state(
            id,
            SlotState::RemovePending,
            SlotState::Present,
        ) {
            // Not fatal: the slot is reported as it is, and the device
            // is running either way.
            debug!(self.log, "an abandoned removal left the slot as it was";
                "id" => id, "error" => %e);
        }
        self.regs.notify_added(slot);
    }

    /// Refuse a slot or an id that is already in use.
    ///
    /// The bus is checked as well as the registry: the chipset attaches
    /// devices the registry never sees.
    fn check_free(&self, id: &str, bdf: Bdf) -> Result<(), HotplugError> {
        if self.registry.get_by_id(id).is_some() {
            return Err(HotplugError::DuplicateId(id.to_string()));
        }
        if self.registry.get_by_bdf(bdf).is_some() {
            return Err(HotplugError::SlotTaken(bdf));
        }
        if self.bus.config_read(&bdf, 0, 4) != NO_DEVICE {
            return Err(HotplugError::SlotTaken(bdf));
        }
        Ok(())
    }

    /// Undo a hot-add that got as far as building the device.
    ///
    /// The workers are stopped before the bus handles go, so the last
    /// reference really is the last one and the backing file closes.
    fn roll_back(
        &self,
        bdf: Bdf,
        pci: Option<&PciDeviceHandle>,
        lifecycle: Option<&Arc<dyn vmm_devices::Lifecycle>>,
    ) {
        if let Some(lifecycle) = lifecycle {
            if let Err(e) = prepare_unplug(lifecycle, self.budget) {
                // The device is new and the guest was never told about
                // it, so nothing is using it. Report it and carry on:
                // leaving it on the bus with no record is worse.
                warn!(self.log, "a rolled-back device did not quiesce";
                    "bdf" => %bdf, "error" => format!("{e:?}"));
            }
        }
        self.detach_paused(bdf, pci);
    }

    /// Take a device off the bus with no vCPU inside a bus handler.
    ///
    /// The pause is what makes the region unregister safe: a vCPU can
    /// be part-way through a BAR access on the device that is leaving.
    /// Only a pause this call took is resumed, so a VM the operator
    /// stopped is never started again from here.
    fn detach_paused(&self, bdf: Bdf, pci: Option<&PciDeviceHandle>) {
        let paused = match self.vm.pause() {
            Ok(()) => true,
            Err(e) => {
                // Carry on: the guest has already let the device go, so
                // leaving it half attached is worse than a racy detach.
                error!(self.log, "failed to pause the VM for a bus change";
                    "bdf" => %bdf, "error" => format!("{e:#}"));
                false
            }
        };

        if let Some(device) = pci {
            device.detach_regions();
        }
        // The bus hands its handle back. Dropping it here is what
        // releases the reference the bus held.
        drop(self.bus.detach(&bdf));

        if paused {
            if let Err(e) = self.vm.resume() {
                error!(self.log, "failed to resume the VM after a bus change";
                    "bdf" => %bdf, "error" => format!("{e:#}"));
            }
        }
    }
}

/// The add and remove engine, and the thread that owns teardown.
pub struct HotplugEngine {
    core: Arc<Core>,
    ctx: Arc<OwnedPciCtx>,
    /// The binary's catalog. `Mutex` because it is `FnMut` and two
    /// control connections can ask at once.
    factory: Mutex<HotplugFactory>,
    stop: Arc<Stopper>,
    thread: Mutex<Option<JoinHandle<()>>>,
    log: Logger,
}

impl HotplugEngine {
    /// Start the engine and its drain thread.
    ///
    /// `factory` must be the same catalog closure the boot-time `-s`
    /// pass uses, so a control request can express nothing that argv
    /// cannot.
    pub fn start(
        registry: Arc<DeviceRegistry>,
        ctx: Arc<OwnedPciCtx>,
        pci_regs: Arc<PciHotplug>,
        factory: HotplugFactory,
        log: Logger,
    ) -> Arc<Self> {
        // The bus and the VM handle come out of the context the catalog
        // builds against, so a device can never be detached from a bus
        // other than the one it was attached to.
        let (bus, vm) = {
            let borrowed = PciDeviceCtx::borrow_from(&ctx);
            (
                Arc::clone(borrowed.chipset.pci_bus()),
                Arc::clone(borrowed.vmm_hdl) as Arc<dyn VmPause>,
            )
        };

        let core = Arc::new(Core {
            registry,
            bus,
            regs: pci_regs,
            vm,
            budget: UNPLUG_BUDGET,
            log: log.clone(),
        });
        let stop = Arc::new(Stopper::default());
        let thread = spawn_drain(&core, &stop, &log);

        Arc::new(Self {
            core,
            ctx,
            factory: Mutex::new(factory),
            stop,
            thread: Mutex::new(thread),
            log,
        })
    }

    /// Add one `slot,driver[,config]` device and return its id.
    pub fn add_device(&self, spec: &str) -> Result<String, HotplugError> {
        // No panic on a control path: the factory is a closure, and a
        // panic inside it cannot leave a half-written value behind.
        let mut factory = lock(&self.factory);
        let ctx = PciDeviceCtx::borrow_from(&self.ctx);
        self.core.add(spec, &mut |spec| factory(spec, &ctx))
    }

    /// Ask the guest to give a device up. Returns as soon as the guest
    /// has been asked.
    pub fn request_remove(&self, id: &str) -> Result<(), HotplugError> {
        self.core.request_remove(id)
    }

    /// Stop the drain thread and wait for it.
    pub fn shutdown(&self) {
        self.stop.stop();
        let handle = lock(&self.thread).take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                error!(self.log, "the hotplug thread panicked");
            }
        }
    }
}

impl Drop for HotplugEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_drain(
    core: &Arc<Core>,
    stop: &Arc<Stopper>,
    log: &Logger,
) -> Option<JoinHandle<()>> {
    // Weak, so an engine that is dropped without a shutdown still ends
    // its thread.
    let core = Arc::downgrade(core);
    let stop = Arc::clone(stop);
    let spawned = thread::Builder::new()
        .name("hotplug".into())
        .spawn(move || drain_loop(&core, &stop));
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            // The VM keeps running: an add still works, and a removal
            // waits in the register file instead of being forced.
            error!(log, "no hotplug thread; ejects will not be drained";
                "error" => %e);
            None
        }
    }
}

fn drain_loop(core: &Weak<Core>, stop: &Stopper) {
    while !stop.wait(DRAIN_INTERVAL) {
        let Some(core) = core.upgrade() else {
            return;
        };
        core.drain_ejects();
    }
}

/// The drain thread's shutdown flag, with the wait that reads it.
#[derive(Default)]
struct Stopper {
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl Stopper {
    fn stop(&self) {
        *lock(&self.stopped) = true;
        self.changed.notify_all();
    }

    /// Wait up to `timeout`, and report whether the engine must stop.
    fn wait(&self, timeout: Duration) -> bool {
        let stopped = lock(&self.stopped);
        if *stopped {
            return true;
        }
        match self.changed.wait_timeout(stopped, timeout) {
            Ok((stopped, _)) => *stopped,
            // A poisoned wait still carries the flag, and the flag is
            // the whole state.
            Err(e) => *e.into_inner().0,
        }
    }
}

/// Take a lock, tolerating poison.
///
/// Every value behind these locks is one field that a panic elsewhere
/// cannot leave half written, and a hotplug request must not take the
/// VM down.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The address a hot-add spec names, if a hot-add can use it.
///
/// Rejects everything the PCI hotplug register file cannot describe:
/// its registers hold one bit per slot on bus 0, and the AML gives an
/// `_EJ0` method to function 0 alone.
fn hotplug_bdf(spec: &str) -> Result<Bdf, HotplugError> {
    let slot = spec_slot(spec);
    if driver_of(spec).is_empty() {
        return Err(HotplugError::BadSpec(format!(
            "'{spec}' needs slot,driver[,config]"
        )));
    }
    let bdf = parse_bdf(slot).ok_or_else(|| {
        HotplugError::BadSpec(format!("'{slot}' is not a PCI address"))
    })?;
    if bdf.bus() != 0 || bdf.func() != 0 {
        return Err(HotplugError::BadSpec(format!(
            "hot-add takes bus 0 function 0, not '{slot}'"
        )));
    }
    if !is_hotpluggable_slot(bdf.dev()) {
        return Err(HotplugError::FixedSlot(bdf.dev()));
    }
    Ok(bdf)
}

/// The leading `slot` field of a `slot,driver[,config]` spec.
fn spec_slot(spec: &str) -> &str {
    spec.split(',').next().unwrap_or("").trim()
}

/// The `driver` field, or an empty string.
fn driver_of(spec: &str) -> &str {
    spec.split(',').nth(1).map(str::trim).unwrap_or("")
}

/// Stable, operator-facing id for a spec.
///
/// The same `driver@slot` rule the boot-time attach pass uses, so one
/// device has one id however it arrived.
fn device_id(spec: &str) -> String {
    let slot = spec_slot(spec);
    match driver_of(spec) {
        "" => slot.to_string(),
        driver => format!("{driver}@{slot}"),
    }
}

/// The `-s` specs a restart has to replay.
///
/// Only hot-added devices: an argv device already has its own `-s`
/// entry, complete with the bootindex the registry does not keep. A
/// slot on its way out is dropped, because the device is going away.
pub fn hotplug_specs(registry: &DeviceRegistry) -> Vec<String> {
    registry
        .list()
        .into_iter()
        .filter(|device| {
            device.hotpluggable && device.state != SlotState::Absent
        })
        .filter_map(|device| device.spec)
        .collect()
}

/// The CPU hot-add engine and the CPU set it adds to.
///
/// The engine answers an add and nothing else, so a control plane that
/// has to report which CPUs are running reads the set. Both are held
/// here, so no caller can hold one without the other and describe a
/// machine that does not exist.
///
/// The set is the [`CpuOnline`] trait and not [`VcpuRegistry`] itself,
/// for the reason the engine gives: every step of a real add is an
/// ioctl, so a control plane can only be tested against a stand-in.
#[derive(Clone)]
pub struct CpuSlots {
    engine: Arc<CpuHotplugEngine>,
    vcpus: Arc<dyn CpuOnline>,
}

impl CpuSlots {
    pub fn new(
        engine: Arc<CpuHotplugEngine>,
        vcpus: Arc<dyn CpuOnline>,
    ) -> Self {
        Self { engine, vcpus }
    }

    /// The same, over the registry a running VM has.
    pub fn from_registry(
        engine: Arc<CpuHotplugEngine>,
        vcpus: Arc<VcpuRegistry>,
    ) -> Self {
        Self::new(engine, vcpus as Arc<dyn CpuOnline>)
    }

    /// Bring one CPU online on the running VM.
    pub fn add(&self, id: u32) -> Result<(), CpuHotplugError> {
        self.engine.add_cpu(id)
    }

    /// How many CPUs the boot path brought online, ids 0 upward.
    pub fn boot_cpus(&self) -> u32 {
        self.vcpus.boot_cpus()
    }

    /// Every CPU slot the tables describe, boot CPUs included.
    pub fn max_cpus(&self) -> u32 {
        self.vcpus.possible_cpus()
    }

    /// Whether the CPU in `id` is running.
    pub fn is_online(&self, id: u32) -> bool {
        self.vcpus.is_online(id)
    }

    /// The ids a failed add spent. There is no `vm_deactivate_cpu`, so
    /// no later add can take one back.
    pub fn consumed(&self) -> Vec<u32> {
        self.engine.consumed_cpus()
    }

    fn shutdown(&self) {
        self.engine.shutdown();
    }
}

/// Every hot-add engine one VM has.
///
/// `None` means the VM was started without the option that engine
/// needs, so a request for it is refused and never queued. The two
/// binaries build this the same way, which is why it lives here and
/// not in either control plane.
#[derive(Clone, Default)]
pub struct HotplugEngines {
    /// PCI add and remove. Needs `--hotplug`.
    pub pci: Option<Arc<HotplugEngine>>,
    /// CPU add. Needs `--hotplug` and a `maxcpus` above the boot count.
    pub cpu: Option<CpuSlots>,
    /// Memory add. Needs `--hotplug` and `-o hotplug.maxmem`.
    pub mem: Option<Arc<MemHotplugEngine>>,
}

impl HotplugEngines {
    /// Stop every drain thread and wait for it.
    ///
    /// Called at teardown. The control plane thread is never joined, so
    /// a request can still arrive after this: a CPU add then finds the
    /// vCPU roster closed and is refused.
    pub fn shutdown(&self) {
        if let Some(pci) = self.pci.as_ref() {
            pci.shutdown();
        }
        if let Some(cpu) = self.cpu.as_ref() {
            cpu.shutdown();
        }
        if let Some(mem) = self.mem.as_ref() {
            mem.shutdown();
        }
    }
}

/// The guest address window that hot-added memory goes in.
///
/// It starts above every RAM region the VM booted with. The machine
/// builder maps `[0, min(mem, MMIO_HOLE_BASE))` and, for a bigger
/// guest, `[MMIO_HOLE_END, MMIO_HOLE_END + mem - MMIO_HOLE_BASE)`, so
/// the first free address is the top of that second range. The window
/// is therefore always above 4 GiB and can never land in the 32-bit
/// MMIO hole.
///
/// `slot_size` has to divide `max_mem`, and the quotient has to fit the
/// [`MEM_SLOTS`] slots the register file and the AML describe. Both are
/// refused here rather than silently clamped, because a window that
/// claims more slots than the guest has would report free memory no add
/// can reach.
///
/// The base is computed from `mem_size` because this runs before the VM
/// exists. If that layout ever changed, the first add would be REFUSED
/// and not misplaced: `PhysMap::claim_ram` checks the range against
/// every published region before any ioctl runs.
///
/// The kernel puts a second, lower ceiling on this: one VM gets five
/// memory segments (`vmm.c:179`) and boot spends some of them, so the
/// real number of adds can be below the slot count. The engine reports
/// that as it happens, because what boot spent depends on the devices.
pub fn hot_add_window(
    mem_size: usize,
    max_mem: u64,
    slot_size: u64,
) -> Result<MemWindow, MemHotplugError> {
    let bad = |reason: String| Err(MemHotplugError::BadWindow(reason));

    if slot_size == 0 {
        return bad("a slot size of 0 describes no memory".to_string());
    }
    let Ok(mem_size) = u64::try_from(mem_size) else {
        return bad(format!("{mem_size} bytes of RAM does not fit a u64"));
    };
    let highmem = mem_size.saturating_sub(MMIO_HOLE_BASE);
    let Some(base) = MMIO_HOLE_END.checked_add(highmem) else {
        return bad(format!("{mem_size} bytes of RAM leaves no window"));
    };
    if !max_mem.is_multiple_of(slot_size) {
        return bad(format!(
            "a window of {max_mem:#x} does not divide into {slot_size:#x} slots",
        ));
    }
    let slots = max_mem / slot_size;
    if slots > MEM_SLOTS as u64 {
        return bad(format!(
            "a {max_mem:#x} window of {slot_size:#x} slots needs {slots} \
             slots, and the guest has {MEM_SLOTS}; raise the slot size",
        ));
    }

    MemWindow::new(base, max_mem, slot_size)
}

pub mod cpu;
pub mod mem;

#[cfg(test)]
mod tests;

/// Tests for the pieces above that the disk engine's own module does
/// not cover. Kept inline so `hotplug/tests.rs` stays under the file
/// size this tree holds itself to.
#[cfg(test)]
mod window_tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const SLOT: u64 = 128 * 1024 * 1024;

    #[test]
    fn the_window_starts_above_every_ram_region_the_vm_booted_with() {
        // A window that overlapped high RAM would map a second segment
        // over memory the guest is already using.
        let small = hot_add_window(1024 * 1024 * 1024, GIB, SLOT)
            .expect("1 GiB of RAM is all low memory");
        assert_eq!(small.base(), MMIO_HOLE_END);

        // 4 GiB of RAM is 3 GiB low and 1 GiB high, so high RAM ends at
        // 5 GiB and the window starts there.
        let big = hot_add_window(4 * 1024 * 1024 * 1024, GIB, SLOT)
            .expect("4 GiB of RAM has a high half");
        assert_eq!(big.base(), MMIO_HOLE_END + GIB);
    }

    #[test]
    fn the_window_never_reaches_the_mmio_hole() {
        for gib in [1u64, 3, 4, 8, 64] {
            let window = hot_add_window((gib * GIB) as usize, GIB, SLOT)
                .unwrap_or_else(|e| panic!("{gib} GiB: {e}"));
            assert!(
                window.base() >= MMIO_HOLE_END,
                "{gib} GiB put the window at {:#x}",
                window.base(),
            );
        }
    }

    #[test]
    fn a_window_the_guest_has_no_slots_for_is_refused() {
        // 8 GiB of 128 MiB slots wants 64 slots and the AML declares 8.
        // Clamping instead would report free memory no add can reach.
        let refused =
            hot_add_window(GIB as usize, 8 * GIB, SLOT).expect_err("64 slots");
        assert!(
            refused.to_string().contains("raise the slot size"),
            "{refused}",
        );

        // The same window in slots the guest has.
        let window = hot_add_window(GIB as usize, 8 * GIB, GIB)
            .expect("8 slots of 1 GiB");
        assert_eq!(window.slots(), 8);
    }

    #[test]
    fn a_window_the_slot_size_does_not_divide_is_refused() {
        let refused = hot_add_window(GIB as usize, GIB + 1, SLOT)
            .expect_err("not a whole number of slots");
        assert!(refused.to_string().contains("does not divide"), "{refused}");

        assert!(hot_add_window(GIB as usize, GIB, 0).is_err());
    }

    #[test]
    fn engines_a_vm_did_not_ask_for_are_absent() {
        // The default. Every control plane refuses a request rather
        // than queueing it for an engine that does not exist.
        let engines = HotplugEngines::default();

        assert!(engines.pci.is_none());
        assert!(engines.cpu.is_none());
        assert!(engines.mem.is_none());
        // A shutdown of nothing must be quiet, because teardown runs it
        // on every VM.
        engines.shutdown();
    }
}
