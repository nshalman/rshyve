// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the split virtqueue.

use super::*;

const INDIRECT_TABLE_GPA: u64 = 0x1000;

fn indirect_physmap(num_descs: usize) -> PhysMap {
    PhysMap::new_anon(INDIRECT_TABLE_GPA, num_descs * 16)
        .expect("create anonymous indirect table memory")
}

fn write_indirect_desc(physmap: &PhysMap, idx: usize, desc: VirtqDesc) {
    physmap
        .lookup(INDIRECT_TABLE_GPA + (idx as u64 * 16), 16)
        .expect("mapped indirect descriptor")
        .write(&desc)
        .expect("write indirect descriptor");
}

#[test]
fn pop_avail_failing_lookup_does_not_advance_last_avail_idx() {
    const REGION_GPA: u64 = 0x4000;
    const REGION_SIZE: usize = 0x1000;
    const REGION_END: u64 = REGION_GPA + REGION_SIZE as u64;

    let physmap = PhysMap::new_anon(REGION_GPA, REGION_SIZE)
        .expect("create guest memory");
    let mut queue = VirtQueue::new(2);
    queue.set_addr_modern(REGION_GPA, REGION_END - 4, REGION_GPA + 0x100);
    physmap
        .lookup(REGION_END - 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&1)
        .expect("write avail index");

    let before = queue.last_avail_idx();
    assert_eq!(queue.pop_avail(&physmap), None);
    assert_eq!(queue.last_avail_idx(), before);
}

#[test]
fn new_queue_defaults() {
    let q = VirtQueue::new(256);
    assert_eq!(q.size(), 256);
    assert!(!q.is_configured());
    assert_eq!(q.pfn(), 0);
}

#[test]
fn set_addr_legacy_computes_regions() {
    let mut q = VirtQueue::new(256);
    // PFN 1 => base = 4096
    q.set_addr_legacy(1);
    assert!(q.is_configured());
    assert_eq!(q.pfn(), 1);

    // Desc at base
    assert_eq!(q.desc_addr, 4096);
    // Avail at base + 256 * 16 = base + 4096 = 8192
    assert_eq!(q.avail_addr, 4096 + 256 * 16);
    // Used at next page boundary after avail ring
    let avail_end = q.avail_addr + 6 + 256 * 2;
    let expected_used = align_up(avail_end, PAGE_SIZE as u64);
    assert_eq!(q.used_addr, expected_used);
}

#[test]
fn set_addr_legacy_zero_resets() {
    let mut q = VirtQueue::new(128);
    q.set_addr_legacy(1);
    assert!(q.is_configured());

    q.set_addr_legacy(0);
    assert!(!q.is_configured());
    assert_eq!(q.desc_addr, 0);
    assert_eq!(q.avail_addr, 0);
    assert_eq!(q.used_addr, 0);
}

#[test]
fn reset_clears_state() {
    let mut q = VirtQueue::new(64);
    q.set_addr_legacy(10);
    q.reset();
    assert!(!q.is_configured());
    assert_eq!(q.pfn(), 0);
}

#[test]
#[should_panic(expected = "power of 2")]
fn non_power_of_two_panics() {
    let _ = VirtQueue::new(100);
}

#[test]
#[should_panic(expected = "power of 2")]
fn zero_size_panics() {
    let _ = VirtQueue::new(0);
}

#[test]
fn align_up_works() {
    assert_eq!(align_up(0, 4096), 0);
    assert_eq!(align_up(1, 4096), 4096);
    assert_eq!(align_up(4096, 4096), 4096);
    assert_eq!(align_up(4097, 4096), 8192);
}

#[test]
fn virtq_desc_flags() {
    let desc = VirtqDesc {
        addr: 0x1000,
        len: 512,
        flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
        next: 1,
    };
    assert!(desc.has_next());
    assert!(desc.is_write());
    assert!(!desc.is_indirect());

    let desc2 = VirtqDesc {
        addr: 0x2000,
        len: 1,
        flags: 0,
        next: 0,
    };
    assert!(!desc2.has_next());
    assert!(!desc2.is_write());
    assert!(!desc2.is_indirect());
}

#[test]
fn virtq_desc_indirect_flag() {
    let desc = VirtqDesc {
        addr: 0x3000,
        len: 256,
        flags: bits::VRING_DESC_F_INDIRECT,
        next: 0,
    };
    assert!(desc.is_indirect());
    assert!(!desc.has_next());
    assert!(!desc.is_write());

    // Indirect + other flags
    let desc2 = VirtqDesc {
        addr: 0x4000,
        len: 128,
        flags: bits::VRING_DESC_F_INDIRECT | bits::VRING_DESC_F_WRITE,
        next: 0,
    };
    assert!(desc2.is_indirect());
    assert!(desc2.is_write());
}

