// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

#[test]
fn the_destination_is_told_about_a_hotplugged_device() {
    // The destination imports the state of every registered device,
    // so a device it was not told to create loses the guest a disk.
    use vmm_machine::{DeviceRegistry, RegisteredDevice};

    let registry = DeviceRegistry::new();
    let mut added = RegisteredDevice::new(
        "virtio-blk@5",
        vmm_machine::parse_bdf("5"),
        None,
        None,
        Some("5,virtio-blk,/added.img".to_string()),
    );
    added.hotpluggable = true;
    registry.insert(added).expect("insert");

    let slots = migrate_pci_slots(
        &["4,nvme,/boot.img,bootindex=1".to_string()],
        &registry,
    );

    assert_eq!(
        slots,
        ["4,nvme,/boot.img,bootindex=1", "5,virtio-blk,/added.img"],
    );
}

#[test]
fn a_machine_with_no_hotplug_reports_its_own_argv() {
    let argv = ["4,nvme,/boot.img".to_string()];
    assert_eq!(
        migrate_pci_slots(&argv, &vmm_machine::DeviceRegistry::new()),
        argv,
    );
}

#[test]
fn migrate_source_rejects_host_local_state() {
    let command = Command::MigrateSource {
        target_addr: "192.0.2.10:4567".to_string(),
        zfs_barrier: None,
        gate: None,
        gate_timeout_ms: None,
        allow_engine_without_migrate_v1: false,
    };

    for state in [
        HostLocalState::new(true, false),
        HostLocalState::new(false, true),
        HostLocalState::new(true, true),
    ] {
        let response = migration_source_guard(state, false, &command)
            .expect("host-local state must block migration");
        assert!(!response.success);
        assert!(response
            .error
            .as_deref()
            .expect("error response must explain rejection")
            .contains("host-local state is not carried by migration"));
    }

    assert!(
        migration_source_guard(HostLocalState::default(), false, &command)
            .is_none()
    );
}

#[test]
fn migrate_source_rejects_ahci_cd() {
    let command = Command::MigrateSource {
        target_addr: "192.0.2.10:4567".to_string(),
        zfs_barrier: None,
        gate: None,
        gate_timeout_ms: None,
        allow_engine_without_migrate_v1: false,
    };

    let response =
        migration_source_guard(HostLocalState::default(), true, &command)
            .expect("AHCI CD must block migration");
    assert!(!response.success);
    assert_eq!(
        response.error.as_deref(),
        Some("ahci-cd is not supported on a migration source")
    );
}

#[test]
fn a_vm_that_did_not_grow_can_still_migrate() {
    // Starting with --hotplug is not the same as using it. A VM
    // that added nothing has to keep the migration it had.
    assert_eq!(growth(&HotplugEngines::default()), (0, 0));
    assert_eq!(grown_since_boot(0, 0), None);
}

#[test]
fn a_boot_device_set_can_still_migrate() {
    // Only a hot-added device changes the set. A VM that added
    // nothing has to keep the migration it had.
    assert_eq!(device_set_changed(&[]), None);
}

#[test]
fn a_hot_added_device_cannot_be_a_migration_source() {
    // The destination is built from the boot device list, so it has
    // nowhere to put a device the guest gained since.
    let reason = device_set_changed(&["5,virtio-blk,/added.img".to_string()])
        .expect("an added device must block migration");
    assert!(reason.contains("hot-added 1 device(s)"), "{reason}");
    assert!(reason.contains("5,virtio-blk,/added.img"), "{reason}");
    assert!(reason.contains("boot device list"), "{reason}");
}

fn ident(dev: u8, kind: &str) -> vmm_migrate::codec::DeviceIdentity {
    vmm_migrate::codec::DeviceIdentity {
        bdf: vmm_devices::lifecycle::WireBdf {
            bus: 0,
            dev,
            func: 0,
        },
        kind: kind.to_string(),
    }
}

