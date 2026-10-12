// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Name the owner of a guest page written after the final pass.
//!
//! A late page is almost always a device ring or a buffer a device
//! filled, so the rings the export just read are the first suspects.

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStatePayload, NvmeMigrateCq, NvmeMigrateSq,
};

use crate::protocol::PAGE_SIZE;

/// How many late pages a log line or an error names.
pub(crate) const SHOWN: usize = 8;

const NVME_SQE: u64 = 64;
const NVME_CQE: u64 = 16;
const VIRTQ_DESC: u64 = 16;
const XHCI_TRB: u64 = 16;

struct Owned {
    start: u64,
    end: u64,
    name: String,
}

/// Guest ranges with a known owner.
pub(crate) struct Owners {
    ram: Vec<(u64, usize)>,
    rings: Vec<Owned>,
}

impl Owners {
    pub fn new(ram: &[(u64, usize)], devices: &[DeviceStatePayload]) -> Self {
        let mut rings = Vec::new();
        for payload in devices {
            let Some(state) = &payload.state else {
                continue;
            };
            rings_of(&payload.bdf.to_string(), state, &mut rings);
        }
        Self {
            ram: ram.to_vec(),
            rings,
        }
    }

    /// The first [`SHOWN`] pages, each with what owns it.
    pub fn describe(&self, gpas: &[u64]) -> String {
        let mut out: Vec<String> = gpas
            .iter()
            .take(SHOWN)
            .map(|&gpa| format!("{gpa:#x} ({})", self.owner(gpa)))
            .collect();
        if gpas.len() > SHOWN {
            out.push(format!("{} more", gpas.len() - SHOWN));
        }
        out.join(", ")
    }

    fn owner(&self, gpa: u64) -> String {
        let end = gpa.saturating_add(PAGE_SIZE as u64);
        let names: Vec<&str> = self
            .rings
            .iter()
            .filter(|r| r.start < end && gpa < r.end)
            .map(|r| r.name.as_str())
            .collect();
        if !names.is_empty() {
            return names.join(" + ");
        }
        match self
            .ram
            .iter()
            .position(|&(base, len)| gpa >= base && gpa - base < len as u64)
        {
            Some(i) => format!("RAM region {i}, no device ring"),
            None => "outside guest RAM".to_string(),
        }
    }
}

fn push(rings: &mut Vec<Owned>, base: u64, len: u64, name: String) {
    if base == 0 || len == 0 {
        return;
    }
    rings.push(Owned {
        start: base,
        end: base.saturating_add(len),
        name,
    });
}

fn rings_of(dev: &str, state: &DeviceMigrateState, rings: &mut Vec<Owned>) {
    match state {
        DeviceMigrateState::Virtio(v) => {
            for q in &v.queues {
                let n = u64::from(q.queue_size);
                let idx = q.queue_idx;
                push(
                    rings,
                    q.desc_addr,
                    n * VIRTQ_DESC,
                    format!("virtio {dev} queue {idx} descriptors"),
                );
                push(
                    rings,
                    q.avail_addr,
                    6 + 2 * n,
                    format!("virtio {dev} queue {idx} avail ring"),
                );
                push(
                    rings,
                    q.used_addr,
                    6 + 8 * n,
                    format!("virtio {dev} queue {idx} used ring"),
                );
            }
        }
        DeviceMigrateState::Nvme(n) => {
            let sq = |rings: &mut Vec<Owned>, q: &NvmeMigrateSq, what| {
                push(rings, q.base, u64::from(q.size) * NVME_SQE, what);
            };
            let cq = |rings: &mut Vec<Owned>, q: &NvmeMigrateCq, what| {
                push(rings, q.base, u64::from(q.size) * NVME_CQE, what);
            };
            if let Some(q) = &n.admin_sq {
                sq(rings, q, format!("nvme {dev} admin SQ"));
            }
            if let Some(q) = &n.admin_cq {
                cq(rings, q, format!("nvme {dev} admin CQ"));
            }
            for (i, q) in n.io_sqs.iter().enumerate() {
                if let Some(q) = q {
                    sq(rings, q, format!("nvme {dev} I/O SQ {}", i + 1));
                }
            }
            for (i, q) in n.io_cqs.iter().enumerate() {
                if let Some(q) = q {
                    cq(rings, q, format!("nvme {dev} I/O CQ {}", i + 1));
                }
            }
        }
        DeviceMigrateState::Xhci(x) => {
            push(
                rings,
                x.event_ring.base,
                u64::from(x.event_ring.size) * XHCI_TRB,
                format!("xhci {dev} event ring"),
            );
            push(
                rings,
                x.erstba,
                u64::from(x.erstsz) * XHCI_TRB,
                format!("xhci {dev} event ring table"),
            );
            // Transfer and command rings have no size in the payload;
            // the page that holds the cursor is the best guess.
            for (ring, what) in [
                (&x.cmd_ring, "command ring"),
                (&x.ep0, "EP0 ring"),
                (&x.ep1_in, "EP1 IN ring"),
            ] {
                push(
                    rings,
                    ring.dequeue & !(PAGE_SIZE as u64 - 1),
                    PAGE_SIZE as u64,
                    format!("xhci {dev} {what}"),
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_devices::lifecycle::{
        NvmeMigrateState, VirtioMigrateQueue, VirtioMigrateState, WireBdf,
    };

    fn bdf(dev: u8) -> WireBdf {
        WireBdf {
            bus: 0,
            dev,
            func: 0,
        }
    }

    #[test]
    fn a_late_page_names_its_ring() {
        let virtio = DeviceStatePayload {
            bdf: bdf(4),
            state: Some(DeviceMigrateState::Virtio(VirtioMigrateState {
                queues: vec![VirtioMigrateQueue {
                    queue_idx: 1,
                    queue_size: 256,
                    desc_addr: 0x10_0000,
                    avail_addr: 0x10_1000,
                    used_addr: 0x10_2000,
                    ..VirtioMigrateQueue::default()
                }],
                ..VirtioMigrateState::default()
            })),
        };
        let nvme = DeviceStatePayload {
            bdf: bdf(5),
            state: Some(DeviceMigrateState::Nvme(NvmeMigrateState {
                io_cqs: vec![
                    None,
                    Some(NvmeMigrateCq {
                        base: 0x20_0000,
                        size: 1024,
                        head: 0,
                        tail: 0,
                        phase: true,
                        iv: 2,
                        ien: true,
                        held: Vec::new(),
                    }),
                ],
                ..NvmeMigrateState::default()
            })),
        };
        let owners = Owners::new(&[(0, 0x100_0000)], &[virtio, nvme]);

        let text = owners.describe(&[0x10_2000, 0x20_3000, 0x50_0000]);
        assert!(text.contains("0x102000 (virtio 0.4.0 queue 1 used ring)"));
        assert!(text.contains("0x203000 (nvme 0.5.0 I/O CQ 2)"), "{text}");
        assert!(text.contains("0x500000 (RAM region 0, no device ring)"));
    }

    #[test]
    fn a_long_list_is_cut_short() {
        let owners = Owners::new(&[(0, 0x100_0000)], &[]);
        let gpas: Vec<u64> = (0..SHOWN as u64 + 3)
            .map(|i| i * PAGE_SIZE as u64)
            .collect();
        let text = owners.describe(&gpas);
        assert!(text.ends_with("3 more"), "{text}");
    }
}