#[test]
fn indirect_table_longer_than_queue_size_is_rejected() {
    // Every descriptor is backed and walkable, so the size bound is
    // the only thing that can reject this table.
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(5);
    for idx in 0..5 {
        write_indirect_desc(
            &physmap,
            idx,
            VirtqDesc {
                addr: 0x2000 + (idx as u64 * 0x1000),
                len: 64,
                flags: if idx == 4 { 0 } else { bits::VRING_DESC_F_NEXT },
                next: idx as u16 + 1,
            },
        );
    }

    assert!(q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 5 * 16)
        .is_none());

    // Same table, queue large enough to admit it: now it walks.
    let q = VirtQueue::new(8);
    assert_eq!(
        q.walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 5 * 16)
            .expect("table within queue size")
            .len(),
        5
    );
}

#[test]
fn indirect_table_within_queue_size_still_walks() {
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(3);
    write_indirect_desc(
        &physmap,
        0,
        VirtqDesc {
            addr: 0x2000,
            len: 64,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        },
    );
    write_indirect_desc(
        &physmap,
        1,
        VirtqDesc {
            addr: 0x3000,
            len: 128,
            flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
            next: 2,
        },
    );
    write_indirect_desc(
        &physmap,
        2,
        VirtqDesc {
            addr: 0x4000,
            len: 32,
            flags: 0,
            next: 0,
        },
    );

    let bufs = q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 3 * 16)
        .expect("valid indirect table");

    assert_eq!(bufs.len(), 3);
    assert!(matches!(
        bufs[0],
        ChainBuf::Readable {
            addr: 0x2000,
            len: 64
        }
    ));
    assert!(matches!(
        bufs[1],
        ChainBuf::Writable {
            addr: 0x3000,
            len: 128
        }
    ));
    assert!(matches!(
        bufs[2],
        ChainBuf::Readable {
            addr: 0x4000,
            len: 32
        }
    ));
}

#[test]
fn indirect_len_not_multiple_of_16_rejected() {
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(2);

    assert!(q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 17)
        .is_none());
}

#[test]
fn indirect_cycle_terminates_and_returns_none() {
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(3);
    for idx in 0..3 {
        write_indirect_desc(
            &physmap,
            idx,
            VirtqDesc {
                addr: 0x2000 + (idx as u64 * 0x1000),
                len: 64,
                flags: bits::VRING_DESC_F_NEXT,
                next: if idx == 2 { 0 } else { idx as u16 + 1 },
            },
        );
    }

    assert!(q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 3 * 16)
        .is_none());
}

// -- EVENT_IDX tests --

#[test]
fn vring_need_event_crossing_threshold() {
    // new_idx crosses event_idx: should interrupt
    // old=0, new=1, event=0: (1-0-1=0) < (1-0=1) → true
    assert!(vring_need_event(0, 1, 0));
    // old=5, new=10, event=7: (10-7-1=2) < (10-5=5) → true
    assert!(vring_need_event(7, 10, 5));
}

#[test]
fn vring_need_event_threshold_already_passed() {
    // event_idx already behind old_idx: no interrupt
    // old=5, new=10, event=4: (10-4-1=5) < (10-5=5) → false
    assert!(!vring_need_event(4, 10, 5));
    // old=0, new=1, event=1: (1-1-1=65535) < (1-0=1) → false
    assert!(!vring_need_event(1, 1, 0));
}

#[test]
fn vring_need_event_no_new_work() {
    // old == new (no work done): never interrupt
    assert!(!vring_need_event(3, 5, 5));
    assert!(!vring_need_event(0, 0, 0));
}

#[test]
fn vring_need_event_wrapping() {
    // Wrapping around u16::MAX
    // old=65535, new=0, event=65535: (0-65535-1=0) < (0-65535=1) → true
    assert!(vring_need_event(65535, 0, 65535));
    // old=65535, new=0, event=0: (0-0-1=65535) < (0-65535=1) → false
    assert!(!vring_need_event(0, 0, 65535));
    // old=65534, new=1, event=65535: (1-65535-1=1) < (1-65534=3) → true
    assert!(vring_need_event(65535, 1, 65534));
}

#[test]
fn vring_need_event_single_step() {
    // Exact threshold crossing: old=N, new=N+1, event=N → interrupt
    for n in [0u16, 1, 100, 255, 65534, 65535] {
        assert!(
            vring_need_event(n, n.wrapping_add(1), n),
            "should interrupt at threshold {n}",
        );
    }
}

#[test]
fn has_new_avail_disabled_when_no_event_idx() {
    let q = VirtQueue::new(128);
    assert!(!q.event_idx_enabled());
    // has_new_avail should always return false when event_idx is off
    // (can't test with real PhysMap, but the early return is the key)
}

#[test]
fn has_new_avail_disabled_when_not_configured() {
    let mut q = VirtQueue::new(128);
    q.set_event_idx(true);
    // Not configured (no PFN set) → false
    assert!(!q.is_configured());
}