#[test]
fn a_vm_with_only_migratable_devices_can_be_a_source() {
    assert_eq!(unmigratable_devices(&[]), None);
    assert_eq!(
        unmigratable_devices(&[ident(4, "nvme"), ident(5, "virtio-blk")]),
        None,
    );
    // Its controller state is carried since v9, so the VNC tablet no
    // longer keeps a VM in place.
    assert_eq!(unmigratable_devices(&[ident(30, "xhci")]), None);
    // Their state goes with their transports since v13.
    assert_eq!(
        unmigratable_devices(&[ident(7, "virtio-gpu"), ident(8, "virtio-snd")]),
        None,
    );
}

#[test]
fn a_device_whose_state_is_not_carried_blocks_migration() {
    // The guest driver would find the device at reset on the far
    // side, with no error anywhere.
    for kind in ["virtio-console", "ahci-cd", "virtio-fs", "virtio-vsock"] {
        let reason = unmigratable_devices(&[ident(6, kind)])
            .unwrap_or_else(|| panic!("{kind} must block migration"));
        assert!(reason.contains(kind), "{reason}");
        assert!(reason.contains("6.0"), "{reason}");
    }
}

#[test]
fn every_blocking_device_is_named_in_the_refusal() {
    // An operator has to see all of them before they can retry.
    let reason =
        unmigratable_devices(&[ident(6, "ahci-cd"), ident(7, "virtio-fs")])
            .expect("two blocking devices");
    assert!(reason.contains("ahci-cd@0.6.0"), "{reason}");
    assert!(reason.contains("virtio-fs@0.7.0"), "{reason}");
}

#[test]
fn a_unix_target_skips_the_tcp_address_check() {
    // The GZ agent's socket path is not a host:port.
    assert!(matches!(
        MigrateTarget::parse("/tmp/vmm-migrate-data.sock"),
        Ok(MigrateTarget::Unix(_)),
    ));
    assert!(matches!(
        MigrateTarget::parse("192.0.2.10:4567"),
        Ok(MigrateTarget::Tcp(_)),
    ));
    MigrateTarget::parse("127.0.0.1:4567")
        .err()
        .expect("loopback is refused");
}

#[test]
fn every_changed_slot_is_named_in_the_refusal() {
    // An operator has to see which slots to remove before they can
    // retry, so the message names all of them.
    let reason = device_set_changed(&[
        "5,virtio-blk,/a.img".to_string(),
        "6,virtio-blk,/b.img".to_string(),
    ])
    .expect("two added devices must block migration");
    assert!(reason.contains("5,virtio-blk,/a.img"), "{reason}");
    assert!(reason.contains("6,virtio-blk,/b.img"), "{reason}");
}

#[test]
fn a_device_still_pending_removal_blocks_migration() {
    // `hotplug_specs` keeps a slot the guest has not ejected, so it
    // is still part of the live set and still unsafe to migrate.
    use vmm_machine::{DeviceRegistry, RegisteredDevice, SlotState};

    let registry = DeviceRegistry::new();
    let mut added = RegisteredDevice::new(
        "virtio-blk@5",
        vmm_machine::parse_bdf("5"),
        None,
        None,
        Some("5,virtio-blk,/added.img".to_string()),
    );
    added.hotpluggable = true;
    let id = added.id.clone();
    registry.insert(added).expect("insert");
    registry
        .set_state(&id, SlotState::Present, SlotState::RemovePending)
        .expect("remove requested");

    assert!(
        device_set_changed(&vmm_machine::hotplug_specs(&registry)).is_some()
    );
}

/// An NVMe disk as the migration path sees it: a ring disk says it
/// cannot migrate, a file disk says nothing.
struct Disk(Option<&'static str>);

impl vmm_devices::Lifecycle for Disk {
    fn type_name(&self) -> &'static str {
        "nvme"
    }

    fn migration_blocker(&self) -> Option<String> {
        self.0.map(str::to_string)
    }
}

const RING: &str = "live migration of a MantaBlock disk is not supported yet (volume 6f1c2a8e-0000-4000-8000-000000000001)";

