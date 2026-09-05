// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the virtio-blk device.

use std::time::{Duration, Instant};

use super::super::queue::VirtqDesc;
use super::*;

mod reset;

#[test]
fn quiesce_query_is_non_blocking_with_active_worker() {
    let file = tempfile::tempfile().expect("create tempfile");
    let fd = file.as_raw_fd();
    let device: Arc<dyn Lifecycle> = Arc::new(VirtioBlock {
        fd,
        capacity: 0,
        sector_size: 512,
        read_only: true,
        nodelete: false,
        num_queues: 0,
        workers_per_queue: 0,
        negotiated_features: Mutex::new(0),
        io_txs: Vec::new(),
        next_worker: Vec::new(),
        physmap: Arc::new(PhysMap::new()),
        interrupt: Arc::new(Mutex::new(None)),
        completions: Mutex::new(Vec::new()),
        _cache: DiskCache::new(fd, true),
        _file: file,
        indicator: Indicator::new(),
        gate: Arc::new(QuiesceGate::new(1)),
        ctx: WorkerCtx {
            physmap: Arc::new(PhysMap::new()),
            gate: Arc::new(QuiesceGate::new(1)),
            epoch: Arc::new(AtomicU64::new(0)),
            inflight: Arc::new(AtomicUsize::new(0)),
        },
    });

    let probe = Arc::clone(&device);
    assert!(!vmm_devices_testsupport::assert_query_non_blocking(
        move || probe.is_quiesced()
    ));
}

#[test]
fn default_opts() {
    let opts = VirtioBlockOpts::default();
    assert!(!opts.read_only);
    assert!(!opts.nodelete);
    assert_eq!(opts.sector_size, 512);
}

#[test]
fn sector_offset_overflow_returns_ioerr() {
    // sector * SECTOR_SIZE would overflow u64
    let huge_sector: u64 = u64::MAX / bits::SECTOR_SIZE + 1;
    assert!(huge_sector.checked_mul(bits::SECTOR_SIZE).is_none());
}

#[test]
fn sector_offset_valid() {
    // Normal sector values should not overflow
    let sector: u64 = 1_000_000;
    let offset = sector.checked_mul(bits::SECTOR_SIZE);
    assert_eq!(offset, Some(512_000_000));
}

#[test]
fn bounds_check_overflow() {
    // sector + sectors_needed overflows u64
    let sector: u64 = u64::MAX;
    let sectors_needed: u64 = 1;
    assert!(sector.checked_add(sectors_needed).is_none());
}

#[test]
fn bounds_check_past_capacity() {
    // sector + sectors_needed > capacity
    let capacity: u64 = 1000;
    let sector: u64 = 999;
    let sectors_needed: u64 = 2;
    assert!(sector
        .checked_add(sectors_needed)
        .is_none_or(|end| end > capacity));
}

#[test]
fn bounds_check_exact_fit() {
    // sector + sectors_needed == capacity should be OK
    let capacity: u64 = 1000;
    let sector: u64 = 998;
    let sectors_needed: u64 = 2;
    assert!(sector
        .checked_add(sectors_needed)
        .is_some_and(|end| end <= capacity));
}

/// Helper: create a VirtioBlock backed by a temporary file.
fn make_test_block(size_bytes: u64) -> VirtioBlock {
    use std::io::Write;
    let mut tmpfile = tempfile::tempfile().expect("create tempfile");
    tmpfile.set_len(size_bytes).expect("set file length");
    // Ensure the file has some content so metadata().len() works
    tmpfile.write_all(&[0u8; 1]).expect("write to tempfile");
    tmpfile.flush().expect("flush tempfile");
    let physmap = Arc::new(PhysMap::new());
    let opts = VirtioBlockOpts::default();
    VirtioBlock::new(tmpfile, &opts, physmap, 1).expect("create VirtioBlock")
}

