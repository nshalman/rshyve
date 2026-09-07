// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Used ring completion for asynchronous virtio devices.

use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use vmm_core::mem::PhysMap;

use super::{bits, vring_need_event, VirtQueue};
use crate::pci::intr::IntrSession;

/// Thread-safe completion handler for async virtio I/O.
///
/// Exclusively owns the used ring write index for one virtqueue.
/// ALL completions, inline fast-path and async workers alike, MUST go
/// through this struct. Never mix with `VirtQueue::push_used()`.
///
/// # Design (Propolis `Mutex<VqUsed>` pattern)
///
/// - `Mutex<u16>` serializes used ring writes so entry data is
///   always committed before the index advances
/// - Release fence before index write ensures the guest sees the
///   entry data before the updated index
/// - Interrupt callback fires after the Mutex is released
///
/// # Performance
///
/// - Uncontended Mutex: ~10ns (single atomic CAS)
/// - Zero heap allocation per I/O
/// - Batch mode: one interrupt for N completions
///
/// # Reuse
///
/// Generic for any async virtio device (block, scsi, net, 9p).
/// Create one per virtqueue via `VirtioCompletion::new()`.
pub struct VirtioCompletion {
    used_addr: u64,
    avail_addr: u64,
    queue_size: u16,
    write_idx: Mutex<u16>,
    physmap: Arc<PhysMap>,
    /// The transport session this handler was built in.
    ///
    /// A device drops its handlers in its reset and builds the next
    /// one inside the session that follows, so the value here is the
    /// session every used entry this handler publishes belongs to.
    /// [`Self::signal`] hands it to the transport, which refuses a
    /// raise for a session it has ended.
    session: IntrSession,
    interrupt: Box<dyn Fn(IntrSession) + Send + Sync>,
    /// Whether VIRTIO_F_RING_EVENT_IDX is negotiated.
    event_idx: bool,
    /// Force-interrupt counter: always fire interrupt for the first
    /// N completions. Used after migration to ensure the guest
    /// processes completions even when EVENT_IDX would suppress them.
    force_intr_count: AtomicU32,
}

impl VirtioCompletion {
    /// Create a completion handler for a configured virtqueue.
    ///
    /// Reads the current used index from guest memory to establish
    /// the starting write position. From this point forward, this
    /// struct exclusively owns the used ring write index.
    pub fn new(
        queue: &VirtQueue,
        physmap: Arc<PhysMap>,
        session: IntrSession,
        interrupt: impl Fn(IntrSession) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let (used_addr, queue_size) = queue.used_ring_info();
        let avail_addr = queue.avail_addr();
        let idx_gpa = used_addr + 2;
        let initial_idx = physmap
            .lookup(idx_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0);

        let event_idx = queue.event_idx_enabled();
        Arc::new(Self {
            used_addr,
            avail_addr,
            queue_size,
            write_idx: Mutex::new(initial_idx),
            physmap,
            session,
            interrupt: Box::new(interrupt),
            event_idx,
            force_intr_count: AtomicU32::new(0),
        })
    }

    /// Complete one request: write used entry, raise interrupt if needed.
    ///
    /// Safe from any thread (vCPU inline path or I/O worker).
    #[inline]
    pub fn complete(&self, desc_idx: u16, len: u32) {
        if self.publish_batch(&[(desc_idx, len)]) {
            self.signal();
        }
    }

    /// Publish used entries and report whether the guest wants an
    /// interrupt.
    ///
    /// Every step here touches guest memory: the used entries, the
    /// used index, and the driver's interrupt-suppression fields. A
    /// device that guards guest access must hold that permission
    /// across the whole call, and must leave [`Self::signal`] outside
    /// it.
    ///
    /// The Mutex is held across all writes so no thread can interleave.
    ///
    /// When `event_idx` is negotiated, reads `used_event` from the
    /// guest to determine if an interrupt is needed. This suppresses
    /// redundant interrupts: the guest only wants an interrupt when
    /// the device has completed past the index the guest last saw.
    pub fn publish_batch(&self, entries: &[(u16, u32)]) -> bool {
        if entries.is_empty() {
            return false;
        }
        let old_idx;
        let new_idx;
        {
            let mut idx = self.write_idx.lock().expect("completion lock");
            old_idx = *idx;
            for &(desc_idx, len) in entries {
                self.write_used_entry(&mut idx, desc_idx, len);
            }
            new_idx = *idx;
        }
        self.should_interrupt(old_idx, new_idx)
    }

    /// Raise the interrupt a publish asked for.
    ///
    /// Kept apart from publishing so a device can deliver outside the
    /// permission that wrote the ring. Delivery can block, and VirtIO
    /// 1.3 sec 2.4.1 forbids queue interaction once a reset is
    /// complete. The raise therefore names the session this handler
    /// belongs to rather than the one running now: a thread parked
    /// here across a whole reset is refused by the transport instead
    /// of interrupting the driver that followed.
    #[inline]
    pub fn signal(&self) {
        (self.interrupt)(self.session);
    }

    /// Force the next N completions to always fire an interrupt,
    /// bypassing EVENT_IDX suppression. Used after migration to
    /// ensure the guest processes stale used entries.
    pub fn set_force_interrupt(&self, count: u32) {
        self.force_intr_count.store(count, Ordering::Release);
    }