fn disk_at(
    registry: &vmm_machine::DeviceRegistry,
    slot: &str,
    blocker: Option<&'static str>,
    hot_added: bool,
) {
    let mut disk = vmm_machine::RegisteredDevice::new(
        format!("nvme@{slot}"),
        vmm_machine::parse_bdf(slot),
        None,
        Some(Arc::new(Disk(blocker)) as Arc<dyn vmm_devices::Lifecycle>),
        Some(format!("{slot},nvme,mantablock=/s,volume=x")),
    );
    disk.hotpluggable = hot_added;
    registry.insert(disk).expect("insert");
}

fn identities(
    registry: &vmm_machine::DeviceRegistry,
) -> Vec<vmm_migrate::codec::DeviceIdentity> {
    registry
        .list()
        .into_iter()
        .filter_map(|slot| Some(ident(slot.bdf?.dev(), "nvme")))
        .collect()
}

fn assert_names_the_ring_disk(reason: Option<String>, slot: &str) {
    let reason = reason.expect("a ring disk must block migration");
    assert!(reason.contains(&format!("nvme@{slot}")), "{reason}");
    assert!(
        reason.contains(
            "live migration of a MantaBlock disk is not supported yet"
        ),
        "{reason}"
    );
    assert!(reason.contains("6f1c2a8e"), "{reason}");
}

#[test]
fn a_ring_disk_present_at_boot_blocks_migrate_dest() {
    let registry = vmm_machine::DeviceRegistry::new();
    disk_at(&registry, "4", None, false);
    disk_at(&registry, "5", Some(RING), false);

    let run = RunControl::new(VmRunState::Paused);
    assert_names_the_ring_disk(claim_dest(&run, &registry).err(), "5");
    assert_eq!(run.get(), VmRunState::Paused);
    assert_names_the_ring_disk(device_blockers(&registry), "5");
}

#[test]
fn a_hot_added_ring_disk_blocks_migrate_dest() {
    let registry = vmm_machine::DeviceRegistry::new();
    disk_at(&registry, "4", None, false);
    disk_at(&registry, "6", Some(RING), true);

    let run = RunControl::new(VmRunState::Paused);
    assert_names_the_ring_disk(claim_dest(&run, &registry).err(), "6");
    assert_eq!(run.get(), VmRunState::Paused);
}

#[test]
fn a_ring_disk_blocks_migrate_source() {
    for hot_added in [false, true] {
        let registry = vmm_machine::DeviceRegistry::new();
        disk_at(&registry, "5", Some(RING), hot_added);
        let run = RunControl::new(VmRunState::Running);
        let reason = claim_source(
            &run,
            &HotplugEngines::default(),
            &registry,
            || identities(&registry),
            false,
        );
        assert_names_the_ring_disk(reason.err(), "5");
        assert_eq!(run.get(), VmRunState::Running);
    }
}

#[test]
fn a_file_backed_nvme_vm_still_migrates() {
    let registry = vmm_machine::DeviceRegistry::new();
    disk_at(&registry, "4", None, false);

    assert_eq!(device_blockers(&registry), None);
    let dest = RunControl::new(VmRunState::Paused);
    assert_eq!(claim_dest(&dest, &registry), Ok(()));
    assert_eq!(dest.get(), VmRunState::Migrating);
    let source = RunControl::new(VmRunState::Running);
    assert_eq!(
        claim_source(
            &source,
            &HotplugEngines::default(),
            &registry,
            || identities(&registry),
            false,
        ),
        Ok(()),
    );
    assert_eq!(source.get(), VmRunState::Migrating);
}

#[test]
fn migrate_dest_still_needs_a_paused_vm() {
    let registry = vmm_machine::DeviceRegistry::new();
    let run = RunControl::new(VmRunState::Running);
    let reason = claim_dest(&run, &registry).expect_err("a running VM");
    assert!(reason.contains("paused"), "{reason}");
    assert_eq!(run.get(), VmRunState::Running);
}