fn make_get_id_queue(
    queue_size: u16,
    avail_idx: u16,
) -> (Arc<PhysMap>, VirtQueue, VirtioBlock) {
    const DESC_GPA: u64 = 0x1000;
    const AVAIL_GPA: u64 = 0x1100;
    const USED_GPA: u64 = 0x1200;
    const HEADER_GPA: u64 = 0x2000;
    const DATA_GPA: u64 = 0x3000;

    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x3000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(queue_size);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    queue.set_event_idx(true);

    let header = VirtioBlkReqHdr {
        rtype: bits::VIRTIO_BLK_T_GET_ID,
        _reserved: 0,
        sector: 0,
    };
    physmap
        .lookup(HEADER_GPA, BLK_REQ_HEADER_SIZE)
        .expect("mapped request header")
        .write(&header)
        .expect("write request header");
    physmap
        .lookup(DESC_GPA, 16)
        .expect("mapped header descriptor")
        .write(&VirtqDesc {
            addr: HEADER_GPA,
            len: BLK_REQ_HEADER_SIZE as u32,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        })
        .expect("write header descriptor");
    physmap
        .lookup(DESC_GPA + 16, 16)
        .expect("mapped data descriptor")
        .write(&VirtqDesc {
            addr: DATA_GPA,
            len: VIRTIO_BLK_ID_BYTES as u32,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        })
        .expect("write data descriptor");

    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&avail_idx)
        .expect("write avail index");
    for idx in 0..queue_size {
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
            .expect("mapped avail entry")
            .write::<u16>(&0)
            .expect("write avail entry");
    }

    let file = tempfile::tempfile().expect("create tempfile");
    file.set_len(512).expect("set file length");
    let opts = VirtioBlockOpts::default();
    let block = VirtioBlock::new(file, &opts, Arc::clone(&physmap), 1)
        .expect("create VirtioBlock");
    (physmap, queue, block)
}

