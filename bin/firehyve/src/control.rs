// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! firehyve's answer to the vsock CONTROL verb.
//!
//! firehyve has no control socket, so the inventory an operator needs
//! is served on the vsock host socket instead. The grammar lives in
//! `vmm_virtio::vsock::control`; this file supplies the commands:
//!
//! ```text
//! device-list                  -> OK 2 virtio-blk@4,0.4.0,present ...
//! cpu-list                     -> OK 4 0,present,boot 2,absent,hotplug ...
//! mem-list                     -> OK 1 boot,1073741824,present
//! device-add 5,virtio-blk,/d   -> OK virtio-blk@5
//! device-remove virtio-blk@5   -> OK virtio-blk@5,remove-pending
//! cpu-add 2                    -> OK 2,present
//! mem-add 134217728            -> OK slot0,present
//! ```
//!
//! `cpu-list` gives one record per CPU slot, `id,state,kind`. `kind`
//! separates a boot CPU from a slot a hot-add can take, and `state` is
//! `present`, `absent`, or `consumed` for an id a failed add spent.
//!
//! `mem-list` gives `key,value,state` records. Without a window that is
//! the one boot record. With one it also reports `window`, `slot-size`,
//! `slot-count`, `slots-used` and `added`, so an operator can see how
//! much of the window is left. `value` is a byte count except in the
//! two slot counts.
//!
//! `mem-add` takes BYTES, not the `-m` size grammar, so it means the
//! same thing on both transports. The engine rounds up to a whole slot.
//!
//! # Add only
//!
//! There is no `cpu-remove` and no `mem-remove` that does anything.
//! illumos has no `vm_deactivate_cpu` and no `VM_FREE_MEMSEG`, so
//! neither removal is possible at the kernel boundary. Both verbs are
//! recognised and refused with that reason, because an operator will
//! try them and "unknown command" does not say why.
//!
//! # Trust
//!
//! SECURITY: `device-add` opens a host file the peer names, which is
//! the same file-open authority argv has. It is not wider: the spec
//! goes through the same catalog closure the `-s` pass uses, so no
//! driver and no path becomes reachable that a command line could not
//! express. Nothing here authorises the peer. The only thing that does
//! is the host socket itself, which `vmm_core::unixsock` binds 0600
//! inside a 0700 directory, so the peer is already this process's own
//! user. rshyve, which has a control socket, adds a uid and zoneid
//! peercred check on top.

use std::collections::BTreeSet;
use std::sync::Arc;

use vmm_machine::{CpuSlots, DeviceRegistry, HotplugEngines, HotplugError};
use vmm_virtio::vsock::control::ControlSink;

/// Longest value written back for one field. An id comes from the `-s`
/// arguments, so an operator can make it long; the response line stays
/// readable either way.
const FIELD_MAX: usize = 64;

/// Longest refusal written back. An error can quote a path the peer
/// sent, so the line stays bounded.
const REASON_MAX: usize = 200;

/// Placeholder for a device that is not on the PCI bus.
const NO_BDF: &str = "-";

/// Why a CPU request was refused on a VM with no CPU slots.
///
/// Two starts give a VM no engine: no `--hotplug`, and a `maxcpus` that
/// is not above the boot count. The answer names both, because the
/// operator has to fix whichever one applies.
const CPU_HOTPLUG_OFF: &str =
    "CPU hot-add needs --hotplug and -c cpus=N,maxcpus=M";

/// The same for memory.
const MEM_HOTPLUG_OFF: &str =
    "memory hot-add needs --hotplug and -o hotplug.maxmem=SIZE";

/// Why `cpu-remove` cannot exist on this platform.
const NO_CPU_REMOVE: &str = "cpu-remove is not supported: illumos has no \
     vm_deactivate_cpu, so an active vCPU cannot be taken back";

/// Why `mem-remove` cannot exist on this platform.
const NO_MEM_REMOVE: &str = "mem-remove is not supported: illumos has no \
     VM_FREE_MEMSEG, so a memory segment lives until the VM does";