#[test]
fn a_vm_that_hot_added_cannot_be_a_migration_source() {
    // The destination is built from the same command line, so the
    // loss would show up as guest corruption on the far side and
    // not as a failed migration.
    let cpus = grown_since_boot(2, 0).expect("two added vCPUs");
    assert!(cpus.contains("hot-added 2 vCPUs"), "{cpus}");

    let memory = grown_since_boot(0, 134_217_728).expect("added memory");
    assert!(memory.contains("134217728 bytes"), "{memory}");
}

fn discard() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

#[test]
fn a_destination_stopped_before_any_source_connects_lets_go() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let cancel = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&cancel);
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(accept_one(
            "0.0.0.0:0",
            || seen.load(Ordering::Acquire),
            &discard(),
        ));
    });

    cancel.store(true, Ordering::Release);

    let ended = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the accept outlived the stop");
    assert_eq!(
        ended.expect_err("no source connected"),
        vmm_migrate::CANCELLED
    );
}

fn stop(run: &RunControl) {
    run.stop(
        super::super::run_state::STOP_WAIT,
        || Ok::<(), ()>(()),
        || Ok(()),
        || Ok(()),
    )
    .expect("stop");
}

/// Fill `stream`'s send buffer, so the next blocking write must wait.
fn fill(stream: &mut UnixStream) {
    use std::io::Write as _;
    stream.set_nonblocking(true).expect("nonblocking");
    let chunk = vec![0u8; 1 << 16];
    loop {
        match stream.write(&chunk) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("fill: {e}"),
        }
    }
    stream.set_nonblocking(false).expect("blocking");
}

#[test]
fn a_source_stopped_while_its_peer_stops_reading_lets_go() {
    use std::io::Write as _;
    use std::sync::mpsc::{self, RecvTimeoutError};
    let run = RunControl::new(VmRunState::Running);
    run.admit_source(|| None).expect("claim");
    let (ours, _peer) = UnixStream::pair().expect("socket pair");
    let mut ours = shut_on_stop(&run, ours, &discard()).expect("registered");
    fill(&mut ours);
    let (writing_tx, writing) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = writing_tx.send(());
        let chunk = vec![0u8; 4096];
        let error = loop {
            if let Err(e) = ours.write_all(&chunk) {
                break e;
            }
        };
        let _ = tx.send(error);
    });
    writing.recv().expect("the writer started");
    // The buffer is full and nothing reads, so a writer still going
    // after this is blocked in the send.
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(300)).err(),
        Some(RecvTimeoutError::Timeout),
        "the send never blocked"
    );

    stop(&run);

    rx.recv_timeout(Duration::from_secs(10))
        .expect("the send outlived the stop");
}

/// A listener whose backlog a few connects fill.
fn short_listener(path: &Path) -> std::os::fd::OwnedFd {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
    let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .expect("socket");
    rustix::net::bind(&fd, &SocketAddrUnix::new(path).expect("addr"))
        .expect("bind");
    rustix::net::listen(&fd, 1).expect("listen");
    fd
}

/// Connections that fill `path`'s backlog, kept so it stays full.
/// `None` where a full backlog refuses a connect instead, as illumos
/// does: there no connect waits.
fn fill_backlog(path: &Path) -> Option<Vec<UnixStream>> {
    let mut held = Vec::new();
    for _ in 0..64 {
        let checks = std::sync::atomic::AtomicUsize::new(0);
        // A connect still waiting after a few checks found it full.
        match vmm_migrate::connect::unix(path, || {
            checks.fetch_add(1, std::sync::atomic::Ordering::AcqRel) > 3
        }) {
            Ok(Ok(stream)) => held.push(stream),
            Ok(Err(e)) if e.kind() == io::ErrorKind::ConnectionRefused => {
                return None;
            }
            Ok(Err(e)) => panic!("filling the backlog: {e}"),
            Err(_) => return Some(held),
        }
    }
    panic!("the backlog never filled");
}