#[test]
fn notify_queue_caps_collection_at_queue_size() {
    const AVAIL_EVENT_SENTINEL: u16 = 0x55aa;

    // Four published entries keep has_new_avail() true after this
    // two-entry queue has processed one queue's worth of work.
    let (physmap, queue, block) = make_get_id_queue(2, 4);
    let mut queues = [queue];
    let avail_event_gpa =
        queues[0].used_addr() + 4 + u64::from(queues[0].size()) * 8;
    physmap
        .lookup(avail_event_gpa, 2)
        .expect("mapped avail event")
        .write::<u16>(&AVAIL_EVENT_SENTINEL)
        .expect("write avail event sentinel");

    block.notify_queue(0, &mut queues, &physmap);

    assert_eq!(queues[0].last_avail_idx(), queues[0].size());
    assert!(queues[0].has_new_avail(&physmap));
    assert_eq!(queues[0].read_used_ring_idx(&physmap), queues[0].size());
    assert_eq!(
        physmap
            .lookup(avail_event_gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event"),
        AVAIL_EVENT_SENTINEL,
    );
}

#[test]
fn notify_queue_drains_ring_and_updates_avail_event() {
    let (physmap, queue, block) = make_get_id_queue(4, 2);
    let mut queues = [queue];
    let avail_event_gpa =
        queues[0].used_addr() + 4 + u64::from(queues[0].size()) * 8;

    block.notify_queue(0, &mut queues, &physmap);

    assert_eq!(queues[0].last_avail_idx(), 2);
    assert_eq!(queues[0].read_used_ring_idx(&physmap), 2);
    assert_eq!(
        physmap
            .lookup(avail_event_gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event"),
        2,
    );
}

#[test]
fn device_id_returns_valid_20_bytes() {
    let blk = make_test_block(512 * 1024);
    let id = blk.device_id();
    assert_eq!(id.len(), VIRTIO_BLK_ID_BYTES);
    assert_eq!(id.len(), 20);
    // Should start with "vmm-virtio-blk"
    let s = std::str::from_utf8(&id[..14]).unwrap();
    assert_eq!(s, "vmm-virtio-blk");
    // Remaining bytes should be zero-padded
    for &b in &id[14..] {
        assert_eq!(b, 0);
    }
}

#[test]
fn config_size_returns_ext() {
    let blk = make_test_block(512 * 1024);
    assert_eq!(blk.config_size(), BLK_CONFIG_SIZE_EXT);
    assert_eq!(blk.config_size(), 60);
}

#[test]
fn device_features_includes_expected_bits() {
    let blk = make_test_block(512 * 1024);
    let f = blk.device_features();
    // Must have FLUSH, SIZE_MAX, SEG_MAX
    assert_ne!(f & bits::VIRTIO_BLK_F_FLUSH, 0, "FLUSH not set");
    assert_ne!(f & bits::VIRTIO_BLK_F_SIZE_MAX, 0, "SIZE_MAX not set");
    assert_ne!(f & bits::VIRTIO_BLK_F_SEG_MAX, 0, "SEG_MAX not set");
}

#[test]
fn device_features_includes_discard_write_zeroes() {
    let blk = make_test_block(512 * 1024);
    let f = blk.device_features();
    // Non-readonly device should have DISCARD and WRITE_ZEROES
    assert_ne!(f & bits::VIRTIO_BLK_F_DISCARD, 0, "DISCARD not set");
    assert_ne!(
        f & bits::VIRTIO_BLK_F_WRITE_ZEROES,
        0,
        "WRITE_ZEROES not set"
    );
}

#[test]
fn device_features_includes_event_idx_and_indirect() {
    let blk = make_test_block(512 * 1024);
    let f = blk.device_features();
    // EVENT_IDX and INDIRECT_DESC are included in device features.
    // Legacy transport strips them in pci.rs; modern advertises them.
    assert_ne!(
        f & bits::VIRTIO_F_RING_EVENT_IDX,
        0,
        "EVENT_IDX should be in device features"
    );
    assert_ne!(
        f & bits::VIRTIO_F_RING_INDIRECT_DESC,
        0,
        "INDIRECT_DESC should be in device features"
    );
}

#[test]
fn device_features_excludes_ro_for_writable() {
    let blk = make_test_block(512 * 1024);
    let f = blk.device_features();
    assert_eq!(
        f & bits::VIRTIO_BLK_F_RO,
        0,
        "RO should not be set for writable device"
    );
}

#[test]
fn device_features_includes_ro_for_readonly() {
    use std::io::Write;
    let mut tmpfile = tempfile::tempfile().expect("create tempfile");
    tmpfile.set_len(512 * 1024).expect("set file length");
    tmpfile.write_all(&[0u8; 1]).expect("write");
    tmpfile.flush().expect("flush");
    let physmap = Arc::new(PhysMap::new());
    let opts = VirtioBlockOpts {
        read_only: true,
        ..Default::default()
    };
    let blk = VirtioBlock::new(tmpfile, &opts, physmap, 1).unwrap();
    let f = blk.device_features();
    assert_ne!(
        f & bits::VIRTIO_BLK_F_RO,
        0,
        "RO should be set for readonly device"
    );
    // Readonly should NOT have DISCARD or WRITE_ZEROES
    assert_eq!(f & bits::VIRTIO_BLK_F_DISCARD, 0);
    assert_eq!(f & bits::VIRTIO_BLK_F_WRITE_ZEROES, 0);
}

#[test]
fn cfg_read_capacity() {
    // 512 * 1024 bytes / 512 sector_size = 1024 sectors
    let blk = make_test_block(512 * 1024);
    // Capacity is at config offset 0x00, 8 bytes (little-endian u64)
    let lo = blk.cfg_read(0x00, 4);
    let hi = blk.cfg_read(0x04, 4);
    let capacity = u64::from(lo) | (u64::from(hi) << 32);
    assert_eq!(capacity, 1024);
}

#[test]
fn cfg_read_size_max() {
    let blk = make_test_block(512 * 1024);
    // size_max at offset 0x08
    let val = blk.cfg_read(0x08, 4);
    assert_eq!(val, SIZE_MAX);
    assert_eq!(val, 128 * 1024);
}

#[test]
fn cfg_read_seg_max() {
    let blk = make_test_block(512 * 1024);
    // seg_max at offset 0x0C
    let val = blk.cfg_read(0x0C, 4);
    assert_eq!(val, SEG_MAX);
    assert_eq!(val, 126);
}

#[test]
fn cfg_read_blk_size() {
    let blk = make_test_block(512 * 1024);
    // blk_size at offset 0x14
    let val = blk.cfg_read(0x14, 4);
    assert_eq!(val, 512);
}

#[test]
fn cfg_read_out_of_range() {
    let blk = make_test_block(512 * 1024);
    // Reading past config space should return 0
    let val = blk.cfg_read(0xFF, 4);
    assert_eq!(val, 0);
}

#[test]
fn num_queues_single() {
    let blk = make_test_block(512 * 1024);
    assert_eq!(blk.num_queues(), 1);
}

#[test]
fn device_features_mq_only_with_multiple_queues() {
    use std::io::Write;
    // Single queue: MQ should not be set
    let blk_single = make_test_block(512 * 1024);
    assert_eq!(blk_single.device_features() & bits::VIRTIO_BLK_F_MQ, 0);

    // Multiple queues: MQ should be set
    let mut tmpfile = tempfile::tempfile().expect("create tempfile");
    tmpfile.set_len(512 * 1024).expect("set len");
    tmpfile.write_all(&[0u8; 1]).expect("write");
    tmpfile.flush().expect("flush");
    let physmap = Arc::new(PhysMap::new());
    let opts = VirtioBlockOpts::default();
    let blk_mq = VirtioBlock::new(tmpfile, &opts, physmap, 2).unwrap();
    assert_ne!(blk_mq.device_features() & bits::VIRTIO_BLK_F_MQ, 0);
}

// -----------------------------------------------------------------
// DISCARD / WRITE_ZEROES
// -----------------------------------------------------------------

const DWZ_DESC_GPA: u64 = 0x1000;
const DWZ_AVAIL_GPA: u64 = 0x1100;
const DWZ_USED_GPA: u64 = 0x1200;
const DWZ_HEADER_GPA: u64 = 0x2000;
const DWZ_PAYLOAD_GPA: u64 = 0x2100;
const DWZ_STATUS_GPA: u64 = 0x2200;
const DWZ_QUEUE_SIZE: u16 = 4;

/// One `struct virtio_blk_discard_write_zeroes` (VirtIO 1.3, 5.2.6.2).
fn encode_seg(sector: u64, num_sectors: u32, flags: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&sector.to_le_bytes());
    b[8..12].copy_from_slice(&num_sectors.to_le_bytes());
    b[12..16].copy_from_slice(&flags.to_le_bytes());
    b
}

fn poke(physmap: &PhysMap, gpa: u64, bytes: &[u8]) {
    physmap
        .lookup(gpa, bytes.len())
        .expect("mapped guest range")
        .write_bytes(bytes)
        .expect("write guest range");
}

/// One DISCARD or WRITE_ZEROES request to drive through the device.
struct DwzCase<'a> {
    rtype: u32,
    segs: &'a [[u8; 16]],
    /// Backing file size in bytes.
    file_bytes: u64,
    /// Byte to pre-fill the backing file with. `None` leaves it sparse.
    fill: Option<u8>,
    read_only: bool,
    nodelete: bool,
    /// Mark the payload descriptor device-writable, which is illegal.
    payload_writable: bool,
}