/// The machine, as the control commands see it.
pub struct Inventory {
    registry: Arc<DeviceRegistry>,
    /// Each engine is `None` unless the VM was started with the option
    /// it needs, which is the only way the guest has an interface to
    /// answer on.
    hotplug: HotplugEngines,
    num_cpus: u32,
    /// Every CPU slot the MADT describes. Equal to `num_cpus` unless
    /// `-c maxcpus=` asked for more.
    max_cpus: u32,
    mem_size: usize,
}

impl Inventory {
    pub fn new(
        registry: Arc<DeviceRegistry>,
        hotplug: HotplugEngines,
        num_cpus: u32,
        max_cpus: u32,
        mem_size: usize,
    ) -> Self {
        Self {
            registry,
            hotplug,
            num_cpus,
            max_cpus,
            mem_size,
        }
    }

    /// Build one device and report the id `device-remove` takes.
    fn device_add(&self, spec: &str) -> String {
        let Some(engine) = self.hotplug.pci.as_ref() else {
            return refused(&HotplugError::Disabled.to_string());
        };
        match engine.add_device(spec) {
            Ok(id) => format!("OK {}", field(&id)),
            Err(e) => refused(&e.to_string()),
        }
    }

    /// Ask the guest to give a device up.
    ///
    /// The answer says the request was recorded, not that the device is
    /// gone: only the guest can finish an eject, and a guest that never
    /// runs `_EJ0` leaves the slot pending for the life of the VM.
    fn device_remove(&self, id: &str) -> String {
        let Some(engine) = self.hotplug.pci.as_ref() else {
            return refused(&HotplugError::Disabled.to_string());
        };
        match engine.request_remove(id) {
            Ok(()) => format!(
                "OK {},{}",
                field(id),
                field(&vmm_machine::SlotState::RemovePending.to_string()),
            ),
            Err(e) => refused(&e.to_string()),
        }
    }

    fn device_list(&self) -> String {
        let devices = self.registry.list();
        let records = devices.iter().map(|device| {
            let bdf = device
                .bdf
                .map_or_else(|| NO_BDF.to_string(), |bdf| bdf.to_string());
            format!(
                "{},{},{}",
                field(&device.id),
                field(&bdf),
                field(&device.state.to_string()),
            )
        });
        ok_list(devices.len(), records)
    }

    /// Bring one CPU online, and report the id that is now running.
    ///
    /// The argument is read before the engine is looked up, so a typo
    /// is answered as a typo whatever the VM was started with. It is
    /// never used as an index: the engine range-checks it as well.
    fn cpu_add(&self, id: &str) -> String {
        let Ok(id) = id.parse::<u32>() else {
            return refused(&format!("{} is not a CPU id", field(id)));
        };
        let Some(cpus) = self.hotplug.cpu.as_ref() else {
            return refused(CPU_HOTPLUG_OFF);
        };
        match cpus.add(id) {
            Ok(()) => format!("OK {id},present"),
            Err(e) => refused(&e.to_string()),
        }
    }

    /// Give the guest more memory, and report the slot it went in.
    ///
    /// The size is BYTES. The engine rounds it up to a whole slot.
    fn mem_add(&self, bytes: &str) -> String {
        let Ok(bytes) = bytes.parse::<u64>() else {
            return refused(&format!(
                "{} is not a size in bytes",
                field(bytes),
            ));
        };
        let Some(engine) = self.hotplug.mem.as_ref() else {
            return refused(MEM_HOTPLUG_OFF);
        };
        match engine.add_memory(bytes) {
            Ok(slot) => format!("OK slot{slot},present"),
            Err(e) => refused(&e.to_string()),
        }
    }

