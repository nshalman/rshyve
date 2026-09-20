// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the virtio entropy device.

use super::*;
use crate::bits;
use crate::queue::VirtqDesc;

const REGION_GPA: u64 = 0x1_0000;
const REGION_SIZE: usize = 0x20_0000;
const DESC_GPA: u64 = REGION_GPA;
const AVAIL_GPA: u64 = REGION_GPA + 0x400;
const USED_GPA: u64 = REGION_GPA + 0x600;
const DATA_GPA: u64 = REGION_GPA + 0x1000;
/// Outside the one region the tests map.
const UNMAPPED_GPA: u64 = 0x8000_0000;

const QUEUE_SIZE: u16 = 64;

/// A queue the device will accept, with guest memory to write into.
fn rng_queue() -> (PhysMap, VirtQueue) {
    let physmap =
        PhysMap::new_anon(REGION_GPA, REGION_SIZE).expect("guest memory");
    let mut queue = VirtQueue::new(QUEUE_SIZE);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    queue.set_event_idx(true);
    (physmap, queue)
}

fn write_desc(
    physmap: &PhysMap,
    idx: u64,
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
) {
    physmap
        .lookup(DESC_GPA + idx * 16, 16)
        .expect("mapped descriptor")
        .write(&VirtqDesc {
            addr,
            len,
            flags,
            next,
        })
        .expect("write descriptor");
}

/// Point the first `count` available entries at descriptor `desc` and
/// publish them.
fn publish(physmap: &PhysMap, desc: u16, count: u16) {
    for slot in 0..count {
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(slot) * 2, 2)
            .expect("mapped avail entry")
            .write::<u16>(&desc)
            .expect("write avail entry");
    }
    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&count)
        .expect("write avail index");
}

fn used_idx(physmap: &PhysMap) -> u16 {
    physmap
        .lookup(USED_GPA + 2, 2)
        .expect("mapped used index")
        .read::<u16>()
        .expect("read used index")
}

fn used_len(physmap: &PhysMap, slot: u64) -> u32 {
    physmap
        .lookup(USED_GPA + 4 + slot * 8 + 4, 4)
        .expect("mapped used entry")
        .read::<u32>()
        .expect("read used length")
}

fn read_bytes(physmap: &PhysMap, gpa: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    physmap
        .lookup(gpa, len)
        .expect("mapped region")
        .read_bytes(&mut buf)
        .expect("read region");
    buf
}

fn fill_bytes(physmap: &PhysMap, gpa: u64, byte: u8, len: usize) {
    physmap
        .lookup(gpa, len)
        .expect("mapped region")
        .write_bytes(&vec![byte; len])
        .expect("write region");
}

#[test]
fn features_include_event_idx_and_indirect() {
    let f = VirtioRng::new().device_features();
    assert_ne!(f & bits::VIRTIO_F_RING_EVENT_IDX, 0);
    assert_ne!(f & bits::VIRTIO_F_RING_INDIRECT_DESC, 0);
}

#[test]
fn config_read_returns_zero() {
    let rng = VirtioRng::new();
    assert_eq!(rng.cfg_read(0, 4), 0);
    assert_eq!(rng.cfg_read(4, 1), 0);
}

#[test]
fn a_writable_buffer_comes_back_filled() {
    const LEN: usize = 64;
    let (physmap, mut queue) = rng_queue();
    write_desc(
        &physmap,
        0,
        DATA_GPA,
        LEN as u32,
        bits::VRING_DESC_F_WRITE,
        0,
    );
    publish(&physmap, 0, 1);
    fill_bytes(&physmap, DATA_GPA, 0, LEN);

    let head = queue.pop_avail(&physmap).expect("one available chain");
    let written = VirtioRng::new().process_queue(0, &mut queue, head, &physmap);

    assert_eq!(written, LEN as u32);
    assert!(
        read_bytes(&physmap, DATA_GPA, LEN).iter().any(|&b| b != 0),
        "the device left the buffer as it found it",
    );
}

/// A guest names the buffers, so it can put one the device cannot map
/// in the middle of a chain. Generating entropy for it is host work
/// with no guest effect, and walking on past it writes buffers the
/// driver will never be told about, because the used length stops at
/// the gap.
#[test]
fn a_chain_stops_at_an_unmapped_buffer() {
    const LEN: usize = 64;
    const TAIL_GPA: u64 = DATA_GPA + 0x1000;
    let (physmap, mut queue) = rng_queue();

    write_desc(
        &physmap,
        0,
        DATA_GPA,
        LEN as u32,
        bits::VRING_DESC_F_WRITE | bits::VRING_DESC_F_NEXT,
        1,
    );
    write_desc(
        &physmap,
        1,
        UNMAPPED_GPA,
        LEN as u32,
        bits::VRING_DESC_F_WRITE | bits::VRING_DESC_F_NEXT,
        2,
    );
    write_desc(
        &physmap,
        2,
        TAIL_GPA,
        LEN as u32,
        bits::VRING_DESC_F_WRITE,
        0,
    );
    publish(&physmap, 0, 1);
    fill_bytes(&physmap, TAIL_GPA, 0xAA, LEN);

    let head = queue.pop_avail(&physmap).expect("one available chain");
    let written = VirtioRng::new().process_queue(0, &mut queue, head, &physmap);

    assert_eq!(written, LEN as u32, "the walk went past the gap");
    assert!(
        read_bytes(&physmap, TAIL_GPA, LEN).iter().all(|&b| b == 0xAA),
        "a buffer behind the gap was written",
    );
}

/// One chain may name as many descriptors as the ring holds, each with
/// a guest `u32` length, so without a bound one chain asks for
/// terabytes of entropy on the vCPU thread.
#[test]
fn one_chain_asks_for_no_more_than_its_byte_budget() {
    let len = (BYTES_PER_CHAIN * 4) as u32;
    let (physmap, mut queue) = rng_queue();
    write_desc(&physmap, 0, DATA_GPA, len, bits::VRING_DESC_F_WRITE, 0);
    publish(&physmap, 0, 1);

    let head = queue.pop_avail(&physmap).expect("one available chain");
    let written = VirtioRng::new().process_queue(0, &mut queue, head, &physmap);

    assert_eq!(written, BYTES_PER_CHAIN as u32);
}

/// The whole drain runs inline on the vCPU under the transport lock, so
/// the chains of one kick share one budget. Every head still comes back
/// on the used ring: the ring has to drain either way, and a chain past
/// the budget comes back with a used length of zero, which is how the
/// device tells the driver to ask again.
#[test]
fn a_kick_serves_no_more_than_its_byte_budget() {
    let chains = (BYTES_PER_KICK / BYTES_PER_CHAIN + 4) as u16;
    let (physmap, mut queue) = rng_queue();
    write_desc(
        &physmap,
        0,
        DATA_GPA,
        BYTES_PER_CHAIN as u32,
        bits::VRING_DESC_F_WRITE,
        0,
    );
    publish(&physmap, 0, chains);

    let mut queues = [queue];
    assert!(VirtioRng::new().notify_queue(0, &mut queues, &physmap));
    queue = queues.into_iter().next().expect("the queue back");

    assert_eq!(used_idx(&physmap), chains, "the ring did not drain");
    let served: u64 = (0..u64::from(chains))
        .map(|slot| u64::from(used_len(&physmap, slot)))
        .sum();
    assert_eq!(served, BYTES_PER_KICK as u64);
    assert!(!queue.needs_reset());
}