impl Default for DwzCase<'_> {
    fn default() -> Self {
        Self {
            rtype: bits::VIRTIO_BLK_T_WRITE_ZEROES,
            segs: &[],
            file_bytes: 8 * 512,
            fill: Some(0xAB),
            read_only: false,
            nodelete: false,
            payload_writable: false,
        }
    }
}

struct DwzHarness {
    physmap: Arc<PhysMap>,
    queues: [VirtQueue; 1],
    block: VirtioBlock,
    backing: File,
}

/// Build a one-request ring carrying the case's payload.
fn dwz_harness(case: &DwzCase<'_>) -> DwzHarness {
    use std::io::Write;

    let physmap =
        Arc::new(PhysMap::new_anon(DWZ_DESC_GPA, 0x4000).expect("queue mem"));
    let mut queue = VirtQueue::new(DWZ_QUEUE_SIZE);
    queue.set_addr_modern(DWZ_DESC_GPA, DWZ_AVAIL_GPA, DWZ_USED_GPA);
    queue.set_event_idx(true);

    let header = VirtioBlkReqHdr {
        rtype: case.rtype,
        _reserved: 0,
        sector: 0,
    };
    physmap
        .lookup(DWZ_HEADER_GPA, BLK_REQ_HEADER_SIZE)
        .expect("mapped request header")
        .write(&header)
        .expect("write request header");

    let payload: Vec<u8> = case.segs.concat();
    if !payload.is_empty() {
        poke(&physmap, DWZ_PAYLOAD_GPA, &payload);
    }
    poke(&physmap, DWZ_STATUS_GPA, &[0xFF]);

    let payload_flags = if case.payload_writable {
        bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE
    } else {
        bits::VRING_DESC_F_NEXT
    };
    let descs = [
        VirtqDesc {
            addr: DWZ_HEADER_GPA,
            len: BLK_REQ_HEADER_SIZE as u32,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        },
        VirtqDesc {
            addr: DWZ_PAYLOAD_GPA,
            len: payload.len() as u32,
            flags: payload_flags,
            next: 2,
        },
        VirtqDesc {
            addr: DWZ_STATUS_GPA,
            len: 1,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        },
    ];
    for (i, desc) in descs.iter().enumerate() {
        physmap
            .lookup(DWZ_DESC_GPA + (i as u64) * 16, 16)
            .expect("mapped descriptor")
            .write(desc)
            .expect("write descriptor");
    }

    poke(&physmap, DWZ_AVAIL_GPA + 2, &1u16.to_le_bytes());
    poke(&physmap, DWZ_AVAIL_GPA + 4, &0u16.to_le_bytes());

    let mut file = tempfile::tempfile().expect("create tempfile");
    match case.fill {
        Some(byte) => {
            file.write_all(&vec![byte; case.file_bytes as usize])
                .expect("fill backing file");
            file.flush().expect("flush backing file");
        }
        None => file.set_len(case.file_bytes).expect("size backing file"),
    }
    let backing = file.try_clone().expect("clone backing file");

    let opts = VirtioBlockOpts {
        read_only: case.read_only,
        nodelete: case.nodelete,
        ..Default::default()
    };
    let block = VirtioBlock::new(file, &opts, Arc::clone(&physmap), 1)
        .expect("create VirtioBlock");

    DwzHarness {
        physmap,
        queues: [queue],
        block,
        backing,
    }
}