    /// One record per CPU slot: `id,state,kind`.
    ///
    /// Every slot the MADT describes is listed, whether or not this VM
    /// can fill it: hiding the spare slots would contradict what the
    /// guest reads. `cpu-add` is what says whether one can be filled.
    fn cpu_list(&self) -> String {
        let cpus = self.hotplug.cpu.as_ref();
        // With an engine, its ceiling is the one an add is checked
        // against. The two agree: both come from `Cli::max_cpus`, which
        // is already capped at the kernel's 64.
        let max_cpus = cpus.map_or(self.max_cpus, CpuSlots::max_cpus);
        let boot_cpus = cpus.map_or(self.num_cpus, CpuSlots::boot_cpus);
        // A set, never a slice indexed by an id. The ids come from the
        // engine and are bounded by the kernel's 64 CPUs, but nothing
        // here has to trust that.
        let consumed: BTreeSet<u32> = cpus
            .map(CpuSlots::consumed)
            .unwrap_or_default()
            .into_iter()
            .collect();

        let records = (0..max_cpus).map(|id| {
            let state = if consumed.contains(&id) {
                "consumed"
            } else if cpus.map_or(id < boot_cpus, |cpus| cpus.is_online(id)) {
                "present"
            } else {
                "absent"
            };
            let kind = if id < boot_cpus { "boot" } else { "hotplug" };
            format!("{id},{state},{kind}")
        });
        ok_list(max_cpus as usize, records)
    }

    /// The boot memory, and the hot-add window when the VM has one.
    fn mem_list(&self) -> String {
        let boot = format!("boot,{},present", self.mem_size);
        let Some(engine) = self.hotplug.mem.as_ref() else {
            return ok_list(1, [boot].into_iter());
        };

        let mut records = vec![boot];
        records.extend(window_records(
            &engine.window(),
            engine.slots_used(),
            engine.bytes_added(),
        ));
        ok_list(records.len(), records.into_iter())
    }
}

/// What `mem-list` says about the hot-add window.
///
/// Split from the engine so the wire shape can be tested: every part of
/// a real engine is an ioctl on a live VM.
fn window_records(
    window: &vmm_machine::MemWindow,
    slots_used: usize,
    added: u64,
) -> [String; 5] {
    [
        format!("window,{},present", window.size()),
        format!("slot-size,{},present", window.slot_size()),
        format!("slot-count,{},present", window.slots()),
        format!("slots-used,{slots_used},present"),
        format!("added,{added},present"),
    ]
}

impl ControlSink for Inventory {
    fn handle_line(&self, line: &str) -> String {
        let mut fields = line.split_whitespace();
        // Read all three up front: a match guard cannot advance the
        // iterator. A `-s` spec carries no space, so one field is a
        // whole spec and a second means the request is malformed.
        let verb = fields.next().unwrap_or("");
        let argument = fields.next();
        let extra = fields.next();

        match (verb, argument, extra) {
            ("device-list", None, _) => self.device_list(),
            ("cpu-list", None, _) => self.cpu_list(),
            ("mem-list", None, _) => self.mem_list(),
            ("device-add", Some(spec), None) => self.device_add(spec),
            ("device-remove", Some(id), None) => self.device_remove(id),
            ("cpu-add", Some(id), None) => self.cpu_add(id),
            ("mem-add", Some(bytes), None) => self.mem_add(bytes),
            // Recognised so the answer says why, rather than reading as
            // a typo the operator can fix.
            ("cpu-remove", _, _) => refused(NO_CPU_REMOVE),
            ("mem-remove", _, _) => refused(NO_MEM_REMOVE),
            ("", _, _) => "ERR empty request".to_string(),
            ("device-list" | "cpu-list" | "mem-list", Some(_), _) => {
                format!("ERR {} takes no argument", field(verb))
            }
            (
                "device-add" | "device-remove" | "cpu-add" | "mem-add",
                None,
                _,
            ) => format!("ERR {} needs one argument", field(verb)),
            (
                "device-add" | "device-remove" | "cpu-add" | "mem-add",
                Some(_),
                Some(_),
            ) => format!("ERR {} takes one argument", field(verb)),
            _ => format!("ERR unknown command {}", field(verb)),
        }
    }
}

/// `OK <count>` and then one field per record.
fn ok_list(count: usize, records: impl Iterator<Item = String>) -> String {
    let mut out = format!("OK {count}");
    for record in records {
        out.push(' ');
        out.push_str(&record);
    }
    out
}