#[test]
fn a_source_stopped_while_it_connects_lets_go() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("full.sock");
    let _listener = short_listener(&path);
    let held = fill_backlog(&path);
    let run = Arc::new(RunControl::new(VmRunState::Running));
    run.admit_source(|| None).expect("claim");
    if held.is_none() {
        let error = connect_unix(&run, &path, &discard()).err();
        assert!(error.is_some_and(|e| !e.contains(vmm_migrate::CANCELLED)));
        return;
    }
    let (tx, rx) = mpsc::channel();
    let connecting = Arc::clone(&run);
    thread::spawn(move || {
        let _ = tx.send(connect_unix(&connecting, &path, &discard()).err());
    });
    // The backlog is full, so a connect still going after this waits.
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(300)).err(),
        Some(RecvTimeoutError::Timeout),
        "the connect never waited"
    );

    stop(&run);

    let error = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the connect outlived the stop")
        .expect("a stopped connect fails");
    assert!(error.contains(vmm_migrate::CANCELLED), "{error}");
}

struct PassthruStub;

impl vmm_devices::pci::PciDevice for PassthruStub {
    fn cfg_read(&self, _offset: u8, _len: u8) -> u32 {
        0
    }
    fn cfg_write(&self, _offset: u8, _len: u8, _val: u32) {}
    fn bar_rw(
        &self,
        _bar: vmm_devices::pci::BarN,
        _offset: usize,
        _rwo: vmm_core::common::RWOp<'_>,
    ) {
    }
}

#[test]
fn a_passthru_device_blocks_migration() {
    // passthru registers a PCI handle and no lifecycle, so no
    // lifecycle blocker ever saw it.
    let registry = vmm_machine::DeviceRegistry::new();
    disk_at(&registry, "4", None, false);
    registry
        .insert(vmm_machine::RegisteredDevice::new(
            "passthru@6",
            vmm_machine::parse_bdf("6"),
            Some(Arc::new(PassthruStub) as Arc<dyn vmm_devices::pci::PciDevice>),
            None,
            Some("6,passthru,/dev/ppt0".to_string()),
        ))
        .expect("insert passthru");

    let reason = device_blockers(&registry).expect("passthru must block");
    assert!(reason.contains("passthru@6"), "{reason}");
    let dest = RunControl::new(VmRunState::Paused);
    assert!(claim_dest(&dest, &registry).is_err());
    assert_eq!(dest.get(), VmRunState::Paused);
}

/// A ring disk on an engine without migrate-v1, with its shape.
struct Unproven;

impl vmm_devices::Lifecycle for Unproven {
    fn type_name(&self) -> &'static str {
        "nvme"
    }

    fn migration_waivable(&self) -> Option<String> {
        Some("the engine does not report migrate-v1".into())
    }

    fn migration_spec_options(&self) -> Option<String> {
        Some("ring-shape=1:256:4096:1:8:16:8:2:d".into())
    }
}

fn unproven_at(registry: &vmm_machine::DeviceRegistry, slot: &str) {
    registry
        .insert(vmm_machine::RegisteredDevice::new(
            format!("nvme@{slot}"),
            vmm_machine::parse_bdf(slot),
            None,
            Some(Arc::new(Unproven) as Arc<dyn vmm_devices::Lifecycle>),
            None,
        ))
        .expect("insert");
}

#[test]
fn an_unproven_engine_blocks_a_source_unless_the_operator_accepts_it() {
    let registry = vmm_machine::DeviceRegistry::new();
    unproven_at(&registry, "5");
    assert_eq!(device_blockers(&registry), None);

    let run = RunControl::new(VmRunState::Running);
    let why = claim_source(
        &run,
        &HotplugEngines::default(),
        &registry,
        || identities(&registry),
        false,
    )
    .expect_err("not accepted");
    assert!(why.contains("nvme@5"), "{why}");
    assert!(why.contains("allow_engine_without_migrate_v1"), "{why}");
    assert_eq!(run.get(), VmRunState::Running);

    assert_eq!(
        claim_source(
            &run,
            &HotplugEngines::default(),
            &registry,
            || identities(&registry),
            true,
        ),
        Ok(()),
    );
}