/// Drive one request through and return its status byte. Worker
/// completions are asynchronous, so poll the used ring.
fn dwz_run(h: &mut DwzHarness) -> u8 {
    h.block.notify_queue(0, &mut h.queues, &h.physmap);
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.queues[0].read_used_ring_idx(&h.physmap) == 0 {
        assert!(Instant::now() < deadline, "request never completed");
        thread::yield_now();
    }
    let mut status = [0u8; 1];
    h.physmap
        .lookup(DWZ_STATUS_GPA, 1)
        .expect("mapped status byte")
        .read_bytes(&mut status)
        .expect("read status byte");
    status[0]
}

fn read_backing(h: &DwzHarness, len: usize) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    h.backing.read_exact_at(&mut buf, 0).expect("read back");
    buf
}

#[test]
fn write_zeroes_zeroes_the_backing_file() {
    // 8 sectors of 0xAB; zero sectors 2..6.
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(2, 4, 0)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_OK);

    let buf = read_backing(&h, 8 * 512);
    assert!(buf[..1024].iter().all(|&b| b == 0xAB), "head clobbered");
    assert!(
        buf[1024..3072].iter().all(|&b| b == 0),
        "WRITE_ZEROES did not zero sectors 2..6"
    );
    assert!(buf[3072..].iter().all(|&b| b == 0xAB), "tail clobbered");
}

#[test]
fn write_zeroes_honours_every_segment() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 1, 0), encode_seg(7, 1, 0)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_OK);

    let buf = read_backing(&h, 8 * 512);
    assert!(buf[..512].iter().all(|&b| b == 0), "sector 0 not zeroed");
    assert!(
        buf[512..3584].iter().all(|&b| b == 0xAB),
        "middle clobbered"
    );
    assert!(buf[3584..].iter().all(|&b| b == 0), "sector 7 not zeroed");
}

#[test]
fn write_zeroes_with_unmap_still_zeroes() {
    // UNMAP lets the device deallocate as well. It never relaxes the
    // guarantee that the range reads back as zeros.
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 8, 1)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_OK);
    assert!(read_backing(&h, 8 * 512).iter().all(|&b| b == 0));
}

#[test]
fn write_zeroes_rejects_reserved_flag() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 1, 0x2)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_UNSUPP);
    assert!(read_backing(&h, 512).iter().all(|&b| b == 0xAB));
}