/// One refusal line, safe to put on the wire.
///
/// Takes text, not one error type: the three engines have three error
/// types and every one of them can quote a value the peer sent. So the
/// text is capped and stripped down to printable ASCII. Spaces stay:
/// the grammar makes a refusal one reason, not a field list.
fn refused(reason: &str) -> String {
    let cleaned: String = reason
        .chars()
        .take(REASON_MAX)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("ERR {cleaned}")
}

/// One field value, safe to put on the wire.
///
/// The grammar separates fields by spaces and values by commas, and an
/// operator reads the answer on a terminal. So an id that came from the
/// command line keeps only plain characters, and the length is capped.
fn field(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .take(FIELD_MAX)
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '@' | ':')
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        NO_BDF.to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use vmm_machine::{
        CpuHotplugEngine, CpuOnline, DeviceRegistry, RegisteredDevice,
        VcpuError,
    };

    use super::*;

    /// A machine with no hot-add engine at all. The default: without
    /// `--hotplug` the guest has no interface to answer on.
    fn inventory(registry: DeviceRegistry) -> Inventory {
        Inventory::new(
            Arc::new(registry),
            HotplugEngines::default(),
            2,
            2,
            1_073_741_824,
        )
    }

    /// A stand-in CPU set. Every step of a real add is an ioctl, so a
    /// control plane can only be tested against one of these.
    struct FakeCpus {
        possible: u32,
        boot: u32,
        online: Mutex<BTreeSet<u32>>,
        refuse: Option<VcpuError>,
    }

    impl FakeCpus {
        fn new(possible: u32, boot: u32) -> Arc<Self> {
            Arc::new(Self {
                possible,
                boot,
                online: Mutex::new((0..boot).collect()),
                refuse: None,
            })
        }

        fn refusing(possible: u32, boot: u32, error: VcpuError) -> Arc<Self> {
            Arc::new(Self {
                possible,
                boot,
                online: Mutex::new((0..boot).collect()),
                refuse: Some(error),
            })
        }

        fn online(&self) -> std::sync::MutexGuard<'_, BTreeSet<u32>> {
            self.online.lock().expect("test lock")
        }
    }

    impl CpuOnline for FakeCpus {
        fn possible_cpus(&self) -> u32 {
            self.possible
        }

        fn boot_cpus(&self) -> u32 {
            self.boot
        }

        fn is_online(&self, id: u32) -> bool {
            self.online().contains(&id)
        }

        fn add_vcpu(&self, id: u32) -> Result<(), VcpuError> {
            if let Some(error) = self.refuse.clone() {
                return Err(error);
            }
            self.online().insert(id);
            Ok(())
        }
    }

    /// Discards every general purpose event. The tests here read the
    /// answer on the wire, not the guest's SCI.
    struct SilentSink;

    impl vmm_devices::acpi_gpe::HotplugEventSink for SilentSink {
        fn raise(&self, _bit: vmm_devices::acpi_gpe::GpeBit) {}
    }

    fn cpu_slots(cpus: Arc<FakeCpus>) -> CpuSlots {
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let regs = vmm_devices::hotplug::cpu::CpuHotplug::new(
            cpus.possible_cpus(),
            Arc::new(SilentSink),
            log.clone(),
        );
        regs.set_boot_cpus(cpus.boot_cpus());
        let engine = CpuHotplugEngine::start_online(
            Arc::clone(&cpus) as Arc<dyn CpuOnline>,
            regs,
            log,
        );
        CpuSlots::new(engine, cpus)
    }

    /// A machine whose CPU slots a hot-add can fill.
    fn inventory_with_cpus(cpus: Arc<FakeCpus>) -> Inventory {
        Inventory::new(
            Arc::new(DeviceRegistry::new()),
            HotplugEngines {
                cpu: Some(cpu_slots(Arc::clone(&cpus))),
                ..HotplugEngines::default()
            },
            cpus.boot_cpus(),
            cpus.possible_cpus(),
            1_073_741_824,
        )
    }

    fn with_devices(specs: &[(&str, Option<&str>)]) -> DeviceRegistry {
        let registry = DeviceRegistry::new();
        for (id, slot) in specs {
            registry
                .insert(RegisteredDevice::new(
                    *id,
                    slot.and_then(vmm_machine::parse_bdf),
                    None,
                    None,
                    None,
                ))
                .expect("insert");
        }
        registry
    }

    #[test]
    fn device_list_reports_id_bdf_and_state() {
        let inv = inventory(with_devices(&[
            ("virtio-blk@4", Some("4")),
            ("virtio-vsock@5", Some("5")),
        ]));

        assert_eq!(
            inv.handle_line("device-list"),
            "OK 2 virtio-blk@4,0.4.0,present virtio-vsock@5,0.5.0,present"
        );
    }

    #[test]
    fn a_device_with_no_pci_face_reports_a_placeholder_bdf() {
        // Every record has three values, so a client can split without
        // counting fields per device.
        let inv = inventory(with_devices(&[("varstore", None)]));

        assert_eq!(inv.handle_line("device-list"), "OK 1 varstore,-,present");
    }

    #[test]
    fn an_empty_registry_answers_with_a_zero_count() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(inv.handle_line("device-list"), "OK 0");
    }

    #[test]
    fn cpu_list_reports_one_record_per_boot_cpu() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(
            inv.handle_line("cpu-list"),
            "OK 2 0,present,boot 1,present,boot"
        );
    }

    #[test]
    fn cpu_list_reports_the_madt_slots_even_with_no_engine() {
        // -c cpus=2,maxcpus=4 without --hotplug still gives the guest
        // four LAPIC entries. Hiding two of them here would contradict
        // what the guest reads. cpu-add is what says they cannot be
        // filled.
        let inv = Inventory::new(
            Arc::new(DeviceRegistry::new()),
            HotplugEngines::default(),
            2,
            4,
            1_073_741_824,
        );

        assert_eq!(
            inv.handle_line("cpu-list"),
            "OK 4 0,present,boot 1,present,boot 2,absent,hotplug \
             3,absent,hotplug"
        );
        assert_eq!(
            inv.handle_line("cpu-add 2"),
            "ERR CPU hot-add needs --hotplug and -c cpus=N,maxcpus=M",
        );
    }

    #[test]
    fn cpu_list_reports_every_slot_a_hot_add_can_take() {
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(
            inv.handle_line("cpu-list"),
            "OK 4 0,present,boot 1,present,boot 2,absent,hotplug \
             3,absent,hotplug"
        );
    }

    #[test]
    fn cpu_list_shows_a_cpu_that_was_hot_added() {
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(inv.handle_line("cpu-add 3"), "OK 3,present");

        let listed = inv.handle_line("cpu-list");
        assert!(listed.contains("3,present,hotplug"), "{listed}");
        assert!(listed.contains("2,absent,hotplug"), "{listed}");
    }

    #[test]
    fn cpu_list_shows_an_id_a_failed_add_spent() {
        // ThreadGone means VM_ACTIVATE_CPU went through and the thread
        // did not. illumos cannot deactivate a vCPU, so the id is gone
        // and an operator has to be able to see that.
        let inv = inventory_with_cpus(FakeCpus::refusing(
            4,
            2,
            VcpuError::ThreadGone(3),
        ));

        assert!(inv.handle_line("cpu-add 3").starts_with("ERR "));

        let listed = inv.handle_line("cpu-list");
        assert!(listed.contains("3,consumed,hotplug"), "{listed}");
        assert_eq!(
            inv.handle_line("cpu-add 3"),
            "ERR vCPU 3 was consumed by a failed add: the kernel cannot \
             deactivate a vCPU, so the id cannot be reused"
        );
    }

    #[test]
    fn a_cpu_id_the_machine_does_not_have_is_refused() {
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(
            inv.handle_line("cpu-add 4"),
            "ERR vCPU 4 is not one of the 4 CPU slots"
        );
        assert_eq!(
            inv.handle_line("cpu-add 0"),
            "ERR vCPU 0 is a boot CPU and is online already"
        );
    }

    #[test]
    fn a_cpu_id_that_is_not_a_number_is_refused() {
        // Read before the engine is looked up, so a typo is answered as
        // a typo whatever the VM was started with.
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(inv.handle_line("cpu-add two"), "ERR two is not a CPU id");
        assert_eq!(inv.handle_line("cpu-add -1"), "ERR -1 is not a CPU id");
        assert_eq!(
            inv.handle_line("cpu-add 99999999999999999999"),
            "ERR 99999999999999999999 is not a CPU id",
        );
    }

    #[test]
    fn a_hostile_cpu_id_cannot_reach_the_operator_terminal() {
        let inv = inventory(DeviceRegistry::new());

        let line = inv.handle_line("cpu-add \u{1b}[2Jwiped");

        assert_eq!(line, "ERR __2Jwiped is not a CPU id");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn mem_list_reports_the_boot_memory() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(inv.handle_line("mem-list"), "OK 1 boot,1073741824,present");
    }

    #[test]
    fn mem_list_reports_the_window_the_slots_and_what_is_used() {
        // The engine itself needs a live VM, so the wire shape is
        // pinned against the numbers a live one would report.
        let window = vmm_machine::hot_add_window(
            1024 * 1024 * 1024,
            4 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
        )
        .expect("4 slots of 1 GiB");

        let records = window_records(&window, 1, 1024 * 1024 * 1024);

        assert_eq!(
            records,
            [
                "window,4294967296,present".to_string(),
                "slot-size,1073741824,present".to_string(),
                "slot-count,4,present".to_string(),
                "slots-used,1,present".to_string(),
                "added,1073741824,present".to_string(),
            ],
        );
    }

    #[test]
    fn a_memory_size_that_is_not_a_number_is_refused() {
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            inv.handle_line("mem-add 512M"),
            "ERR 512M is not a size in bytes",
        );
        assert_eq!(
            inv.handle_line("mem-add -1"),
            "ERR -1 is not a size in bytes"
        );
    }

    #[test]
    fn no_refusal_carries_a_run_of_spaces() {
        // A `\` line continuation in a string literal eats the newline
        // AND the indent that follows it. A message rebuilt without one
        // keeps that indent and reaches the operator as a gap.
        for message in [
            CPU_HOTPLUG_OFF,
            MEM_HOTPLUG_OFF,
            NO_CPU_REMOVE,
            NO_MEM_REMOVE,
        ] {
            assert!(!message.contains("  "), "{message}");
        }
    }

    #[test]
    fn a_removal_says_why_this_platform_cannot_do_it() {
        // "unknown command" would read as a typo the operator can fix.
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            inv.handle_line("cpu-remove 3"),
            "ERR cpu-remove is not supported: illumos has no \
             vm_deactivate_cpu, so an active vCPU cannot be taken back"
        );
        assert_eq!(
            inv.handle_line("mem-remove slot0"),
            "ERR mem-remove is not supported: illumos has no \
             VM_FREE_MEMSEG, so a memory segment lives until the VM does"
        );
    }

    #[test]
    fn an_unknown_verb_is_refused() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(
            inv.handle_line("device-plug 4,virtio-blk,/d.img"),
            "ERR unknown command device-plug"
        );
        assert_eq!(
            inv.handle_line("DEVICE-LIST"),
            "ERR unknown command DEVICE-LIST"
        );
    }

    #[test]
    fn a_vm_without_hotplug_refuses_an_add_or_a_remove() {
        // The default. Without --hotplug the guest has no interface to
        // answer on, so the request is refused and nothing is opened.
        let inv = inventory(DeviceRegistry::new());
        let refused = "ERR this VM was not started with --hotplug";

        assert_eq!(inv.handle_line("device-add 5,virtio-blk,/d.img"), refused);
        assert_eq!(inv.handle_line("device-remove virtio-blk@5"), refused);
    }

    #[test]
    fn a_vm_without_hotplug_refuses_a_cpu_or_memory_add() {
        // Each answer names the option the operator has to add, since
        // the two engines need different ones.
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            inv.handle_line("cpu-add 3"),
            "ERR CPU hot-add needs --hotplug and -c cpus=N,maxcpus=M",
        );
        assert_eq!(
            inv.handle_line("mem-add 134217728"),
            "ERR memory hot-add needs --hotplug and -o hotplug.maxmem=SIZE",
        );
    }

    #[test]
    fn a_vm_with_cpu_slots_still_refuses_a_memory_add() {
        // The two engines are started by different options, so one
        // being on must not answer for the other.
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(
            inv.handle_line("mem-add 134217728"),
            "ERR memory hot-add needs --hotplug and -o hotplug.maxmem=SIZE",
        );
    }

    #[test]
    fn a_hotplug_verb_needs_exactly_one_argument() {
        // A device-add with no spec must not read as an empty spec, and
        // a second field means the request was mis-quoted.
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            inv.handle_line("device-add"),
            "ERR device-add needs one argument"
        );
        assert_eq!(
            inv.handle_line("device-remove"),
            "ERR device-remove needs one argument"
        );
        assert_eq!(
            inv.handle_line("device-add 5,virtio-blk, /d.img"),
            "ERR device-add takes one argument"
        );
        assert_eq!(
            inv.handle_line("cpu-add"),
            "ERR cpu-add needs one argument"
        );
        assert_eq!(
            inv.handle_line("mem-add"),
            "ERR mem-add needs one argument"
        );
        assert_eq!(
            inv.handle_line("mem-add 128 MiB"),
            "ERR mem-add takes one argument"
        );
    }

    #[test]
    fn a_refusal_cannot_reach_the_operator_terminal() {
        // An error quotes the spec the peer sent, so a control sequence
        // in it would otherwise be read by whatever terminal shows the
        // answer.
        let line = refused(
            &HotplugError::BadSpec("\u{1b}[2Jwiped\nOK 0".to_string())
                .to_string(),
        );

        // The escape introducer is gone, so the rest is inert text.
        assert_eq!(line, "ERR bad spec: _[2Jwiped_OK 0");
        assert!(!line.contains('\n'), "a refusal cannot add a line");
    }

    #[test]
    fn a_long_refusal_is_capped() {
        let long = "a".repeat(REASON_MAX * 2);
        let line = refused(&HotplugError::BadSpec(long).to_string());

        assert_eq!(line.len(), REASON_MAX + "ERR ".len());
    }

    #[test]
    fn a_read_only_verb_takes_no_argument() {
        // Silently ignoring the argument would let a later
        // `device-list <id>` look like it filtered.
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(
            inv.handle_line("device-list all"),
            "ERR device-list takes no argument"
        );
    }

    #[test]
    fn a_blank_request_is_refused() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(inv.handle_line(""), "ERR empty request");
    }

    #[test]
    fn a_hostile_id_cannot_forge_a_record_boundary() {
        // The id comes from a `-s` argument. A space or a comma in it
        // would add a field or a value that a client would read as a
        // second device.
        let inv = inventory(with_devices(&[("evil id,0.0.0,absent", None)]));

        assert_eq!(
            inv.handle_line("device-list"),
            "OK 1 evil_id_0.0.0_absent,-,present"
        );
    }

    #[test]
    fn a_hostile_id_cannot_reach_the_operator_terminal() {
        // An escape sequence in an id would otherwise be interpreted by
        // whatever terminal reads the answer.
        let inv = inventory(with_devices(&[("\u{1b}[2Jwiped", None)]));

        assert_eq!(inv.handle_line("device-list"), "OK 1 __2Jwiped,-,present");
    }

    #[test]
    fn a_long_id_is_capped() {
        let long = "a".repeat(FIELD_MAX * 2);
        let inv = inventory(with_devices(&[(long.as_str(), None)]));

        let answer = inv.handle_line("device-list");
        assert_eq!(answer, format!("OK 1 {},-,present", "a".repeat(FIELD_MAX)));
    }

    #[test]
    fn an_unknown_verb_is_echoed_safely() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(inv.handle_line("\u{1b}[2J"), "ERR unknown command __2J");
    }
}
