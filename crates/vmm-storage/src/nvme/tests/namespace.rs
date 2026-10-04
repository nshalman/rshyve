// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The namespace shape a device chooses: block size and write cache.

use vmm_devices::lifecycle::{NvmeMigrateBackend, NvmeMigrateNamespace};

use super::*;

const NO_CACHE_4K: NvmeConfig = NvmeConfig {
    lba_size: LbaSize::B4096,
    volatile_write_cache: false,
    msi_policy: MsiPolicy::PerBatch,
};

fn identify(h: &mut Harness, cns: u8) -> Vec<u8> {
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_IDENTIFY,
        nsid: 1,
        prp1: 0x20000,
        cdw10: u32::from(cns),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    h.read_guest(0x20000, PAGE_SIZE)
}

fn get_vwc(h: &mut Harness) -> Cqe {
    h.admin(Sqe {
        opcode: ADMIN_OPC_GET_FEATURES,
        cdw10: u32::from(FEAT_VOLATILE_WC),
        ..Sqe::default()
    })
}

fn lbads(ns: &[u8]) -> u32 {
    (u32::from_le_bytes(ns[128..132].try_into().expect("LBAF0")) >> 16) & 0xFF
}

#[test]
fn the_default_namespace_keeps_512_byte_blocks_and_a_write_cache() {
    let mut h = Harness::new();
    h.enable();
    let ns = identify(&mut h, IDENTIFY_CNS_NAMESPACE);
    assert_eq!(lbads(&ns), 9);
    let nsze = u64::from_le_bytes(ns[0..8].try_into().expect("NSZE"));
    assert_eq!(nsze, BACKING_BLOCKS);
    assert_eq!(identify(&mut h, IDENTIFY_CNS_CONTROLLER)[525], 1, "VWC");
    let cqe = get_vwc(&mut h);
    assert_eq!((cqe.status(), cqe.dw0), (Status::SUCCESS, 1));
}

#[test]
fn a_4k_namespace_without_a_write_cache_says_so() {
    let manual = Manual::durable(IO_QDEPTH);
    let mut h = Harness::manual_with(&manual, NO_CACHE_4K);
    h.enable();
    let ns = identify(&mut h, IDENTIFY_CNS_NAMESPACE);
    assert_eq!(lbads(&ns), 12);
    let nsze = u64::from_le_bytes(ns[0..8].try_into().expect("NSZE"));
    assert_eq!(nsze, BACKING_BLOCKS * 512 / 4096);
    assert_eq!(identify(&mut h, IDENTIFY_CNS_CONTROLLER)[525], 0, "VWC");
    assert_eq!(
        get_vwc(&mut h).status(),
        Status::of(SC_INVALID_FIELD | SC_DNR)
    );
}

#[test]
fn a_4k_namespace_moves_4k_per_block() {
    let manual = Manual::durable(IO_QDEPTH);
    let mut h = Harness::manual_with(&manual, NO_CACHE_4K);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(3, 1, 0x50000, 0), 0x31);
    let req = manual.take().pop().expect("one request");
    assert_eq!((req.lba(), req.blocks()), (3, 1));
    assert_eq!((req.byte_offset(), req.data().len()), (3 * 4096, 4096));
    let status = manual.run(&req);
    manual.sink().complete(&[(req.id(), status)]);
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);
    let expect: Vec<u8> = (3 * 4096..4 * 4096).map(backing_byte).collect();
    assert_eq!(h.read_guest(0x50000, 4096), expect);

    // MDTS is 2 MiB whatever the block size: 513 blocks is past it.
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 513, 0x50000, 0), 0x32);
    let cqe = h.wait_cqe(CQ1, 1, true);
    assert_eq!(cqe.status(), Status::of(SC_INVALID_FIELD | SC_DNR));
}

/// With no write cache every completed write is durable, so a flush
/// has nothing to do and never reaches the backend.
#[test]
fn a_flush_without_a_write_cache_completes_at_once() {
    let manual = Manual::durable(IO_QDEPTH);
    let mut h = Harness::manual_with(&manual, NO_CACHE_4K);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let flush = Sqe {
        opcode: NVM_OPC_FLUSH,
        nsid: 1,
        ..Sqe::default()
    };
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, flush, 0x41);
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);
    assert!(manual.submitted_queues().is_empty(), "the flush was sent");
}

/// A guest told there is no cache never flushes, so a store that
/// buffers writes would lose what the guest was told is safe.
#[test]
fn no_write_cache_needs_a_backend_whose_writes_are_durable() {
    let manual = Manual::new(IO_QDEPTH);
    let error = Harness::from_factory(
        manual.factory(),
        false,
        FENCE_BUDGETS,
        NO_CACHE_4K,
    )
    .err()
    .expect("a buffering backend with no cache reported");
    assert!(error.to_string().contains("durable"), "{error}");

    let disk = backing_disk();
    let log = slog::Logger::root(slog::Discard, slog::o!());
    let pool =
        WorkerPool::factory(Arc::clone(&disk), disk, log).expect("worker pool");
    assert!(
        Harness::from_factory(pool, false, FENCE_BUDGETS, NO_CACHE_4K).is_err()
    );
}