#[test]
fn discard_rejects_the_unmap_flag() {
    // UNMAP is defined for WRITE_ZEROES only.
    let mut h = dwz_harness(&DwzCase {
        rtype: bits::VIRTIO_BLK_T_DISCARD,
        segs: &[encode_seg(0, 1, 1)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_UNSUPP);
}

#[test]
fn write_zeroes_rejects_a_sector_past_capacity() {
    // Capacity is 8 sectors, so 4 + 5 runs off the end.
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(4, 5, 0)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
    assert!(read_backing(&h, 8 * 512).iter().all(|&b| b == 0xAB));
}

#[test]
fn write_zeroes_rejects_a_sector_count_that_overflows() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(u64::MAX, 1, 0)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
}

#[test]
fn discard_rejects_a_sector_past_capacity() {
    let mut h = dwz_harness(&DwzCase {
        rtype: bits::VIRTIO_BLK_T_DISCARD,
        segs: &[encode_seg(8, 1, 0)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
}

#[test]
fn write_zeroes_rejects_more_sectors_than_advertised() {
    let over = discard::MAX_WRITE_ZEROES_SECTORS + 1;
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, over, 0)],
        // Big enough that only the advertised limit can refuse this.
        file_bytes: u64::from(over) * 512,
        fill: None,
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
}

#[test]
fn write_zeroes_rejects_more_segments_than_advertised() {
    let segs: Vec<[u8; 16]> = (0..=discard::MAX_SEG)
        .map(|_| encode_seg(0, 1, 0))
        .collect();
    let mut h = dwz_harness(&DwzCase {
        segs: &segs,
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
    assert!(read_backing(&h, 512).iter().all(|&b| b == 0xAB));
}

#[test]
fn write_zeroes_rejects_a_truncated_payload() {
    let mut short = [0u8; 16];
    short[..8].copy_from_slice(&0u64.to_le_bytes());
    let mut h = dwz_harness(&DwzCase {
        segs: &[short],
        ..Default::default()
    });
    // Shrink the payload descriptor to half a segment.
    poke(&h.physmap, DWZ_DESC_GPA + 16 + 8, &8u32.to_le_bytes());
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
}

#[test]
fn write_zeroes_rejects_a_device_writable_payload() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 1, 0)],
        payload_writable: true,
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
    assert!(read_backing(&h, 512).iter().all(|&b| b == 0xAB));
}

#[test]
fn discard_is_a_validated_no_op() {
    // VirtIO 1.3 sec 5.2.6.2 leaves the data unspecified after DISCARD,
    // so leaving the backing store alone is legal.
    let mut h = dwz_harness(&DwzCase {
        rtype: bits::VIRTIO_BLK_T_DISCARD,
        segs: &[encode_seg(0, 8, 0)],
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_OK);
    assert!(read_backing(&h, 8 * 512).iter().all(|&b| b == 0xAB));
}

#[test]
fn read_only_disk_refuses_write_zeroes() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 8, 0)],
        read_only: true,
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_IOERR);
    assert!(read_backing(&h, 8 * 512).iter().all(|&b| b == 0xAB));
}

#[test]
fn nodelete_disk_refuses_discard_but_still_zeroes() {
    let mut h = dwz_harness(&DwzCase {
        rtype: bits::VIRTIO_BLK_T_DISCARD,
        segs: &[encode_seg(0, 8, 0)],
        nodelete: true,
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_UNSUPP);

    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 8, 0)],
        nodelete: true,
        ..Default::default()
    });
    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_OK);
    assert!(read_backing(&h, 8 * 512).iter().all(|&b| b == 0));
}

#[test]
fn nodelete_disk_advertises_write_zeroes_only() {
    let file = tempfile::tempfile().expect("create tempfile");
    file.set_len(512 * 1024).expect("set file length");
    let opts = VirtioBlockOpts {
        nodelete: true,
        ..Default::default()
    };
    let blk = VirtioBlock::new(file, &opts, Arc::new(PhysMap::new()), 1)
        .expect("create VirtioBlock");
    let f = blk.device_features();
    assert_eq!(f & bits::VIRTIO_BLK_F_DISCARD, 0);
    assert_ne!(f & bits::VIRTIO_BLK_F_WRITE_ZEROES, 0);
    // A feature that is not offered must leave its config fields zero.
    assert_eq!(blk.cfg_read(0x24, 4), 0);
    assert_eq!(blk.cfg_read(0x28, 4), 0);
    assert_eq!(blk.cfg_read(0x2C, 4), 0);
    assert_ne!(blk.cfg_read(0x30, 4), 0);
}