/// A ring disk whose engine can change migrate-v1 under a live VM, as
/// a reattach after an engine restart does.
struct Restarted(Arc<std::sync::atomic::AtomicBool>);

impl vmm_devices::Lifecycle for Restarted {
    fn type_name(&self) -> &'static str {
        "nvme"
    }

    fn migration_waivable(&self) -> Option<String> {
        let proven = self.0.load(std::sync::atomic::Ordering::Acquire);
        (!proven).then(|| "the engine does not report migrate-v1".into())
    }
}

#[test]
fn a_source_follows_migrate_v1_as_the_engine_changes() {
    let registry = vmm_machine::DeviceRegistry::new();
    let proven = Arc::new(std::sync::atomic::AtomicBool::new(false));
    registry
        .insert(vmm_machine::RegisteredDevice::new(
            "nvme@5",
            vmm_machine::parse_bdf("5"),
            None,
            Some(Arc::new(Restarted(Arc::clone(&proven)))
                as Arc<dyn vmm_devices::Lifecycle>),
            None,
        ))
        .expect("insert");
    let claim = || {
        let run = RunControl::new(VmRunState::Running);
        claim_source(
            &run,
            &HotplugEngines::default(),
            &registry,
            || identities(&registry),
            false,
        )
    };
    assert!(claim().is_err_and(|w| w.contains("nvme@5")));
    proven.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(claim(), Ok(()));
    proven.store(false, std::sync::atomic::Ordering::Release);
    assert!(claim().is_err_and(|w| w.contains("migrate-v1")));
}

#[test]
fn migrate_config_gives_a_ring_disk_its_shape_once() {
    let registry = vmm_machine::DeviceRegistry::new();
    unproven_at(&registry, "5");
    let argv = [
        "4,nvme,/boot.img".to_string(),
        "5,nvme,mantablock=/s,volume=v,bootindex=1".to_string(),
        "5,nvme,mantablock=/s,volume=v,ring-shape=1:9:512:1:1:1:1:1:-"
            .to_string(),
    ];
    let slots = migrate_pci_slots(&argv, &registry);
    assert_eq!(slots[0], "4,nvme,/boot.img");
    assert_eq!(
        slots[1],
        "5,nvme,mantablock=/s,volume=v,bootindex=1,\
         ring-shape=1:256:4096:1:8:16:8:2:d"
    );
    // A VM that was itself a destination: the old shape is replaced.
    assert_eq!(
        slots[2],
        "5,nvme,mantablock=/s,volume=v,ring-shape=1:256:4096:1:8:16:8:2:d"
    );
}

#[test]
fn the_gate_wait_is_bounded() {
    assert_eq!(gate_budget(None), Ok(vmm_migrate::gate::DEFAULT_BUDGET));
    assert_eq!(gate_budget(Some(5000)), Ok(Duration::from_secs(5)));
    assert!(gate_budget(Some(0)).is_err());
    assert!(gate_budget(Some(25_001)).is_err());
}

#[test]
fn the_gate_is_an_absolute_socket_path() {
    assert!(agent_socket("/var/run/gate.sock").is_ok());
    assert!(agent_socket("gate.sock").is_err());
    assert!(agent_socket("/var/run/a\0b").is_err());
}

#[test]
fn a_running_vm_cannot_import_a_ring_disk() {
    // It attached the volume at boot, and the source still owns it.
    let registry = vmm_machine::DeviceRegistry::new();
    unproven_at(&registry, "5");
    let run = RunControl::new(VmRunState::Paused);
    let why = claim_dest(&run, &registry).expect_err("a ring disk");
    assert!(why.contains("nvme@5"), "{why}");
    assert!(why.contains("--migrate-listen"), "{why}");
    assert_eq!(run.get(), VmRunState::Paused);
}