#[test]
fn a_restore_onto_another_namespace_shape_is_refused() {
    let manual = Manual::durable(IO_QDEPTH);
    let mut src = Harness::manual_with(&manual, NO_CACHE_4K);
    src.enable();
    let state = src
        .ctrl
        .export_migrate_state()
        .expect("export")
        .expect("enabled");
    let vmm_devices::DeviceMigrateState::Nvme(mig) = &state else {
        panic!("not NVMe state");
    };
    assert_eq!(
        mig.namespace,
        Some(NvmeMigrateNamespace {
            block_size: 4096,
            volatile_write_cache: false,
            total_blocks: BACKING_BLOCKS * 512 / 4096,
            read_only: false,
        })
    );

    let dst = Harness::new();
    let error = dst
        .ctrl
        .restore_migrate_state(&state)
        .expect_err("512-byte blocks here");
    assert!(error.to_string().contains("namespace"), "{error}");

    // A v2 payload names no shape, which means the one v2 had.
    let mut v2 = mig.clone();
    v2.namespace = None;
    let v2 = vmm_devices::DeviceMigrateState::Nvme(v2);
    let dst = Harness::manual_with(&Manual::durable(IO_QDEPTH), NO_CACHE_4K);
    dst.ctrl.restore_migrate_state(&v2).expect_err("not 4K");
    Harness::new()
        .ctrl
        .restore_migrate_state(&v2)
        .expect("a v2 payload onto the v2 shape");
}

fn exported(h: &mut Harness) -> vmm_devices::lifecycle::NvmeMigrateState {
    h.enable();
    match h.ctrl.export_migrate_state() {
        Ok(Some(vmm_devices::DeviceMigrateState::Nvme(state))) => state,
        other => panic!("no NVMe state: {other:?}"),
    }
}

/// A guest whose disk shrank reads past its end; one that grew has a
/// partition table that no longer reaches the last block.
#[test]
fn a_restore_onto_another_capacity_is_refused() {
    let mut state = exported(&mut Harness::new());
    let ns = state.namespace.as_mut().expect("v3 carries the namespace");
    assert_eq!(ns.total_blocks, BACKING_BLOCKS);
    ns.total_blocks += 1;

    let error = Harness::new()
        .ctrl
        .restore_migrate_state(&vmm_devices::DeviceMigrateState::Nvme(state))
        .expect_err("one block more on the source");
    assert!(error.to_string().contains("total_blocks"), "{error}");
}

/// A guest that mounted a read-only disk would see its writes fail,
/// and one that mounted it writable could write a disk shared as
/// read-only.
#[test]
fn a_restore_onto_another_write_protection_is_refused() {
    let state = exported(&mut Harness::read_only());
    assert!(state.namespace.expect("namespace").read_only);
    let state = vmm_devices::DeviceMigrateState::Nvme(state);

    let error = Harness::new()
        .ctrl
        .restore_migrate_state(&state)
        .expect_err("read-only source, writable destination");
    assert!(error.to_string().contains("read_only"), "{error}");
    Harness::read_only()
        .ctrl
        .restore_migrate_state(&state)
        .expect("the same protection");

    let state = exported(&mut Harness::new());
    let state = vmm_devices::DeviceMigrateState::Nvme(state);
    Harness::read_only()
        .ctrl
        .restore_migrate_state(&state)
        .expect_err("writable source, read-only destination");
}

fn volume(name: &str) -> NvmeMigrateBackend {
    NvmeMigrateBackend::MantaBlock {
        volume: name.to_string(),
    }
}

fn on_store(store: NvmeMigrateBackend) -> Harness {
    Harness::manual(&Manual::on_store(store, true))
}

fn restore(
    h: &Harness,
    state: vmm_devices::lifecycle::NvmeMigrateState,
) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
    h.ctrl
        .restore_migrate_state(&vmm_devices::DeviceMigrateState::Nvme(state))
}

/// Two volumes of one shape pass every namespace check, so only the
/// store tells them apart.
#[test]
fn a_restore_onto_another_store_is_refused() {
    let file = exported(&mut Harness::new());
    assert_eq!(file.backend, Some(NvmeMigrateBackend::File));
    restore(&on_store(NvmeMigrateBackend::File), file.clone())
        .expect("file to file");

    let error = restore(&on_store(volume("a")), file.clone())
        .expect_err("file to a volume");
    assert!(error.to_string().contains("MantaBlock volume a"), "{error}");

    let on_a = exported(&mut on_store(volume("a")));
    assert_eq!(on_a.backend, Some(volume("a")));
    restore(&on_store(volume("a")), on_a.clone()).expect("the same volume");
    restore(&on_store(volume("b")), on_a.clone()).expect_err("another volume");
    restore(&Harness::new(), on_a).expect_err("a volume to a file");
}

/// A v3 source had only host files.
#[test]
fn a_payload_without_a_store_is_a_file() {
    let mut state = exported(&mut Harness::new());
    state.backend = None;
    restore(&Harness::new(), state.clone()).expect("a v3 payload");
    restore(&on_store(volume("a")), state).expect_err("onto a volume");
}

/// The source keeps its attachment, so a destination that attached as
/// well would make two owners of one volume.
#[test]
fn a_store_that_cannot_migrate_refuses_export_and_restore() {
    let store = Manual::on_store(volume("a"), false);
    let h = Harness::manual(&store);
    let why = h.ctrl.migration_blocker().expect("a blocker");
    assert!(why.contains("MantaBlock volume a"), "{why}");
    // Disabled: the export would otherwise be an empty `None`.
    let error = h.ctrl.export_migrate_state().expect_err("export");
    assert!(error.to_string().contains("MantaBlock volume a"), "{error}");

    let on_a = exported(&mut on_store(volume("a")));
    restore(&h, on_a).expect_err("restore");
    assert_eq!(Harness::new().ctrl.migration_blocker(), None);
}