#[test]
fn cfg_read_publishes_the_advertised_discard_limits() {
    let blk = make_test_block(512 * 1024);
    // Linux reads a zero limit as "unlimited", so none may be zero.
    assert_ne!(discard::MAX_DISCARD_SECTORS, 0);
    assert_ne!(discard::MAX_WRITE_ZEROES_SECTORS, 0);
    assert_ne!(discard::MAX_SEG, 0);

    // VirtIO 1.3 sec 5.2.4 field offsets.
    assert_eq!(blk.cfg_read(0x24, 4), discard::MAX_DISCARD_SECTORS);
    assert_eq!(blk.cfg_read(0x28, 4), discard::MAX_SEG);
    assert_eq!(blk.cfg_read(0x2C, 4), 1);
    assert_eq!(blk.cfg_read(0x30, 4), discard::MAX_WRITE_ZEROES_SECTORS);
    assert_eq!(blk.cfg_read(0x34, 4), discard::MAX_SEG);
    // We write zeros instead of deallocating.
    assert_eq!(blk.cfg_read(0x38, 1), 0);
    // The config must not run past what config_size() reports.
    assert!(u16::from(0x38u8) < blk.config_size());
}

#[test]
fn cfg_read_discard_alignment_tracks_sector_size() {
    let file = tempfile::tempfile().expect("create tempfile");
    file.set_len(512 * 1024).expect("set file length");
    let opts = VirtioBlockOpts {
        sector_size: 4096,
        ..Default::default()
    };
    let blk = VirtioBlock::new(file, &opts, Arc::new(PhysMap::new()), 1)
        .expect("create VirtioBlock");
    // discard_sector_alignment is counted in 512-byte sectors.
    assert_eq!(blk.cfg_read(0x2C, 4), 8);
}

#[test]
fn read_only_disk_publishes_no_discard_limits() {
    let file = tempfile::tempfile().expect("create tempfile");
    file.set_len(512 * 1024).expect("set file length");
    let opts = VirtioBlockOpts {
        read_only: true,
        ..Default::default()
    };
    let blk = VirtioBlock::new(file, &opts, Arc::new(PhysMap::new()), 1)
        .expect("create VirtioBlock");
    let f = blk.device_features();
    assert_eq!(f & bits::VIRTIO_BLK_F_DISCARD, 0);
    assert_eq!(f & bits::VIRTIO_BLK_F_WRITE_ZEROES, 0);
    for off in [0x24u16, 0x28, 0x2C, 0x30, 0x34, 0x38] {
        assert_eq!(blk.cfg_read(off, 4), 0, "offset {off:#x} not zero");
    }
}

/// A chain the walker refuses must still go back on the used ring.
/// `pop_avail` already took its head out of the available ring, so
/// dropping it burns one descriptor for the life of the device: the
/// block layer has no abort path to recover it, and a guest that can
/// drive enough refusals stalls its own disk.
#[test]
fn a_refused_chain_returns_its_descriptor() {
    const DESC_GPA: u64 = 0x1000;
    const USED_GPA: u64 = 0x1200;

    let (physmap, queue, block) = make_get_id_queue(4, 1);
    // NEXT past the end of the descriptor table: the walker refuses it.
    physmap
        .lookup(DESC_GPA, 16)
        .expect("mapped header descriptor")
        .write(&VirtqDesc {
            addr: 0x2000,
            len: BLK_REQ_HEADER_SIZE as u32,
            flags: bits::VRING_DESC_F_NEXT,
            next: 9,
        })
        .expect("write header descriptor");
    let mut queues = [queue];

    block.notify_queue(0, &mut queues, &physmap);

    assert_eq!(
        queues[0].read_used_ring_idx(&physmap),
        1,
        "the refused head never went back to the guest",
    );
    let entry = physmap.lookup(USED_GPA + 4, 8).expect("mapped used entry");
    assert_eq!(entry.read::<u32>().expect("used id"), 0);
    assert_eq!(
        entry
            .subregion(4, 4)
            .expect("used length")
            .read::<u32>()
            .expect("used length"),
        0,
    );
}