    /// Check if the guest needs an interrupt.
    ///
    /// With event_idx: only interrupt when `new_idx` crosses the
    /// `avail_event` threshold set by the guest. Uses the VirtIO 1.3
    /// wrapping comparison from spec sec 2.7.7.1.
    ///
    /// Without event_idx: always interrupt (no suppression).
    fn should_interrupt(&self, old_idx: u16, new_idx: u16) -> bool {
        // Force-interrupt mode: always fire for the first N completions.
        // Claimed with one read-modify-write, because two threads that
        // both read the last count and both subtract it wrap the
        // counter to u32::MAX and force every completion after that.
        let forced = self
            .force_intr_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_sub(1)
            })
            .is_ok();
        if forced {
            return true;
        }
        if !self.event_idx {
            // Without EVENT_IDX, check the avail ring flags field.
            if self.avail_addr != 0 {
                if let Some(sub) = self.physmap.lookup(self.avail_addr, 2) {
                    if let Ok(flags) = sub.read::<u16>() {
                        return flags & bits::VIRTQ_AVAIL_F_NO_INTERRUPT == 0;
                    }
                }
            }
            return true;
        }

        // Full barrier: ensure our used_idx write (in write_used_entry)
        // is visible to the guest before we read used_event. Without
        // this, a Store-Load reorder could cause us to read a stale
        // used_event and skip an interrupt the guest expects.
        // QEMU: smp_mb() in virtio_split_should_notify().
        fence(Ordering::SeqCst);

        // Read used_event from the avail ring (driver's threshold).
        // Layout: flags(2) + idx(2) + ring(2*N) + used_event(2)
        let used_event_gpa =
            self.avail_addr + 4 + u64::from(self.queue_size) * 2;
        let used_event = match self
            .physmap
            .lookup(used_event_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(v) => v,
            None => return true, // Can't read → always interrupt (safe)
        };

        // VirtIO 1.3 sec 2.7.7.1 (vring_need_event):
        // Interrupt when the used index crosses the guest's threshold.
        vring_need_event(used_event, new_idx, old_idx)
    }

    /// Write one entry and advance the index. Caller holds Mutex.
    fn write_used_entry(&self, idx: &mut u16, desc_idx: u16, len: u32) {
        let slot = u64::from(*idx % self.queue_size);
        let entry_gpa = self.used_addr + 4 + slot * 8;

        if let Some(sub) = self.physmap.lookup(entry_gpa, 8) {
            // write to guest memory; no recovery if unmapped
            let _ = sub.write::<u32>(&u32::from(desc_idx));
            if let Some(len_sub) = sub.subregion(4, 4) {
                let _ = len_sub.write::<u32>(&len);
            }
        }

        // Release fence: guest reads index first, then entry data.
        fence(Ordering::Release);

        *idx = idx.wrapping_add(1);
        let idx_gpa = self.used_addr + 2;
        if let Some(sub) = self.physmap.lookup(idx_gpa, 2) {
            // write to guest memory; no recovery if unmapped
            let _ = sub.write::<u16>(idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;

    const AVAIL_GPA: u64 = 0x1100;
    const QUEUE_SIZE: u16 = 8;
    const RACERS: usize = 8;
    const FORCED: u32 = 4;

    /// A completion for a ring whose driver has pushed `used_event`
    /// out of reach, so only the force counter can ask for an
    /// interrupt.
    fn suppressed(physmap: &Arc<PhysMap>) -> Arc<VirtioCompletion> {
        let mut queue = VirtQueue::new(QUEUE_SIZE);
        queue.set_addr_modern(0x1000, AVAIL_GPA, 0x1200);
        queue.set_event_idx(true);
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(QUEUE_SIZE) * 2, 2)
            .expect("mapped used_event")
            .write::<u16>(&60_000)
            .expect("write used_event");
        VirtioCompletion::new(
            &queue,
            Arc::clone(physmap),
            IntrSession::INITIAL,
            |_| {},
        )
    }

    // Threads can read the same forced count and each subtract it. A
    // plain subtract then wraps the counter to u32::MAX and the device
    // forces an interrupt for every completion after that, so the
    // guest loses EVENT_IDX suppression on a ring it asked to keep
    // quiet. N counts must force N interrupts and no more.
    #[test]
    fn a_race_cannot_force_more_interrupts_than_were_asked_for() {
        let physmap =
            Arc::new(PhysMap::new_anon(0x1000, 0x1000).expect("queue memory"));
        let completion = suppressed(&physmap);

        for round in 0..512 {
            completion.set_force_interrupt(FORCED);
            let forced = AtomicUsize::new(0);
            let ready = AtomicUsize::new(0);
            let go = AtomicBool::new(false);
            std::thread::scope(|s| {
                for _ in 0..RACERS {
                    s.spawn(|| {
                        ready.fetch_add(1, Ordering::Release);
                        while !go.load(Ordering::Acquire) {
                            std::hint::spin_loop();
                        }
                        if completion.should_interrupt(0, 1) {
                            forced.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
                while ready.load(Ordering::Acquire) < RACERS {
                    std::hint::spin_loop();
                }
                go.store(true, Ordering::Release);
            });
            let n = forced.load(Ordering::Relaxed);
            assert_eq!(
                n, FORCED as usize,
                "round {round} forced {n} interrupts, not {FORCED}"
            );
        }
    }
}
