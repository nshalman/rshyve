// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use vmm_devices::lifecycle::{
    GpuMigrateBlob, GpuMigrateBlobImage, GpuMigrateCursor, GpuMigrateResource,
    GpuMigrateState, VirtioDeviceMigrateState,
};

use zerocopy::{FromBytes, IntoBytes};

use super::*;
use crate::bits;
use crate::queue::VirtqDesc;
use crate::testlog::CountingDrain;

/// Guest memory for requests, responses and pixel backing.
const MEM_GPA: u64 = 0x10_0000;
const MEM_LEN: usize = 0x80_0000;
const REQ_GPA: u64 = MEM_GPA;
const RESP_GPA: u64 = MEM_GPA + 0x1000;
/// Pixel backing starts here, page aligned.
const BACKING_GPA: u64 = MEM_GPA + 0x10_0000;
const UNMAPPED_GPA: u64 = 0x9000_0000;

struct Guest {
    physmap: Arc<PhysMap>,
    gpu: VirtioGpu,
}

impl Guest {
    fn new() -> Self {
        Self::with_size(1024, 768)
    }

    fn with_size(width: u32, height: u32) -> Self {
        Self::paced(width, height, FLIP_INTERVAL)
    }

    /// A guest whose flips are answered `interval` apart.
    fn paced(width: u32, height: u32, interval: Duration) -> Self {
        Self::build(
            width,
            height,
            interval,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    /// A guest whose device logs to `log`.
    fn logged(log: slog::Logger) -> Self {
        Self::build(1024, 768, FLIP_INTERVAL, log)
    }

    fn build(
        width: u32,
        height: u32,
        interval: Duration,
        log: slog::Logger,
    ) -> Self {
        let physmap = Arc::new(
            PhysMap::new_anon(MEM_GPA, MEM_LEN).expect("create guest memory"),
        );
        let gpu = VirtioGpu::paced(
            width,
            height,
            Arc::clone(&physmap),
            interval,
            log,
        )
        .expect("device");
        Self { physmap, gpu }
    }

    /// Submit one control request and return the response header and
    /// the full response bytes.
    fn ctrl(&self, req: &[u8], resp_len: u32) -> (CtrlHdr, Vec<u8>) {
        self.physmap
            .lookup(REQ_GPA, req.len())
            .expect("mapped request")
            .write_bytes(req)
            .expect("write request");
        let bufs = [
            ChainBuf::Readable {
                addr: REQ_GPA,
                len: u32::try_from(req.len()).expect("fits"),
            },
            ChainBuf::Writable {
                addr: RESP_GPA,
                len: resp_len,
            },
        ];
        let written = self.gpu.handle_ctrl(&bufs, &self.physmap);
        let mut resp = vec![0u8; written as usize];
        self.physmap
            .lookup(RESP_GPA, resp.len().max(1))
            .expect("mapped response")
            .read_bytes(&mut resp)
            .expect("read response");
        let hdr = CtrlHdr::read_from_prefix(&resp)
            .map(|(h, _)| h)
            .unwrap_or_default();
        (hdr, resp)
    }

    /// Submit a request that has only a header response.
    fn cmd(&self, type_: u32, body: &[u8]) -> u32 {
        let mut req = hdr(type_).as_bytes().to_vec();
        req.extend_from_slice(body);
        self.ctrl(&req, 24).0.type_
    }

    fn create(&self, id: u32, format: u32, width: u32, height: u32) -> u32 {
        self.cmd(
            CMD_RESOURCE_CREATE_2D,
            ResourceCreate2d {
                resource_id: id,
                format,
                width,
                height,
            }
            .as_bytes(),
        )
    }

    fn attach(&self, id: u32, entries: &[MemEntry]) -> u32 {
        let mut body = ResourceAttachBacking {
            resource_id: id,
            nr_entries: u32::try_from(entries.len()).expect("fits"),
        }
        .as_bytes()
        .to_vec();
        for e in entries {
            body.extend_from_slice(e.as_bytes());
        }
        self.cmd(CMD_RESOURCE_ATTACH_BACKING, &body)
    }

    fn transfer(&self, id: u32, r: Rect, offset: u64) -> u32 {
        self.cmd(
            CMD_TRANSFER_TO_HOST_2D,
            TransferToHost2d {
                r,
                offset,
                resource_id: id,
                padding: 0,
            }
            .as_bytes(),
        )
    }

    fn set_scanout(&self, scanout_id: u32, id: u32, r: Rect) -> u32 {
        self.cmd(
            CMD_SET_SCANOUT,
            SetScanout {
                r,
                scanout_id,
                resource_id: id,
            }
            .as_bytes(),
        )
    }

    fn flush(&self, id: u32, r: Rect) -> u32 {
        self.cmd(
            CMD_RESOURCE_FLUSH,
            ResourceFlush {
                r,
                resource_id: id,
                padding: 0,
            }
            .as_bytes(),
        )
    }

    fn create_blob(&self, id: u32, size: u64, entries: &[MemEntry]) -> u32 {
        let mut body = ResourceCreateBlob {
            resource_id: id,
            blob_mem: BLOB_MEM_GUEST,
            blob_flags: 2,
            nr_entries: u32::try_from(entries.len()).expect("fits"),
            blob_id: 0,
            size,
        }
        .as_bytes()
        .to_vec();
        for e in entries {
            body.extend_from_slice(e.as_bytes());
        }
        self.cmd(CMD_RESOURCE_CREATE_BLOB, &body)
    }

    fn set_scanout_blob(&self, s: SetScanoutBlob) -> u32 {
        self.cmd(CMD_SET_SCANOUT_BLOB, s.as_bytes())
    }

    fn unref(&self, id: u32) -> u32 {
        self.cmd(
            CMD_RESOURCE_UNREF,
            ResourceUnref {
                resource_id: id,
                padding: 0,
            }
            .as_bytes(),
        )
    }

    fn fill_backing(&self, offset: u64, bytes: &[u8]) {
        self.physmap
            .lookup(BACKING_GPA + offset, bytes.len())
            .expect("mapped backing")
            .write_bytes(bytes)
            .expect("write backing");
    }

    /// A resource `width` x `height` with contiguous backing, shown on
    /// scanout 0, with every pixel of the backing set to its index.
    fn shown(&self, id: u32, width: u32, height: u32) {
        let len = width * height * 4;
        assert_eq!(
            self.create(id, FORMAT_B8G8R8X8_UNORM, width, height),
            RESP_OK_NODATA
        );
        assert_eq!(self.attach(id, &[entry(BACKING_GPA, len)]), RESP_OK_NODATA);
        let pixels: Vec<u8> =
            (0..width * height).flat_map(|i| i.to_le_bytes()).collect();
        self.fill_backing(0, &pixels);
        assert_eq!(
            self.set_scanout(0, id, rect(0, 0, width, height)),
            RESP_OK_NODATA
        );
    }
}

fn hdr(type_: u32) -> CtrlHdr {
    CtrlHdr {
        type_,
        ..Default::default()
    }
}

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn entry(addr: u64, length: u32) -> MemEntry {
    MemEntry {
        addr,
        length,
        padding: 0,
    }
}

/// The pixel at (x, y) of the display, as the u32 the test wrote.
fn display_pixel(display: &GpuDisplay, x: u32, y: u32) -> u32 {
    display.with_surface(|s| {
        let at = ((y * s.width + x) * 4) as usize;
        u32::from_le_bytes(s.pixels[at..at + 4].try_into().expect("4 bytes"))
    })
}

// ---------------------------------------------------------------------------
// Device identity and config
// ---------------------------------------------------------------------------

#[test]
fn config_reports_one_scanout_and_no_capsets() {
    let g = Guest::new();
    let gpu = &g.gpu;
    assert_eq!(gpu.cfg_read(0, 4), 0, "events_read");
    assert_eq!(gpu.cfg_read(8, 4), 1, "num_scanouts");
    assert_eq!(gpu.cfg_read(12, 4), 0, "num_capsets");
}

#[test]
fn features_are_ring_features_and_blobs() {
    let g = Guest::new();
    let gpu = &g.gpu;
    assert_eq!(
        gpu.device_features(),
        bits::VIRTIO_F_RING_EVENT_IDX
            | bits::VIRTIO_F_RING_INDIRECT_DESC
            | F_RESOURCE_BLOB
    );
}

// ---------------------------------------------------------------------------
// Control queue
// ---------------------------------------------------------------------------

#[test]
fn display_info_reports_scanout_zero_at_the_configured_size() {
    let g = Guest::with_size(1280, 800);
    let (h, resp) = g.ctrl(
        hdr(CMD_GET_DISPLAY_INFO).as_bytes(),
        size_of::<RespDisplayInfo>() as u32,
    );
    assert_eq!(h.type_, RESP_OK_DISPLAY_INFO);
    let (info, _) =
        RespDisplayInfo::read_from_prefix(&resp).expect("full response");
    assert_eq!(info.pmodes[0].r, rect(0, 0, 1280, 800));
    assert_eq!(info.pmodes[0].enabled, 1);
    assert!(info.pmodes[1..].iter().all(|p| p.enabled == 0));
}

#[test]
fn a_fenced_request_gets_its_fence_back() {
    let g = Guest::new();
    let req = CtrlHdr {
        type_: CMD_RESOURCE_CREATE_2D,
        flags: FLAG_FENCE,
        fence_id: 0xfeed_beef_1234,
        ctx_id: 0,
        ring_idx: 0,
        padding: [0; 3],
    };
    let mut bytes = req.as_bytes().to_vec();
    bytes.extend_from_slice(
        ResourceCreate2d {
            resource_id: 1,
            format: FORMAT_B8G8R8X8_UNORM,
            width: 4,
            height: 4,
        }
        .as_bytes(),
    );
    let (h, _) = g.ctrl(&bytes, 24);
    assert_eq!(h.type_, RESP_OK_NODATA);
    assert_eq!(h.flags & FLAG_FENCE, FLAG_FENCE);
    assert_eq!(h.fence_id, 0xfeed_beef_1234);
}

#[test]
fn an_unknown_command_is_refused() {
    let g = Guest::new();
    assert_eq!(g.cmd(0x0200, &[]), RESP_ERR_UNSPEC, "3D is not offered");
}

#[test]
fn a_truncated_request_is_refused() {
    let g = Guest::new();
    // A create with half its body.
    assert_eq!(g.cmd(CMD_RESOURCE_CREATE_2D, &[0u8; 8]), RESP_ERR_UNSPEC);
}

#[test]
fn a_request_without_room_for_a_response_writes_nothing() {
    let g = Guest::new();
    let (_, resp) = g.ctrl(hdr(CMD_GET_DISPLAY_INFO).as_bytes(), 8);
    assert!(resp.is_empty());
}

#[test]
fn create_refuses_id_zero_duplicates_and_bad_formats() {
    let g = Guest::new();
    assert_eq!(
        g.create(0, FORMAT_B8G8R8X8_UNORM, 4, 4),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(
        g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(g.create(2, 67, 4, 4), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(
        g.create(3, FORMAT_B8G8R8A8_UNORM, 0, 4),
        RESP_ERR_INVALID_PARAMETER
    );
}

/// Resource dimensions are guest-chosen and become a host allocation,
/// so the device must cap what all resources together may hold.
#[test]
fn create_refuses_past_the_host_memory_budget() {
    let g = Guest::new();
    assert_eq!(
        g.create(1, FORMAT_B8G8R8X8_UNORM, 16384, 16384),
        RESP_ERR_OUT_OF_MEMORY
    );
    // Smaller resources are accepted until the budget runs out.
    let side = 4096u32; // 64 MiB each, and its record
    let fit = MAX_HOST_BYTES
        / (side as usize * side as usize * 4 + RESOURCE_OVERHEAD);
    for id in 1..=fit as u32 {
        assert_eq!(
            g.create(id, FORMAT_B8G8R8X8_UNORM, side, side),
            RESP_OK_NODATA
        );
    }
    assert_eq!(
        g.create(fit as u32 + 1, FORMAT_B8G8R8X8_UNORM, side, side),
        RESP_ERR_OUT_OF_MEMORY
    );
    // Unref gives the budget back.
    assert_eq!(
        g.cmd(
            CMD_RESOURCE_UNREF,
            ResourceUnref {
                resource_id: 1,
                padding: 0
            }
            .as_bytes()
        ),
        RESP_OK_NODATA
    );
    assert_eq!(
        g.create(fit as u32 + 1, FORMAT_B8G8R8X8_UNORM, side, side),
        RESP_OK_NODATA
    );
}

/// A refused command reaches the log, so a black screen leaves a trace;
/// and a guest repeating it cannot fill the log.
#[test]
fn a_refused_command_is_logged_but_not_without_limit() {
    let records = Arc::new(AtomicUsize::new(0));
    let g = Guest::logged(slog::Logger::root(
        CountingDrain(Arc::clone(&records)),
        slog::o!(),
    ));
    assert_eq!(g.unref(42), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(records.load(Ordering::Relaxed), 1);
    for _ in 0..100 {
        g.unref(42);
    }
    let logged = records.load(Ordering::Relaxed);
    assert!(
        (1..=REFUSAL_LOG_BURST as usize).contains(&logged),
        "{logged} records"
    );
}

/// A chain the device cannot serve at all, too short for a header or
/// with no room for the answer, gets no answer, and leaves a trace in
/// the log as a refused command does.
#[test]
fn a_malformed_request_is_logged() {
    let records = Arc::new(AtomicUsize::new(0));
    let g = Guest::logged(slog::Logger::root(
        CountingDrain(Arc::clone(&records)),
        slog::o!(),
    ));
    assert_eq!(g.ctrl(&[0; 4], 24).1.len(), 0, "a short header");
    assert_eq!(records.load(Ordering::Relaxed), 1);
    let get = hdr(CMD_GET_DISPLAY_INFO);
    assert_eq!(g.ctrl(get.as_bytes(), 4).1.len(), 0, "no room");
    assert_eq!(records.load(Ordering::Relaxed), 2);
}

#[test]
fn unref_of_an_unknown_resource_is_refused() {
    let g = Guest::new();
    assert_eq!(
        g.cmd(
            CMD_RESOURCE_UNREF,
            ResourceUnref {
                resource_id: 9,
                padding: 0
            }
            .as_bytes()
        ),
        RESP_ERR_INVALID_RESOURCE_ID
    );
}

/// Fill the budget to within `room` bytes with one resource, id 9.
fn leave_room(g: &Guest, room: usize) {
    let width = 4096u32;
    let height = ((MAX_HOST_BYTES - room) / (width as usize * 4)) as u32;
    assert_eq!(
        g.create(9, FORMAT_B8G8R8X8_UNORM, width, height),
        RESP_OK_NODATA
    );
}

/// Each resource costs the budget its record too, so 1x1 resources run
/// out long before their four bytes of pixels would, and an unref gives
/// the room back.
#[test]
fn each_resource_counts_against_the_memory_budget() {
    let g = Guest::new();
    // Exactly 16 KiB left: the big resource's rows are 16 KiB.
    leave_room(&g, 16 << 10);
    let mut made = 0;
    while g.create(10 + made, FORMAT_B8G8R8X8_UNORM, 1, 1) == RESP_OK_NODATA {
        made += 1;
        assert!(made < 4096, "1x1 resources never ran out");
    }
    let room = (16 << 10) - RESOURCE_OVERHEAD;
    assert_eq!(made as usize, room / (BYTES_PER_PIXEL + RESOURCE_OVERHEAD));

    assert_eq!(g.unref(10), RESP_OK_NODATA);
    assert_eq!(g.create(10, FORMAT_B8G8R8X8_UNORM, 1, 1), RESP_OK_NODATA);
}

/// A page list that, with its blob's record, takes exactly the room
/// `leave_room(g, 32 KiB)` leaves after that call's own resource record.
fn an_exact_list() -> Vec<MemEntry> {
    let room = (32 << 10) - 2 * RESOURCE_OVERHEAD;
    assert_eq!(room % size_of::<MemEntry>(), 0);
    vec![entry(BACKING_GPA, 4096); room / size_of::<MemEntry>()]
}

/// 2048 entries, 32 KiB of host memory. Small, so the tests stay cheap
/// beside the timing-sensitive ones that run alongside them.
fn a_long_list() -> Vec<MemEntry> {
    vec![entry(BACKING_GPA, 4096); 2048]
}

/// A backing page list is host memory too: 65536 entries are a MiB. A
/// guest could hang that on any number of 1x1 resources, each a few
/// bytes of pixels, so the lists count against the same budget.
#[test]
fn backing_entries_count_against_the_memory_budget() {
    let g = Guest::new();
    leave_room(&g, 16 << 10);
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 1, 1), RESP_OK_NODATA);
    let many = a_long_list();
    assert_eq!(g.attach(1, &many), RESP_ERR_OUT_OF_MEMORY);
    // A short list still fits.
    assert_eq!(g.attach(1, &[entry(BACKING_GPA, 4)]), RESP_OK_NODATA);
}

/// Detaching or dropping a resource gives its list's bytes back.
#[test]
fn detach_and_unref_give_backing_bytes_back() {
    let g = Guest::new();
    leave_room(&g, 48 << 10);
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 1, 1), RESP_OK_NODATA);
    let many = a_long_list();
    // Each list is 32 KiB, so two never fit together: a re-attach
    // replaces the old list's bytes rather than adding to them.
    for _ in 0..3 {
        assert_eq!(g.attach(1, &many), RESP_OK_NODATA);
    }
    assert_eq!(g.create(2, FORMAT_B8G8R8X8_UNORM, 1, 1), RESP_OK_NODATA);
    assert_eq!(g.attach(2, &many), RESP_ERR_OUT_OF_MEMORY);
    assert_eq!(
        g.cmd(
            CMD_RESOURCE_DETACH_BACKING,
            ResourceDetachBacking {
                resource_id: 1,
                padding: 0,
            }
            .as_bytes()
        ),
        RESP_OK_NODATA
    );
    assert_eq!(g.attach(2, &many), RESP_OK_NODATA);
    assert_eq!(
        g.cmd(
            CMD_RESOURCE_UNREF,
            ResourceUnref {
                resource_id: 2,
                padding: 0,
            }
            .as_bytes()
        ),
        RESP_OK_NODATA
    );
    assert_eq!(g.attach(1, &many), RESP_OK_NODATA);
}

#[test]
fn attach_refuses_an_unknown_resource_and_too_many_entries() {
    let g = Guest::new();
    assert_eq!(
        g.attach(1, &[entry(BACKING_GPA, 64)]),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    // The count is guest-chosen; the entries it names are not there.
    let body = ResourceAttachBacking {
        resource_id: 1,
        nr_entries: u32::MAX,
    };
    assert_eq!(
        g.cmd(CMD_RESOURCE_ATTACH_BACKING, body.as_bytes()),
        RESP_ERR_INVALID_PARAMETER
    );
}

#[test]
fn transfer_without_backing_is_refused() {
    let g = Guest::new();
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(g.transfer(1, rect(0, 0, 4, 4), 0), RESP_ERR_UNSPEC);
}

#[test]
fn transfer_refuses_a_rect_outside_the_resource() {
    let g = Guest::new();
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(g.attach(1, &[entry(BACKING_GPA, 64)]), RESP_OK_NODATA);
    assert_eq!(
        g.transfer(1, rect(2, 2, 3, 1), 0),
        RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(
        g.transfer(1, rect(u32::MAX, 0, 2, 1), 0),
        RESP_ERR_INVALID_PARAMETER
    );
}

#[test]
fn transfer_refuses_backing_that_is_not_mapped() {
    let g = Guest::new();
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(g.attach(1, &[entry(UNMAPPED_GPA, 64)]), RESP_OK_NODATA);
    assert_eq!(g.transfer(1, rect(0, 0, 4, 4), 0), RESP_ERR_UNSPEC);
}

#[test]
fn transfer_refuses_a_read_past_the_end_of_the_backing() {
    let g = Guest::new();
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(g.attach(1, &[entry(BACKING_GPA, 32)]), RESP_OK_NODATA);
    assert_eq!(g.transfer(1, rect(0, 0, 4, 4), 0), RESP_ERR_UNSPEC);
}

#[test]
fn set_scanout_refuses_other_scanouts_and_unknown_resources() {
    let g = Guest::new();
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout(1, 1, rect(0, 0, 4, 4)),
        RESP_ERR_INVALID_SCANOUT_ID
    );
    assert_eq!(
        g.set_scanout(0, 7, rect(0, 0, 4, 4)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(
        g.set_scanout(0, 1, rect(0, 0, 5, 4)),
        RESP_ERR_INVALID_PARAMETER
    );
}

// ---------------------------------------------------------------------------
// Pixels reaching the display
// ---------------------------------------------------------------------------

#[test]
fn set_scanout_sizes_the_display() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    assert_eq!(g.gpu.display().size(), (8, 6));
}

#[test]
fn a_transfer_and_flush_reach_the_display_and_report_damage() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    // SET_SCANOUT damages the whole surface; clear that first.
    g.gpu.display().take_damage();

    assert_eq!(g.transfer(1, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
    // Transfer alone shows nothing yet.
    assert_eq!(display_pixel(g.gpu.display(), 3, 2), 0);
    assert_eq!(g.gpu.display().take_damage(), None);

    assert_eq!(g.flush(1, rect(2, 1, 3, 2)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().take_damage(), Some(rect(2, 1, 3, 2)));
    // Inside the flushed rect: the pixel index the backing holds.
    assert_eq!(display_pixel(g.gpu.display(), 3, 2), 2 * 8 + 3);
    // Outside it: not yet shown.
    assert_eq!(display_pixel(g.gpu.display(), 7, 5), 0);
}

#[test]
fn damage_accumulates_until_taken() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    g.gpu.display().take_damage();
    assert_eq!(g.transfer(1, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(0, 0, 1, 1)), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(4, 4, 2, 2)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().take_damage(), Some(rect(0, 0, 6, 6)));
    assert_eq!(g.gpu.display().take_damage(), None);
}

/// A sub-rectangle transfer reads the backing at `offset` plus one
/// resource stride per row, the way Linux lays it out and QEMU reads it.
#[test]
fn a_partial_transfer_reads_rows_a_stride_apart() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    let r = rect(2, 3, 2, 2);
    let offset = (3 * 8 + 2) * 4u64;
    assert_eq!(g.transfer(1, r, offset), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    assert_eq!(display_pixel(g.gpu.display(), 2, 3), 3 * 8 + 2);
    assert_eq!(display_pixel(g.gpu.display(), 3, 4), 4 * 8 + 3);
    // Outside the transferred rect the resource is still zero.
    assert_eq!(display_pixel(g.gpu.display(), 4, 3), 0);
    assert_eq!(display_pixel(g.gpu.display(), 2, 2), 0);
}

/// Linux backs a resource with one entry per page, so the device must
/// read across entries that are not contiguous in guest memory.
/// A transfer reads its rows at growing offsets, and the page list is
/// walked once across them, not from its start for every row: a list may
/// have 65536 entries, a transfer as many rows as the resource, and the
/// walk runs on the vCPU.
#[test]
fn reading_rows_walks_the_page_list_once() {
    let g = Guest::new();
    let entries: Vec<MemEntry> = (0..4096u64)
        .map(|i| entry(BACKING_GPA + i * 64, 64))
        .collect();
    let pixels: Vec<u8> = (0..4096 * 64).map(|i| (i / 64) as u8).collect();
    g.fill_backing(0, &pixels);
    let mut cursor = BackingCursor::new(&entries);
    let mut row = [0u8; 256];
    for r in 0..1024u64 {
        cursor.read(&g.physmap, r * 256, &mut row).expect("a row");
        assert_eq!(row[0], (r * 4) as u8, "row {r} starts in entry {}", r * 4);
        assert_eq!(row[255], (r * 4 + 3) as u8);
    }
    // Linear: each entry is passed once by a copy and once by the step
    // past it, and each row adds one. Walking from the start for every
    // row would be about two million here.
    assert!(
        cursor.visited <= 2 * entries.len() + 1024,
        "{} entries visited for 1024 rows over {}",
        cursor.visited,
        entries.len()
    );
}

/// An empty read reads nothing, wherever it is.
#[test]
fn an_empty_read_succeeds_without_walking() {
    let entries = [entry(BACKING_GPA, 64)];
    let g = Guest::new();
    assert!(BackingCursor::new(&entries)
        .read(&g.physmap, 1 << 20, &mut [])
        .is_some());
}

#[test]
fn backing_split_across_entries_is_read_in_order() {
    let g = Guest::new();
    let (w, h) = (4u32, 4u32);
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, w, h), RESP_OK_NODATA);
    // Two 32-byte halves, placed in reverse order in guest memory.
    let second = BACKING_GPA;
    let first = BACKING_GPA + 0x2000;
    assert_eq!(
        g.attach(1, &[entry(first, 32), entry(second, 32)]),
        RESP_OK_NODATA
    );
    let px: Vec<u8> = (0..16u32).flat_map(|i| i.to_le_bytes()).collect();
    g.fill_backing(0x2000, &px[..32]);
    g.fill_backing(0, &px[32..]);
    assert_eq!(g.set_scanout(0, 1, rect(0, 0, w, h)), RESP_OK_NODATA);
    assert_eq!(g.transfer(1, rect(0, 0, w, h), 0), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(0, 0, w, h)), RESP_OK_NODATA);
    for i in 0..16 {
        assert_eq!(display_pixel(g.gpu.display(), i % 4, i / 4), i);
    }
}

#[test]
fn a_flush_of_a_resource_not_on_the_scanout_changes_nothing() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    g.gpu.display().take_damage();
    assert_eq!(g.create(2, FORMAT_B8G8R8X8_UNORM, 8, 6), RESP_OK_NODATA);
    assert_eq!(g.flush(2, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().take_damage(), None);
}

/// A scanout may show a window of a larger resource: the display is
/// the window's size and a flush lands at its offset in the window.
#[test]
fn a_scanout_can_show_part_of_a_resource() {
    let g = Guest::new();
    let (w, h) = (8u32, 6u32);
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, w, h), RESP_OK_NODATA);
    assert_eq!(
        g.attach(1, &[entry(BACKING_GPA, w * h * 4)]),
        RESP_OK_NODATA
    );
    let px: Vec<u8> = (0..w * h).flat_map(|i| i.to_le_bytes()).collect();
    g.fill_backing(0, &px);
    assert_eq!(g.set_scanout(0, 1, rect(2, 1, 4, 3)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().size(), (4, 3));
    g.gpu.display().take_damage();

    assert_eq!(g.transfer(1, rect(0, 0, w, h), 0), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(0, 0, w, h)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().take_damage(), Some(rect(0, 0, 4, 3)));
    // Display (0,0) is resource (2,1).
    assert_eq!(display_pixel(g.gpu.display(), 0, 0), 8 + 2);
    assert_eq!(display_pixel(g.gpu.display(), 3, 2), 3 * 8 + 5);
}

#[test]
fn unref_of_the_shown_resource_disables_the_scanout() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    assert_eq!(
        g.cmd(
            CMD_RESOURCE_UNREF,
            ResourceUnref {
                resource_id: 1,
                padding: 0
            }
            .as_bytes()
        ),
        RESP_OK_NODATA
    );
    assert_eq!(g.gpu.display().size(), (0, 0));
}

#[test]
fn set_scanout_to_resource_zero_disables_it() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    assert_eq!(g.set_scanout(0, 0, rect(0, 0, 0, 0)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().size(), (0, 0));
}

#[test]
fn reset_drops_every_resource_and_the_scanout() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    VirtioDevice::reset(&g.gpu);
    assert_eq!(g.gpu.display().size(), (0, 0));
    // The id is free again.
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
}

// ---------------------------------------------------------------------------
// Cursor queue
// ---------------------------------------------------------------------------

fn cursor_cmd(g: &Guest, type_: u32, cur: UpdateCursor) {
    let mut req = hdr(type_).as_bytes().to_vec();
    req.extend_from_slice(cur.as_bytes());
    g.physmap
        .lookup(REQ_GPA, req.len())
        .expect("mapped request")
        .write_bytes(&req)
        .expect("write request");
    // Linux posts cursor commands with no response buffer.
    let bufs = [ChainBuf::Readable {
        addr: REQ_GPA,
        len: u32::try_from(req.len()).expect("fits"),
    }];
    assert_eq!(g.gpu.handle_cursor(&bufs, &g.physmap), 0);
}

#[test]
fn update_cursor_captures_the_image_and_hotspot() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    // A 64x64 cursor resource.
    let (w, h) = (CURSOR_SIZE, CURSOR_SIZE);
    assert_eq!(g.create(2, FORMAT_B8G8R8A8_UNORM, w, h), RESP_OK_NODATA);
    let at = 0x40_0000u64;
    assert_eq!(
        g.attach(2, &[entry(BACKING_GPA + at, w * h * 4)]),
        RESP_OK_NODATA
    );
    let px: Vec<u8> = (0..w * h)
        .flat_map(|i| (i | 0xff00_0000).to_le_bytes())
        .collect();
    g.fill_backing(at, &px);
    assert_eq!(g.transfer(2, rect(0, 0, w, h), 0), RESP_OK_NODATA);

    cursor_cmd(
        &g,
        CMD_UPDATE_CURSOR,
        UpdateCursor {
            pos: CursorPos {
                scanout_id: 0,
                x: 5,
                y: 6,
                padding: 0,
            },
            resource_id: 2,
            hot_x: 3,
            hot_y: 4,
            padding: 0,
        },
    );
    let cursor = g.gpu.display().cursor().expect("a cursor was set");
    assert_eq!((cursor.x, cursor.y), (5, 6));
    assert_eq!((cursor.hot_x, cursor.hot_y), (3, 4));
    assert_eq!(cursor.pixels.len(), (w * h * 4) as usize);
    assert_eq!(&cursor.pixels[4..8], &(1u32 | 0xff00_0000).to_le_bytes());

    cursor_cmd(
        &g,
        CMD_MOVE_CURSOR,
        UpdateCursor {
            pos: CursorPos {
                scanout_id: 0,
                x: 7,
                y: 1,
                padding: 0,
            },
            ..Default::default()
        },
    );
    let cursor = g.gpu.display().cursor().expect("still set");
    assert_eq!((cursor.x, cursor.y), (7, 1));
    assert_eq!(
        (cursor.hot_x, cursor.hot_y),
        (3, 4),
        "a move keeps the image"
    );
}

/// A 64x64 cursor resource ready to show, as resource 2.
fn cursor_resource(g: &Guest) {
    let (w, h) = (CURSOR_SIZE, CURSOR_SIZE);
    assert_eq!(g.create(2, FORMAT_B8G8R8A8_UNORM, w, h), RESP_OK_NODATA);
    assert_eq!(
        g.attach(2, &[entry(BACKING_GPA + 0x40_0000, w * h * 4)]),
        RESP_OK_NODATA
    );
}

/// The size a VM starts with is the size it offers the guest, which a
/// migration destination checks: one it would refuse is refused at the
/// start, not at the first move.
#[test]
fn a_start_size_a_migration_would_refuse_is_refused() {
    let physmap =
        Arc::new(PhysMap::new_anon(MEM_GPA, MEM_LEN).expect("memory"));
    let log = || slog::Logger::root(slog::Discard, slog::o!());
    assert!(VirtioGpu::new(6000, 6000, Arc::clone(&physmap), log()).is_err());
    assert!(VirtioGpu::new(0, 768, Arc::clone(&physmap), log()).is_err());
    assert!(VirtioGpu::new(5120, 2880, physmap, log()).is_ok());
}

/// The hotspot lies in the cursor, whatever the guest says: a server
/// draws the pointer from it.
#[test]
fn update_cursor_keeps_the_hotspot_in_the_cursor() {
    let g = Guest::new();
    cursor_resource(&g);
    cursor_cmd(
        &g,
        CMD_UPDATE_CURSOR,
        UpdateCursor {
            resource_id: 2,
            hot_x: 500,
            hot_y: u32::MAX,
            ..Default::default()
        },
    );
    let cursor = g.gpu.display().cursor().expect("a cursor was set");
    assert_eq!(
        (cursor.hot_x, cursor.hot_y),
        (CURSOR_SIZE - 1, CURSOR_SIZE - 1)
    );
}

/// The cursor is 64x64: a resource of another size is refused, and the
/// cursor the guest had stays.
#[test]
fn update_cursor_refuses_a_resource_of_another_size() {
    let g = Guest::new();
    set_cursor(&g, 2);
    assert_eq!(g.create(3, FORMAT_B8G8R8A8_UNORM, 32, 32), RESP_OK_NODATA);
    assert_eq!(
        g.attach(3, &[entry(BACKING_GPA, 32 * 32 * 4)]),
        RESP_OK_NODATA
    );
    let before = g.gpu.display().cursor().expect("a cursor was set");
    cursor_cmd(
        &g,
        CMD_UPDATE_CURSOR,
        UpdateCursor {
            resource_id: 3,
            ..Default::default()
        },
    );
    let after = g.gpu.display().cursor().expect("still set");
    assert_eq!(after.pixels, before.pixels);
}

#[test]
fn update_cursor_to_resource_zero_hides_it() {
    let g = Guest::new();
    cursor_cmd(&g, CMD_UPDATE_CURSOR, UpdateCursor::default());
    assert!(g.gpu.display().cursor().is_none());
}

/// Give the guest a 64x64 cursor resource `id` whose pixels are their
/// indices, and point the cursor at it with hotspot (3, 4).
fn set_cursor(g: &Guest, id: u32) {
    let (w, h) = (CURSOR_SIZE, CURSOR_SIZE);
    assert_eq!(g.create(id, FORMAT_B8G8R8A8_UNORM, w, h), RESP_OK_NODATA);
    let at = 0x40_0000u64;
    assert_eq!(
        g.attach(id, &[entry(BACKING_GPA + at, w * h * 4)]),
        RESP_OK_NODATA
    );
    let px: Vec<u8> = (0..w * h).flat_map(|i| i.to_le_bytes()).collect();
    g.fill_backing(at, &px);
    assert_eq!(g.transfer(id, rect(0, 0, w, h), 0), RESP_OK_NODATA);
    cursor_cmd(g, CMD_UPDATE_CURSOR, cursor_at(id, 5, 6));
}

fn cursor_at(resource_id: u32, x: u32, y: u32) -> UpdateCursor {
    UpdateCursor {
        pos: CursorPos {
            scanout_id: 0,
            x,
            y,
            padding: 0,
        },
        resource_id,
        hot_x: 3,
        hot_y: 4,
        padding: 0,
    }
}

/// Until the guest sets a pointer it draws its own, if any, and a
/// client keeps its own.
#[test]
fn servers_see_the_default_pointer_until_the_guest_sets_one() {
    use vmm_devices::display::{CursorShape, DisplaySource};
    let g = Guest::new();
    assert_eq!(DisplaySource::cursor(g.gpu.display()), CursorShape::Default);
    assert_eq!(g.gpu.display().cursor_serial(), 0);
}

#[test]
fn servers_see_the_pointer_the_guest_sets() {
    use vmm_devices::display::{CursorImage, CursorShape, DisplaySource};
    let g = Guest::new();
    set_cursor(&g, 2);
    assert_ne!(g.gpu.display().cursor_serial(), 0);
    let want = CursorImage {
        width: CURSOR_SIZE,
        height: CURSOR_SIZE,
        hot_x: 3,
        hot_y: 4,
        pixels: (0..CURSOR_SIZE * CURSOR_SIZE)
            .flat_map(|i| i.to_le_bytes())
            .collect(),
    };
    assert_eq!(
        DisplaySource::cursor(g.gpu.display()),
        CursorShape::Image(Arc::new(want))
    );
}

/// A client draws the pointer where its own mouse is, so a move changes
/// nothing a server sends.
#[test]
fn moving_the_pointer_does_not_change_it_for_servers() {
    use vmm_devices::display::DisplaySource;
    let g = Guest::new();
    set_cursor(&g, 2);
    let serial = g.gpu.display().cursor_serial();
    cursor_cmd(&g, CMD_MOVE_CURSOR, cursor_at(0, 9, 9));
    assert_eq!(g.gpu.display().cursor_serial(), serial);
}

#[test]
fn a_hidden_pointer_is_hidden_for_servers() {
    use vmm_devices::display::{CursorShape, DisplaySource};
    let g = Guest::new();
    set_cursor(&g, 2);
    let serial = g.gpu.display().cursor_serial();
    cursor_cmd(&g, CMD_UPDATE_CURSOR, UpdateCursor::default());
    assert_ne!(g.gpu.display().cursor_serial(), serial);
    assert_eq!(DisplaySource::cursor(g.gpu.display()), CursorShape::Hidden);
}

/// A reset leaves the guest's firmware and a new driver with no pointer
/// set, so a client goes back to its own rather than showing none.
#[test]
fn a_reset_gives_servers_the_default_pointer_again() {
    use vmm_devices::display::{CursorShape, DisplaySource};
    let g = Guest::new();
    set_cursor(&g, 2);
    let serial = g.gpu.display().cursor_serial();
    VirtioDevice::reset(&g.gpu);
    assert_ne!(g.gpu.display().cursor_serial(), serial);
    assert_eq!(DisplaySource::cursor(g.gpu.display()), CursorShape::Default);
}

// ---------------------------------------------------------------------------
// End to end through the ring
// ---------------------------------------------------------------------------

#[test]
fn notify_queue_serves_the_control_queue() {
    const DESC_GPA: u64 = MEM_GPA + 0x2000;
    const AVAIL_GPA: u64 = MEM_GPA + 0x3000;
    const USED_GPA: u64 = MEM_GPA + 0x4000;
    let g = Guest::new();
    let mut queue = VirtQueue::new(4);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);

    let req = hdr(CMD_GET_DISPLAY_INFO);
    g.physmap
        .lookup(REQ_GPA, 24)
        .expect("mapped")
        .write_bytes(req.as_bytes())
        .expect("write");
    let descs = [
        VirtqDesc {
            addr: REQ_GPA,
            len: 24,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        },
        VirtqDesc {
            addr: RESP_GPA,
            len: size_of::<RespDisplayInfo>() as u32,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        },
    ];
    for (i, d) in descs.iter().enumerate() {
        g.physmap
            .lookup(DESC_GPA + i as u64 * 16, 16)
            .expect("mapped")
            .write(d)
            .expect("write desc");
    }
    g.physmap
        .lookup(AVAIL_GPA + 4, 2)
        .expect("mapped")
        .write::<u16>(&0)
        .expect("avail entry");
    g.physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped")
        .write::<u16>(&1)
        .expect("avail idx");

    let mut queues = [queue, VirtQueue::new(4)];
    assert!(g.gpu.notify_queue(QUEUE_CONTROL, &mut queues, &g.physmap));
    assert_eq!(queues[0].read_used_ring_idx(&g.physmap), 1);
    let used_len = g
        .physmap
        .lookup(USED_GPA + 8, 4)
        .expect("mapped")
        .read::<u32>()
        .expect("used len");
    assert_eq!(used_len as usize, size_of::<RespDisplayInfo>());
}

// ---------------------------------------------------------------------------
// As a display source
// ---------------------------------------------------------------------------

#[test]
fn the_display_is_a_display_source() {
    use vmm_devices::display::{DisplaySource, Rect as HostRect};

    let g = Guest::new();
    g.shown(1, 8, 6);
    assert_eq!(g.transfer(1, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(0, 0, 8, 6)), RESP_OK_NODATA);

    let src: Arc<dyn DisplaySource> = g.gpu.display_handle();
    assert_eq!(src.size(), (8, 6));
    let r = HostRect {
        x: 2,
        y: 1,
        width: 2,
        height: 2,
    };
    let mut out = vec![0u8; 16];
    src.read_rect(r, &mut out).expect("inside the display");
    let got: Vec<u32> = out
        .chunks(4)
        .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
        .collect();
    assert_eq!(got, [8 + 2, 8 + 3, 16 + 2, 16 + 3]);

    assert!(src.read_rect(HostRect { x: 7, ..r }, &mut out).is_err());
    assert!(src.read_rect(r, &mut out[..4]).is_err());
}

/// A display server is outside this crate, so halt reaches it through
/// a hook. Without it the server thread outlives the VM.
#[test]
fn halt_runs_each_hook_once() {
    let g = Guest::new();
    let gpu = &g.gpu;
    let ran = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ran);
    gpu.on_halt(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    Lifecycle::halt(gpu);
    Lifecycle::halt(gpu);
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

/// A page-flipping guest points the scanout at another resource of the
/// same size and flushes it afterwards. The display must show that
/// resource's pixels from the moment the scanout moves, not a blank
/// frame a display server could catch before the flush.
#[test]
fn set_scanout_shows_the_new_resource_at_once() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    assert_eq!(g.transfer(1, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(0, 0, 8, 6)), RESP_OK_NODATA);

    // A second buffer, same size, different contents.
    let at = 0x2_0000u64;
    assert_eq!(g.create(2, FORMAT_B8G8R8X8_UNORM, 8, 6), RESP_OK_NODATA);
    assert_eq!(
        g.attach(2, &[entry(BACKING_GPA + at, 8 * 6 * 4)]),
        RESP_OK_NODATA
    );
    let px: Vec<u8> =
        (0..48u32).flat_map(|i| (i + 1000).to_le_bytes()).collect();
    g.fill_backing(at, &px);
    assert_eq!(g.transfer(2, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
    g.gpu.display().take_damage();

    assert_eq!(g.set_scanout(0, 2, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    assert_eq!(display_pixel(g.gpu.display(), 3, 2), 1000 + 2 * 8 + 3);
    assert_eq!(g.gpu.display().take_damage(), Some(rect(0, 0, 8, 6)));
}

/// A second 8x6 buffer, id 2, filled and transferred, as a page-flipping
/// guest prepares the next frame.
fn second_buffer(g: &Guest) {
    let at = 0x2_0000u64;
    assert_eq!(g.create(2, FORMAT_B8G8R8X8_UNORM, 8, 6), RESP_OK_NODATA);
    assert_eq!(
        g.attach(2, &[entry(BACKING_GPA + at, 8 * 6 * 4)]),
        RESP_OK_NODATA
    );
    let px: Vec<u8> =
        (0..48u32).flat_map(|i| (i + 1000).to_le_bytes()).collect();
    g.fill_backing(at, &px);
    assert_eq!(g.transfer(2, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
}

/// A page flip at the same size reuses the display's buffer: a guest
/// flips far more often than it changes mode.
#[test]
fn a_flip_at_the_same_size_keeps_the_display_buffer() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    let before = g.gpu.display().with_surface(|s| s.pixels.as_ptr());
    second_buffer(&g);
    assert_eq!(g.set_scanout(0, 2, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    let after = g.gpu.display().with_surface(|s| s.pixels.as_ptr());
    assert_eq!(before, after, "the buffer was reallocated");
    assert_eq!(display_pixel(g.gpu.display(), 3, 2), 1000 + 2 * 8 + 3);
    assert_eq!(g.gpu.display().take_damage(), Some(rect(0, 0, 8, 6)));
}

/// The flush that follows a flip shows what the flip already copied: the
/// resource has had no transfer since. It reports the damage without
/// copying the frame again; a transfer makes the next flush copy.
#[test]
fn a_flush_of_what_is_already_shown_copies_nothing() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    second_buffer(&g);
    assert_eq!(g.set_scanout(0, 2, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    g.gpu.display().take_damage();
    let copies = g.gpu.display().lock().copies;

    assert_eq!(g.flush(2, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().lock().copies, copies, "copied again");
    assert_eq!(g.gpu.display().take_damage(), Some(rect(0, 0, 8, 6)));

    assert_eq!(g.transfer(2, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
    assert_eq!(g.flush(2, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().lock().copies, copies + 1);
}

// ---------------------------------------------------------------------------
// Resizing from the display side
// ---------------------------------------------------------------------------

fn display_info(g: &Guest) -> RespDisplayInfo {
    let (_, resp) = g.ctrl(
        hdr(CMD_GET_DISPLAY_INFO).as_bytes(),
        size_of::<RespDisplayInfo>() as u32,
    );
    RespDisplayInfo::read_from_prefix(&resp)
        .expect("full response")
        .0
}

/// A client asking for another size changes the mode the device
/// offers, and tells the driver through the display event and the
/// configuration change interrupt.
#[test]
fn a_size_request_changes_the_offered_mode_and_signals_the_driver() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vmm_devices::display::DisplaySource;

    let g = Guest::with_size(1024, 768);
    let signalled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&signalled);
    g.gpu.on_config_change(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    assert!(g.gpu.display_handle().request_size(1600, 900));

    assert_eq!(display_info(&g).pmodes[0].r, rect(0, 0, 1600, 900));
    assert_eq!(g.gpu.cfg_read(0, 4), EVENT_DISPLAY, "events_read");
    assert_eq!(signalled.load(Ordering::SeqCst), 1);

    // The driver acknowledges through events_clear.
    g.gpu.cfg_write(4, EVENT_DISPLAY, 4);
    assert_eq!(g.gpu.cfg_read(0, 4), 0);
}

/// A client that asks again for the size already offered changes
/// nothing, so the driver is not told: a VNC client that never gets the
/// exact width it asked for repeats it every few seconds, and each
/// signal makes the guest's compositor look at its output again.
#[test]
fn a_request_for_the_offered_size_signals_nothing() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vmm_devices::display::DisplaySource;

    let g = Guest::with_size(1024, 768);
    let signalled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&signalled);
    g.gpu.on_config_change(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    assert!(g.gpu.display_handle().request_size(1024, 768), "accepted");
    assert_eq!(signalled.load(Ordering::SeqCst), 0);
    assert_eq!(g.gpu.cfg_read(0, 4), 0, "no display event");

    assert!(g.gpu.display_handle().request_size(1600, 900));
    g.gpu.cfg_write(4, EVENT_DISPLAY, 4);
    assert!(g.gpu.display_handle().request_size(1600, 900), "again");
    assert_eq!(signalled.load(Ordering::SeqCst), 1);
    assert_eq!(g.gpu.cfg_read(0, 4), 0);
}

#[test]
fn a_size_request_the_device_cannot_back_is_refused() {
    use vmm_devices::display::DisplaySource;

    let g = Guest::with_size(1024, 768);
    let display = g.gpu.display_handle();
    assert!(!display.request_size(0, 768));
    assert!(!display.request_size(16384, 16384), "past the host budget");
    assert_eq!(display_info(&g).pmodes[0].r, rect(0, 0, 1024, 768));
    assert_eq!(g.gpu.cfg_read(0, 4), 0);
}

#[test]
fn reset_clears_pending_events_but_keeps_the_requested_size() {
    use vmm_devices::display::DisplaySource;

    let g = Guest::with_size(1024, 768);
    assert!(g.gpu.display_handle().request_size(1280, 720));
    VirtioDevice::reset(&g.gpu);
    assert_eq!(g.gpu.cfg_read(0, 4), 0);
    assert_eq!(display_info(&g).pmodes[0].r, rect(0, 0, 1280, 720));
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

/// What the destination gets: this device's state, and a copy of guest
/// memory, which the migration carries on its own.
fn migrate(g: &Guest) -> Guest {
    let state = gpu_state(g);
    migrate_with(g, state)
}

/// Migrate `g`'s memory with `state` in place of its device's own.
fn migrate_with(g: &Guest, state: GpuMigrateState) -> Guest {
    migrate_into(g, state, Guest::new())
}

/// Migrate `g`'s memory and `state` into `dest`.
fn migrate_into(g: &Guest, state: GpuMigrateState, dest: Guest) -> Guest {
    let mut ram = vec![0u8; MEM_LEN];
    g.physmap
        .lookup(MEM_GPA, MEM_LEN)
        .expect("source memory")
        .read_bytes(&mut ram)
        .expect("read source memory");
    dest.physmap
        .lookup(MEM_GPA, MEM_LEN)
        .expect("destination memory")
        .write_bytes(&ram)
        .expect("write destination memory");
    let state = VirtioDeviceMigrateState::Gpu(state);
    VirtioDevice::restore_device_state(&dest.gpu, Some(&state), &dest.physmap)
        .expect("the state restores");
    dest
}

fn gpu_state(g: &Guest) -> GpuMigrateState {
    match VirtioDevice::export_device_state(&g.gpu).expect("export") {
        Some(VirtioDeviceMigrateState::Gpu(s)) => s,
        other => panic!("virtio-gpu exports its own state: {other:?}"),
    }
}

/// The destination shows what the source showed, read back from the
/// resource's backing, and all of it is damaged, so a client that
/// connects there is sent the whole picture.
#[test]
fn the_picture_survives_a_migration() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    assert_eq!(g.transfer(1, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
    assert_eq!(g.flush(1, rect(0, 0, 8, 6)), RESP_OK_NODATA);

    let d = migrate(&g);
    let display = d.gpu.display();
    assert_eq!(display.size(), (8, 6));
    for (x, y) in [(0, 0), (7, 0), (3, 4), (7, 5)] {
        assert_eq!(display_pixel(display, x, y), y * 8 + x, "({x}, {y})");
    }
    assert_eq!(display.take_damage(), Some(rect(0, 0, 8, 6)));
}

/// The guest carries on with the resources it made: they still exist,
/// with their backing, and new draws reach the display.
#[test]
fn resources_carry_on_after_a_migration() {
    let g = Guest::new();
    g.shown(1, 8, 6);
    let d = migrate(&g);
    assert_eq!(
        d.create(1, FORMAT_B8G8R8X8_UNORM, 8, 6),
        RESP_ERR_INVALID_RESOURCE_ID,
        "resource 1 still exists"
    );
    d.fill_backing(0, &0xdead_beefu32.to_le_bytes());
    assert_eq!(d.transfer(1, rect(0, 0, 1, 1), 0), RESP_OK_NODATA);
    assert_eq!(d.flush(1, rect(0, 0, 1, 1)), RESP_OK_NODATA);
    assert_eq!(display_pixel(d.gpu.display(), 0, 0), 0xdead_beef);
}

/// The host memory budget comes across with the resources: the
/// destination refuses what the source would have.
#[test]
fn the_memory_budget_survives_a_migration() {
    let g = Guest::new();
    // Half the budget, twice over, leaves nothing.
    let side = ((MAX_HOST_BYTES / 2 / BYTES_PER_PIXEL) as f64).sqrt() as u32;
    assert_eq!(
        g.create(1, FORMAT_B8G8R8X8_UNORM, side, side),
        RESP_OK_NODATA
    );
    assert_eq!(
        g.create(2, FORMAT_B8G8R8X8_UNORM, side, side),
        RESP_OK_NODATA
    );
    let d = migrate(&g);
    assert_eq!(
        d.create(3, FORMAT_B8G8R8X8_UNORM, side, side),
        RESP_ERR_OUT_OF_MEMORY
    );
}

/// 0x7FFF_FFFF by 0x8000_0001 pixels is 2^64 - 4 bytes: the size fits a
/// usize, and only the resource's record takes it past.
#[test]
fn create_refuses_a_resource_whose_record_passes_the_address_space() {
    let g = Guest::new();
    assert_eq!(
        g.create(1, FORMAT_B8G8R8X8_UNORM, 0x7FFF_FFFF, 0x8000_0001),
        RESP_ERR_OUT_OF_MEMORY
    );
}

#[test]
fn the_cursor_size_and_events_survive_a_migration() {
    use vmm_devices::display::DisplaySource;
    let g = Guest::new();
    g.shown(1, 8, 6);
    let (w, h) = (CURSOR_SIZE, CURSOR_SIZE);
    assert_eq!(g.create(2, FORMAT_B8G8R8A8_UNORM, w, h), RESP_OK_NODATA);
    let at = 0x40_0000u64;
    assert_eq!(
        g.attach(2, &[entry(BACKING_GPA + at, w * h * 4)]),
        RESP_OK_NODATA
    );
    g.fill_backing(at, &vec![0x5a; (w * h * 4) as usize]);
    assert_eq!(g.transfer(2, rect(0, 0, w, h), 0), RESP_OK_NODATA);
    cursor_cmd(
        &g,
        CMD_UPDATE_CURSOR,
        UpdateCursor {
            pos: CursorPos {
                scanout_id: 0,
                x: 5,
                y: 6,
                padding: 0,
            },
            resource_id: 2,
            hot_x: 3,
            hot_y: 4,
            padding: 0,
        },
    );
    assert!(g.gpu.display().request_size(1280, 800));

    let d = migrate(&g);
    let cursor = d.gpu.display().cursor().expect("the cursor came across");
    assert_eq!(
        (cursor.x, cursor.y, cursor.hot_x, cursor.hot_y),
        (5, 6, 3, 4)
    );
    assert_eq!(cursor.pixels, vec![0x5a; (w * h * 4) as usize]);
    assert_eq!(d.gpu.display().preferred(), (1280, 800));
    // A server on the destination sends the pointer to its clients.
    assert_ne!(DisplaySource::cursor_serial(d.gpu.display()), 0);
    assert_eq!(d.gpu.cfg_read(0, 4), EVENT_DISPLAY);
}

/// A guest may attach backing it never draws from. The migration must
/// not fail over it: the resource comes back blank.
#[test]
fn a_resource_whose_backing_cannot_be_read_comes_back_blank() {
    let g = Guest::new();
    assert_eq!(g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(g.attach(1, &[entry(UNMAPPED_GPA, 64)]), RESP_OK_NODATA);
    // Blank, but not silently: a black screen after the move says why.
    let records = Arc::new(AtomicUsize::new(0));
    let dest = Guest::logged(slog::Logger::root(
        CountingDrain(Arc::clone(&records)),
        slog::o!(),
    ));
    let d = migrate_into(&g, gpu_state(&g), dest);
    assert_eq!(records.load(Ordering::Relaxed), 1);
    assert_eq!(
        d.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4),
        RESP_ERR_INVALID_RESOURCE_ID
    );
}

/// A payload with no state for this device is one from a peer that
/// cannot carry it: refused, not taken as an empty device.
#[test]
fn a_payload_without_gpu_state_is_refused() {
    let d = Guest::new();
    assert!(
        VirtioDevice::restore_device_state(&d.gpu, None, &d.physmap).is_err()
    );
}

/// The peer is unauthenticated: state this device could not have held
/// is refused whole, and nothing of it is kept.
#[test]
fn state_the_device_could_not_hold_is_refused() {
    let good = || {
        let g = Guest::new();
        g.shown(1, 8, 6);
        gpu_state(&g)
    };
    let res = |id, width, height| GpuMigrateResource {
        id,
        width,
        height,
        backing: None,
    };
    let blob = |id, size| GpuMigrateBlob {
        id,
        size,
        backing: vec![(BACKING_GPA, 4096)],
    };
    let image = |width, height, stride, offset| GpuMigrateBlobImage {
        width,
        height,
        stride,
        offset,
    };
    // A blob shown on the scanout, which the restore accepts.
    let blob_good = || {
        let g = Guest::new();
        assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
        assert_eq!(
            g.set_scanout_blob(blob_scanout(1, 8, 6, 32, 0)),
            RESP_OK_NODATA
        );
        gpu_state(&g)
    };
    let side = ((MAX_HOST_BYTES / BYTES_PER_PIXEL) as f64).sqrt() as u32;
    let bad: Vec<(&str, GpuMigrateState)> = vec![
        ("resource id 0", {
            let mut s = good();
            s.resources.push(res(0, 4, 4));
            s
        }),
        ("a duplicate id", {
            let mut s = good();
            s.resources.push(res(1, 4, 4));
            s
        }),
        ("an empty resource", {
            let mut s = good();
            s.resources.push(res(2, 0, 4));
            s
        }),
        ("a resource whose size and record pass the address space", {
            let mut s = good();
            s.resources.push(res(2, 0x7FFF_FFFF, 0x8000_0001));
            s
        }),
        ("past the memory budget", {
            let mut s = good();
            s.resources.push(res(2, side, side));
            s
        }),
        ("backing past the memory budget", {
            let mut s = good();
            s.resources.push(GpuMigrateResource {
                backing: Some(vec![(0, 4096); MAX_BACKING_ENTRIES as usize]),
                ..res(2, side - 1, side - 1)
            });
            s
        }),
        ("too many backing entries", {
            let mut s = good();
            s.resources.push(GpuMigrateResource {
                backing: Some(vec![
                    (0, 4096);
                    MAX_BACKING_ENTRIES as usize + 1
                ]),
                ..res(2, 4, 4)
            });
            s
        }),
        ("a scanout of no resource", {
            let mut s = good();
            s.scanout.as_mut().expect("shown").resource_id = 9;
            s
        }),
        ("a scanout outside its resource", {
            let mut s = good();
            s.scanout.as_mut().expect("shown").width = 9;
            s
        }),
        ("an empty scanout", {
            let mut s = good();
            s.scanout.as_mut().expect("shown").height = 0;
            s
        }),
        ("a blob with id 0", {
            let mut s = good();
            s.blobs.push(blob(0, 4096));
            s
        }),
        ("a blob with a 2D resource's id", {
            let mut s = good();
            s.blobs.push(blob(1, 4096));
            s
        }),
        ("two blobs with one id", {
            let mut s = good();
            s.blobs.push(blob(2, 4096));
            s.blobs.push(blob(2, 4096));
            s
        }),
        ("an empty blob", {
            let mut s = good();
            s.blobs.push(blob(2, 0));
            s
        }),
        ("a blob longer than its pages", {
            let mut s = good();
            s.blobs.push(blob(2, 4097));
            s
        }),
        ("a blob with too many entries", {
            let mut s = good();
            s.blobs.push(GpuMigrateBlob {
                backing: vec![(0, 4096); MAX_BACKING_ENTRIES as usize + 1],
                ..blob(2, 4096)
            });
            s
        }),
        ("a blob list past the memory budget", {
            let mut s = good();
            s.resources.push(res(2, side - 1, side - 1));
            s.blobs.push(GpuMigrateBlob {
                backing: vec![(0, 4096); MAX_BACKING_ENTRIES as usize],
                ..blob(3, 4096)
            });
            s
        }),
        ("a blob image on a 2D resource", {
            let mut s = good();
            s.scanout.as_mut().expect("shown").blob = Some(image(8, 6, 32, 0));
            s
        }),
        ("a 2D scanout of a blob", {
            let mut s = blob_good();
            s.scanout.as_mut().expect("shown").blob = None;
            s
        }),
        ("a blob image past the blob's end", {
            let mut s = blob_good();
            s.scanout.as_mut().expect("shown").blob =
                Some(image(8, 6, 32, 4000));
            s
        }),
        ("a blob image with rows that overlap", {
            let mut s = blob_good();
            s.scanout.as_mut().expect("shown").blob = Some(image(8, 6, 28, 0));
            s
        }),
        ("a window outside the blob image", {
            let mut s = blob_good();
            s.scanout.as_mut().expect("shown").width = 9;
            s
        }),
        ("a blob window larger than any resource", {
            let mut s = good();
            let side = 1u32 << 16;
            s.blobs.push(GpuMigrateBlob {
                id: 2,
                size: 1 << 34,
                backing: vec![(BACKING_GPA, u32::MAX); 5],
            });
            s.scanout = Some(GpuMigrateScanout {
                resource_id: 2,
                x: 0,
                y: 0,
                width: side,
                height: side,
                blob: Some(image(side, side, side * 4, 0)),
            });
            s
        }),
        ("a blob window whose bytes pass a u64", {
            let mut s = blob_good();
            let shown = s.scanout.as_mut().expect("shown");
            shown.width = u32::MAX;
            shown.height = u32::MAX;
            s
        }),
        ("a cursor of the wrong size", {
            let mut s = good();
            s.cursor = Some(GpuMigrateCursor {
                pixels: vec![0; 16],
                ..Default::default()
            });
            s
        }),
        ("an event no device raises", {
            let mut s = good();
            s.events = 0x80;
            s
        }),
        ("an offered size of nothing", {
            let mut s = good();
            s.preferred = (0, 0);
            s
        }),
        ("an offered size past the budget", {
            let mut s = good();
            s.preferred = (u32::MAX, u32::MAX);
            s
        }),
        ("a cursor hotspot outside the cursor", {
            let mut s = good();
            s.cursor = Some(GpuMigrateCursor {
                hot_x: CURSOR_SIZE,
                pixels: vec![0; (CURSOR_SIZE * CURSOR_SIZE * 4) as usize],
                ..Default::default()
            });
            s
        }),
    ];
    for (what, state) in bad {
        let d = Guest::new();
        let payload = VirtioDeviceMigrateState::Gpu(state);
        assert!(
            VirtioDevice::restore_device_state(
                &d.gpu,
                Some(&payload),
                &d.physmap
            )
            .is_err(),
            "{what} was accepted"
        );
        assert_eq!(
            d.create(1, FORMAT_B8G8R8X8_UNORM, 8, 6),
            RESP_OK_NODATA,
            "{what}: a refused restore kept a resource"
        );
    }
    let d = Guest::new();
    let sound = VirtioDeviceMigrateState::Sound(Default::default());
    assert!(VirtioDevice::restore_device_state(
        &d.gpu,
        Some(&sound),
        &d.physmap
    )
    .is_err());
}

// ---------------------------------------------------------------------------
// Blob resources
// ---------------------------------------------------------------------------

/// A scanout of a `width` x `height` blob image with rows `stride`
/// bytes apart, starting `offset` bytes in.
fn blob_scanout(
    id: u32,
    width: u32,
    height: u32,
    stride: u32,
    offset: u32,
) -> SetScanoutBlob {
    SetScanoutBlob {
        r: rect(0, 0, width, height),
        scanout_id: 0,
        resource_id: id,
        width,
        height,
        format: FORMAT_B8G8R8X8_UNORM,
        padding: 0,
        strides: [stride, 0, 0, 0],
        offsets: [offset, 0, 0, 0],
    }
}

/// Fill a blob image at BACKING_GPA with each pixel set to its index,
/// rows `stride` bytes apart from `offset`.
fn fill_image(g: &Guest, width: u32, height: u32, stride: u32, offset: u32) {
    for y in 0..height {
        let row: Vec<u8> = (0..width)
            .flat_map(|x| (y * width + x).to_le_bytes())
            .collect();
        g.fill_backing(u64::from(offset + y * stride), &row);
    }
}

/// A blob of `size` bytes at BACKING_GPA, split into pages the way
/// Linux sends a buffer that is not contiguous.
fn blob_pages(size: u32) -> Vec<MemEntry> {
    (0..size.div_ceil(4096))
        .map(|i| entry(BACKING_GPA + u64::from(i) * 4096, 4096))
        .collect()
}

#[test]
fn a_blob_is_shown_from_the_guest_pages() {
    let g = Guest::new();
    let (w, h, stride, offset) = (8u32, 6u32, 64u32, 128u32);
    let size = 4096;
    assert_eq!(g.create_blob(1, size, &blob_pages(4096)), RESP_OK_NODATA);
    fill_image(&g, w, h, stride, offset);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, w, h, stride, offset)),
        RESP_OK_NODATA
    );
    let display = g.gpu.display();
    assert_eq!(display.size(), (w, h));
    for (x, y) in [(0, 0), (7, 0), (3, 4), (7, 5)] {
        assert_eq!(display_pixel(display, x, y), y * w + x, "({x}, {y})");
    }
    assert_eq!(display.take_damage(), Some(rect(0, 0, w, h)));
}

/// The guest draws straight into the blob: a flush shows what its
/// pages hold now, with no transfer, and damages only its rectangle.
#[test]
fn a_flush_of_a_blob_reads_the_pages_again() {
    let g = Guest::new();
    let (w, h, stride) = (8u32, 6u32, 32u32);
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, w, h, stride, 0)),
        RESP_OK_NODATA
    );
    g.gpu.display().take_damage();
    // Pixel (2, 3).
    g.fill_backing(
        u64::from(3 * stride + 2 * 4),
        &0xdead_beefu32.to_le_bytes(),
    );
    assert_eq!(g.flush(1, rect(2, 3, 1, 1)), RESP_OK_NODATA);
    assert_eq!(display_pixel(g.gpu.display(), 2, 3), 0xdead_beef);
    assert_eq!(g.gpu.display().take_damage(), Some(rect(2, 3, 1, 1)));
}

/// Linux still sends a transfer for a dumb buffer it made a blob. The
/// device reads the pages at flush, so there is nothing to copy.
#[test]
fn a_transfer_to_a_blob_is_accepted() {
    let g = Guest::new();
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(g.transfer(1, rect(0, 0, 8, 6), 0), RESP_OK_NODATA);
}

/// A scanout may show a window of the image, as with a 2D resource.
#[test]
fn a_blob_scanout_can_show_part_of_the_image() {
    let g = Guest::new();
    let (w, h, stride) = (8u32, 6u32, 32u32);
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    fill_image(&g, w, h, stride, 0);
    let mut s = blob_scanout(1, w, h, stride, 0);
    s.r = rect(2, 1, 4, 3);
    assert_eq!(g.set_scanout_blob(s), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().size(), (4, 3));
    assert_eq!(display_pixel(g.gpu.display(), 0, 0), 8 + 2);
    assert_eq!(display_pixel(g.gpu.display(), 3, 2), 3 * 8 + 5);
    g.gpu.display().take_damage();
    assert_eq!(g.flush(1, rect(0, 0, w, h)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().take_damage(), Some(rect(0, 0, 4, 3)));
}

#[test]
fn create_blob_refuses_what_it_cannot_hold() {
    let g = Guest::new();
    assert_eq!(
        g.create_blob(0, 4096, &blob_pages(4096)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.create_blob(1, 4096, &blob_pages(4096)),
        RESP_ERR_INVALID_RESOURCE_ID,
        "a blob id is taken"
    );
    assert_eq!(g.create(2, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    assert_eq!(
        g.create_blob(2, 4096, &blob_pages(4096)),
        RESP_ERR_INVALID_RESOURCE_ID,
        "a 2D id is taken"
    );
    assert_eq!(
        g.create(1, FORMAT_B8G8R8X8_UNORM, 4, 4),
        RESP_ERR_INVALID_RESOURCE_ID,
        "2D sees blob ids"
    );
    assert_eq!(
        g.create_blob(3, 0, &[]),
        RESP_ERR_INVALID_PARAMETER,
        "empty"
    );
    assert_eq!(
        g.create_blob(3, 8192, &blob_pages(4096)),
        RESP_ERR_INVALID_PARAMETER,
        "pages shorter than the blob"
    );
    let mut body = ResourceCreateBlob {
        resource_id: 3,
        blob_mem: 2,
        blob_flags: 0,
        nr_entries: 1,
        blob_id: 0,
        size: 4096,
    }
    .as_bytes()
    .to_vec();
    body.extend_from_slice(entry(BACKING_GPA, 4096).as_bytes());
    assert_eq!(
        g.cmd(CMD_RESOURCE_CREATE_BLOB, &body),
        RESP_ERR_INVALID_PARAMETER,
        "host memory blobs are not offered"
    );
    body.truncate(8);
    assert_eq!(g.cmd(CMD_RESOURCE_CREATE_BLOB, &body), RESP_ERR_UNSPEC);
}

/// A blob's page list is host memory, so it counts against the budget
/// as a 2D resource's does, and unref gives it back.
#[test]
fn a_blob_page_list_counts_against_the_budget() {
    // Room for exactly the list and its record: leave_room rounds to
    // whole rows, and 32 KiB is a whole number of them.
    let list = an_exact_list();
    let g = Guest::new();
    leave_room(&g, 32 << 10);
    let longer = vec![entry(BACKING_GPA, 4096); list.len() + 1];
    assert_eq!(g.create_blob(10, 4096, &longer), RESP_ERR_OUT_OF_MEMORY);
    assert_eq!(g.create_blob(10, 4096, &list), RESP_OK_NODATA);
    assert_eq!(
        g.create_blob(11, 4096, &blob_pages(4096)),
        RESP_ERR_OUT_OF_MEMORY
    );
    assert_eq!(g.unref(10), RESP_OK_NODATA);
    assert_eq!(g.create_blob(11, 4096, &blob_pages(4096)), RESP_OK_NODATA);
}

#[test]
fn create_blob_refuses_too_many_entries() {
    let g = Guest::new();
    // Every entry is in the chain: the limit refuses it, not the length.
    let many = vec![entry(BACKING_GPA, 4096); MAX_BACKING_ENTRIES as usize + 1];
    assert_eq!(g.create_blob(1, 4096, &many), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(g.create_blob(1, 4096, &many[1..]), RESP_OK_NODATA);
}

#[test]
fn set_scanout_blob_refuses_an_image_outside_the_blob() {
    let g = Guest::new();
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(g.create(2, FORMAT_B8G8R8X8_UNORM, 4, 4), RESP_OK_NODATA);
    let ok = blob_scanout(1, 8, 6, 32, 0);
    let bad: Vec<(&str, SetScanoutBlob, u32)> = vec![
        (
            "scanout 1",
            SetScanoutBlob {
                scanout_id: 1,
                ..ok
            },
            RESP_ERR_INVALID_SCANOUT_ID,
        ),
        (
            "no resource",
            SetScanoutBlob {
                resource_id: 9,
                ..ok
            },
            RESP_ERR_INVALID_RESOURCE_ID,
        ),
        (
            "a 2D resource",
            SetScanoutBlob {
                resource_id: 2,
                ..ok
            },
            RESP_ERR_INVALID_RESOURCE_ID,
        ),
        (
            "a format of 3 bytes",
            SetScanoutBlob { format: 0, ..ok },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            "rows overlap",
            SetScanoutBlob {
                strides: [28, 0, 0, 0],
                ..ok
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            "past the end",
            SetScanoutBlob {
                offsets: [4096 - 5 * 32 - 31, 0, 0, 0],
                ..ok
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            "a huge stride",
            SetScanoutBlob {
                strides: [u32::MAX, 0, 0, 0],
                ..ok
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            "a window outside the image",
            SetScanoutBlob {
                r: rect(1, 0, 8, 6),
                ..ok
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            "an empty window",
            SetScanoutBlob {
                r: rect(0, 0, 0, 6),
                ..ok
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            "a window whose bytes pass a u64",
            SetScanoutBlob {
                r: rect(0, 0, u32::MAX, u32::MAX),
                ..ok
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
    ];
    for (what, s, want) in bad {
        assert_eq!(g.set_scanout_blob(s), want, "{what}");
        assert_eq!(g.gpu.display().size(), (0, 0), "{what} was shown");
    }
    // A window bigger than any resource could be is refused before the
    // display is sized for it, however large the blob says it is.
    let side = 1u32 << 16;
    let huge = vec![entry(BACKING_GPA, u32::MAX); 5];
    assert_eq!(g.create_blob(3, 1 << 34, &huge), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(3, side, side, side * 4, 0)),
        RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(g.gpu.display().size(), (0, 0));
    // The last row may end exactly at the end of the blob.
    let edge = SetScanoutBlob {
        offsets: [4096 - 5 * 32 - 32, 0, 0, 0],
        ..ok
    };
    assert_eq!(g.set_scanout_blob(edge), RESP_OK_NODATA);
    // A 2D scanout does not show a blob.
    assert_eq!(
        g.set_scanout(0, 1, rect(0, 0, 8, 6)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
}

/// The guest decides what memory a blob is, so pages that are not
/// mapped refuse the scanout rather than show something.
#[test]
fn a_blob_on_unmapped_pages_is_refused_at_scanout() {
    let g = Guest::new();
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, 8, 6, 32, 0)),
        RESP_OK_NODATA
    );
    assert_eq!(
        g.create_blob(2, 4096, &[entry(UNMAPPED_GPA, 4096)]),
        RESP_OK_NODATA
    );
    assert_eq!(
        g.set_scanout_blob(blob_scanout(2, 8, 6, 32, 0)),
        RESP_ERR_UNSPEC
    );
    // What the display had was partly overwritten: it is off, not a
    // mix of two pictures.
    assert_eq!(g.gpu.display().size(), (0, 0));
}

#[test]
fn a_flush_outside_the_blob_image_is_refused() {
    let g = Guest::new();
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, 8, 6, 32, 0)),
        RESP_OK_NODATA
    );
    assert_eq!(g.flush(1, rect(0, 0, 9, 6)), RESP_ERR_INVALID_PARAMETER);
}

#[test]
fn unref_of_the_shown_blob_disables_the_scanout() {
    let g = Guest::new();
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, 8, 6, 32, 0)),
        RESP_OK_NODATA
    );
    assert_eq!(g.unref(1), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().size(), (0, 0));
    assert_eq!(g.flush(1, rect(0, 0, 8, 6)), RESP_ERR_INVALID_RESOURCE_ID);
}

/// A page flip from a blob to another blob shows the new one, and a 2D
/// scanout after a blob one no longer reads the blob.
#[test]
fn a_flip_between_blobs_shows_the_new_one() {
    let g = Guest::new();
    let (w, h, stride) = (8u32, 6u32, 32u32);
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.create_blob(2, 4096, &[entry(BACKING_GPA + 0x4_0000, 4096)]),
        RESP_OK_NODATA
    );
    g.fill_backing(0x4_0000, &7u32.to_le_bytes());
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, w, h, stride, 0)),
        RESP_OK_NODATA
    );
    assert_eq!(
        g.set_scanout_blob(blob_scanout(2, w, h, stride, 0)),
        RESP_OK_NODATA
    );
    assert_eq!(display_pixel(g.gpu.display(), 0, 0), 7);
    // Resource 1 is off the scanout: its flush changes nothing.
    g.gpu.display().take_damage();
    assert_eq!(g.flush(1, rect(0, 0, w, h)), RESP_OK_NODATA);
    assert_eq!(g.gpu.display().take_damage(), None);
}

/// Linux sends no layout for a cursor: QEMU reads a blob cursor as a
/// packed 64x64 image from its start, and so does this device.
#[test]
fn a_blob_cursor_is_read_from_its_pages() {
    let g = Guest::new();
    let size = CURSOR_SIZE * CURSOR_SIZE * 4;
    assert_eq!(
        g.create_blob(2, u64::from(size), &blob_pages(size)),
        RESP_OK_NODATA
    );
    let px: Vec<u8> = (0..CURSOR_SIZE * CURSOR_SIZE)
        .flat_map(|i| (i | 0xff00_0000).to_le_bytes())
        .collect();
    g.fill_backing(0, &px);
    cursor_cmd(&g, CMD_UPDATE_CURSOR, cursor_at(2, 5, 6));
    let cursor = g.gpu.display().cursor().expect("a cursor was set");
    assert_eq!(cursor.pixels, px);
}

#[test]
fn a_blob_too_small_for_a_cursor_is_refused() {
    let g = Guest::new();
    // The pages would hold a cursor; the blob is only a page of them.
    let size = CURSOR_SIZE * CURSOR_SIZE * 4;
    assert_eq!(g.create_blob(2, 4096, &blob_pages(size)), RESP_OK_NODATA);
    cursor_cmd(&g, CMD_UPDATE_CURSOR, cursor_at(2, 5, 6));
    assert!(g.gpu.display().cursor().is_none());
}

#[test]
fn reset_drops_every_blob() {
    let g = Guest::new();
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, 8, 6, 32, 0)),
        RESP_OK_NODATA
    );
    VirtioDevice::reset(&g.gpu);
    assert_eq!(g.gpu.display().size(), (0, 0));
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
}

/// A blob-showing guest moves with its picture: the destination reads
/// the image from the pages the migration carried, and the guest's
/// draws carry on reaching the display.
#[test]
fn a_blob_picture_survives_a_migration() {
    let g = Guest::new();
    let (w, h, stride, offset) = (8u32, 6u32, 64u32, 128u32);
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    fill_image(&g, w, h, stride, offset);
    let mut s = blob_scanout(1, w, h, stride, offset);
    s.r = rect(1, 1, 6, 4);
    assert_eq!(g.set_scanout_blob(s), RESP_OK_NODATA);

    let d = migrate(&g);
    let display = d.gpu.display();
    assert_eq!(display.size(), (6, 4));
    for (x, y) in [(0, 0), (5, 0), (2, 3), (5, 3)] {
        assert_eq!(
            display_pixel(display, x, y),
            (y + 1) * w + x + 1,
            "({x}, {y})"
        );
    }
    assert_eq!(display.take_damage(), Some(rect(0, 0, 6, 4)));

    d.fill_backing(
        u64::from(offset + stride + 4),
        &0xdead_beefu32.to_le_bytes(),
    );
    assert_eq!(d.flush(1, rect(1, 1, 1, 1)), RESP_OK_NODATA);
    assert_eq!(display_pixel(d.gpu.display(), 0, 0), 0xdead_beef);
}

/// The blob itself comes across: its id is taken, its page list still
/// counts against the budget, and unref frees it.
#[test]
fn blobs_and_their_budget_survive_a_migration() {
    let g = Guest::new();
    let list = an_exact_list();
    leave_room(&g, 32 << 10);
    assert_eq!(g.create_blob(10, 4096, &list), RESP_OK_NODATA);
    let d = migrate(&g);
    assert_eq!(
        d.create_blob(10, 4096, &blob_pages(4096)),
        RESP_ERR_INVALID_RESOURCE_ID,
        "blob 10 still exists"
    );
    assert_eq!(
        d.create_blob(11, 4096, &blob_pages(4096)),
        RESP_ERR_OUT_OF_MEMORY
    );
    assert_eq!(d.unref(10), RESP_OK_NODATA);
    assert_eq!(d.create_blob(11, 4096, &blob_pages(4096)), RESP_OK_NODATA);
}

/// Pages a blob names may not be mapped on the destination's copy
/// either; a shown blob that cannot be read comes back blank rather
/// than failing the migration, as a 2D resource does.
#[test]
fn a_shown_blob_whose_pages_cannot_be_read_comes_back_blank() {
    let g = Guest::new();
    let (w, h, stride) = (8u32, 6u32, 1024u32);
    assert_eq!(g.create_blob(1, 8192, &blob_pages(8192)), RESP_OK_NODATA);
    fill_image(&g, w, h, stride, 0);
    // Index 0 is black; make the first pixel something else.
    g.fill_backing(0, &0xdead_beefu32.to_le_bytes());
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, w, h, stride, 0)),
        RESP_OK_NODATA
    );
    // Rows 0-3 are on the first page, which still reads; 4 and 5 are
    // on the second, which no longer does.
    let mut s = gpu_state(&g);
    s.blobs[0].backing[1] = (UNMAPPED_GPA, 4096);
    let d = migrate_with(&g, s);
    assert_eq!(d.gpu.display().size(), (w, h));
    for (x, y) in [(0, 0), (7, 3), (7, 5)] {
        assert_eq!(display_pixel(d.gpu.display(), x, y), 0, "({x}, {y})");
    }
}

// ---------------------------------------------------------------------------
// Flip pacing
// ---------------------------------------------------------------------------

/// A control ring in guest memory, with one request and response slot
/// per descriptor pair.
struct Ring {
    queues: [VirtQueue; 2],
    posted: u16,
}

const RING_DESC_GPA: u64 = MEM_GPA + 0x8000;
const RING_AVAIL_GPA: u64 = MEM_GPA + 0x9000;
const RING_USED_GPA: u64 = MEM_GPA + 0xa000;
const RING_SLOTS_GPA: u64 = MEM_GPA + 0xb000;
const RING_SIZE: u16 = 16;

impl Ring {
    fn new() -> Self {
        let mut control = VirtQueue::new(RING_SIZE);
        control.set_addr_modern(RING_DESC_GPA, RING_AVAIL_GPA, RING_USED_GPA);
        Self {
            queues: [control, VirtQueue::new(RING_SIZE)],
            posted: 0,
        }
    }

    /// Post `req` with room for a header response.
    fn post(&mut self, g: &Guest, req: &[u8]) {
        let slot = self.posted % (RING_SIZE / 2);
        let req_gpa = RING_SLOTS_GPA + u64::from(slot) * 0x100;
        let resp_gpa = req_gpa + 0x80;
        g.physmap
            .lookup(req_gpa, req.len())
            .expect("mapped")
            .write_bytes(req)
            .expect("write request");
        let head = slot * 2;
        let descs = [
            VirtqDesc {
                addr: req_gpa,
                len: u32::try_from(req.len()).expect("fits"),
                flags: bits::VRING_DESC_F_NEXT,
                next: head + 1,
            },
            VirtqDesc {
                addr: resp_gpa,
                len: 24,
                flags: bits::VRING_DESC_F_WRITE,
                next: 0,
            },
        ];
        for (i, d) in descs.iter().enumerate() {
            g.physmap
                .lookup(RING_DESC_GPA + (u64::from(head) + i as u64) * 16, 16)
                .expect("mapped")
                .write(d)
                .expect("write desc");
        }
        let entry = RING_AVAIL_GPA + 4 + u64::from(self.posted % RING_SIZE) * 2;
        g.physmap
            .lookup(entry, 2)
            .expect("mapped")
            .write::<u16>(&head)
            .expect("avail entry");
        self.posted = self.posted.wrapping_add(1);
        g.physmap
            .lookup(RING_AVAIL_GPA + 2, 2)
            .expect("mapped")
            .write::<u16>(&self.posted)
            .expect("avail idx");
        g.gpu
            .notify_queue(QUEUE_CONTROL, &mut self.queues, &g.physmap);
    }

    /// How many requests the device has answered.
    fn answered(&self, g: &Guest) -> u16 {
        g.physmap
            .lookup(RING_USED_GPA + 2, 2)
            .expect("mapped")
            .read::<u16>()
            .expect("used idx")
    }

    /// Wait up to five seconds for `n` answers; when they came.
    fn wait_for(&self, g: &Guest, n: u16) -> Instant {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.answered(g) < n {
            assert!(Instant::now() < deadline, "{n} answers never came");
            thread::sleep(Duration::from_millis(1));
        }
        Instant::now()
    }
}

/// A flush with a fence, as Linux sends for a blob primary plane.
fn fenced_flush(id: u32) -> Vec<u8> {
    let mut req = CtrlHdr {
        flags: FLAG_FENCE,
        fence_id: 7,
        ..hdr(CMD_RESOURCE_FLUSH)
    }
    .as_bytes()
    .to_vec();
    req.extend_from_slice(
        ResourceFlush {
            r: rect(0, 0, 8, 6),
            resource_id: id,
            padding: 0,
        }
        .as_bytes(),
    );
    req
}

fn unfenced_flush(id: u32) -> Vec<u8> {
    let mut req = fenced_flush(id);
    req[4..8].copy_from_slice(&0u32.to_le_bytes());
    req
}

/// Raises for the control queue the device made.
fn count_raises(g: &Guest) -> Arc<AtomicUsize> {
    let raised = Arc::new(AtomicUsize::new(0));
    let r = Arc::clone(&raised);
    g.gpu
        .install_interrupt(BackendIntr::detached(move |_, queue| {
            if queue == QUEUE_CONTROL {
                r.fetch_add(1, Ordering::SeqCst);
            }
        }));
    raised
}

/// Linux waits on a fenced flush's answer before it flips again, so
/// answering no sooner than a frame after the last caps its flip rate.
/// The picture itself is not held: the flush shows at once.
#[test]
fn a_flip_soon_after_another_is_answered_a_frame_later() {
    let interval = Duration::from_millis(300);
    let g = Guest::paced(1024, 768, interval);
    let raised = count_raises(&g);
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    let start = Instant::now();
    ring.post(&g, &fenced_flush(1));
    assert_eq!(ring.answered(&g), 1, "the first flip is not held");

    g.fill_backing(0, &0xdead_beefu32.to_le_bytes());
    assert_eq!(g.transfer(1, rect(0, 0, 1, 1), 0), RESP_OK_NODATA);
    ring.post(&g, &fenced_flush(1));
    assert_eq!(display_pixel(g.gpu.display(), 0, 0), 0xdead_beef);
    assert_eq!(ring.answered(&g), 1, "the second waits for its frame");
    let answered = ring.wait_for(&g, 2);
    assert!(
        answered - start >= interval,
        "answered after {:?}",
        answered - start
    );
    // The device answered it off the vCPU, so it raised the interrupt.
    let deadline = Instant::now() + Duration::from_secs(5);
    while raised.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "no interrupt for the answer");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Only fenced flushes wait. Everything else is answered as it comes,
/// a flip waiting or not.
#[test]
fn other_requests_are_answered_while_a_flip_waits() {
    let g = Guest::paced(1024, 768, Duration::from_secs(10));
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    assert_eq!(ring.answered(&g), 1);
    ring.post(&g, hdr(CMD_GET_DISPLAY_INFO).as_bytes());
    ring.post(&g, &unfenced_flush(1));
    assert_eq!(ring.answered(&g), 3, "the flip alone waits");
}

/// A guest flipping slower than the pace is never held.
#[test]
fn a_flip_after_a_quiet_frame_is_answered_at_once() {
    let interval = Duration::from_millis(50);
    let g = Guest::paced(1024, 768, interval);
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    ring.post(&g, &fenced_flush(1));
    thread::sleep(interval * 2);
    ring.post(&g, &fenced_flush(1));
    assert_eq!(ring.answered(&g), 2);
}

/// A paused device has nothing in flight: a migration exports the
/// rings next. Pausing answers the waiting flip, flips that come while
/// paused are not held, and pacing comes back on resume.
#[test]
fn pausing_answers_a_waiting_flip() {
    let g = Guest::paced(1024, 768, Duration::from_secs(10));
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    assert!(!Lifecycle::is_quiesced(&g.gpu));
    assert!(
        VirtioDevice::export_device_state(&g.gpu).is_err(),
        "a waiting flip is in flight"
    );

    Lifecycle::pause(&g.gpu);
    ring.wait_for(&g, 2);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !Lifecycle::is_quiesced(&g.gpu) {
        assert!(Instant::now() < deadline, "never quiesced");
        thread::sleep(Duration::from_millis(1));
    }
    assert!(VirtioDevice::export_device_state(&g.gpu).is_ok());
    ring.post(&g, &fenced_flush(1));
    assert_eq!(ring.answered(&g), 3, "not held while paused");

    // The flip answered while paused began a frame.
    Lifecycle::resume(&g.gpu);
    ring.post(&g, &fenced_flush(1));
    assert_eq!(ring.answered(&g), 3, "held again once resumed");
}

/// A reset takes the ring back: the waiting flip is never answered
/// into memory the driver has since freed.
#[test]
fn a_reset_drops_a_waiting_flip() {
    let interval = Duration::from_millis(100);
    let g = Guest::paced(1024, 768, interval);
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    VirtioDevice::reset(&g.gpu);
    assert!(Lifecycle::is_quiesced(&g.gpu), "the answer was dropped");
    thread::sleep(interval * 4);
    assert_eq!(ring.answered(&g), 1);
    assert!(Lifecycle::is_quiesced(&g.gpu));
}

/// So does the driver moving the ring.
#[test]
fn moving_the_ring_drops_a_waiting_flip() {
    let interval = Duration::from_millis(100);
    let g = Guest::paced(1024, 768, interval);
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    VirtioDevice::queue_addr_set(&g.gpu, QUEUE_CONTROL, &ring.queues[0]);
    thread::sleep(interval * 4);
    assert_eq!(ring.answered(&g), 1);
}

/// Halt stops the pacing thread: the instance is going away, and a
/// waiting answer goes with it.
#[test]
fn halt_stops_pacing() {
    let interval = Duration::from_millis(100);
    let g = Guest::paced(1024, 768, interval);
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    Lifecycle::halt(&g.gpu);
    thread::sleep(interval * 4);
    assert_eq!(ring.answered(&g), 1);
}

/// A driver that turned interrupts off for the ring gets none for a
/// waiting answer either.
#[test]
fn a_waiting_answer_raises_only_if_the_driver_wants_it() {
    let g = Guest::paced(1024, 768, Duration::from_millis(100));
    let raised = count_raises(&g);
    g.shown(1, 8, 6);
    let mut ring = Ring::new();
    g.physmap
        .lookup(RING_AVAIL_GPA, 2)
        .expect("mapped")
        .write::<u16>(&bits::VIRTQ_AVAIL_F_NO_INTERRUPT)
        .expect("avail flags");
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    ring.wait_for(&g, 2);
    // The raise, had there been one, comes after the publish.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(raised.load(Ordering::SeqCst), 0);
}

/// A display server that cannot keep up asks for slower flips, and the
/// device answers them that far apart.
#[test]
fn a_display_server_can_slow_the_flips() {
    let g = Guest::paced(1024, 768, Duration::from_millis(1));
    g.shown(1, 8, 6);
    let asked = Duration::from_millis(80);
    vmm_devices::display::DisplaySource::pace(g.gpu.display(), Some(asked));
    let mut ring = Ring::new();
    let start = Instant::now();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    let answered = ring.wait_for(&g, 2);
    assert!(
        answered - start >= asked,
        "answered after {:?}",
        answered - start
    );
    // The frame after a waiting flip's is as long.
    ring.post(&g, &fenced_flush(1));
    let answered = ring.wait_for(&g, 3);
    assert!(
        answered - start >= asked * 2,
        "answered after {:?}",
        answered - start
    );
}

/// Withdrawn, the request leaves the device's own pace.
#[test]
fn a_withdrawn_request_restores_the_devices_own_pace() {
    let g = Guest::paced(1024, 768, Duration::from_millis(1));
    g.shown(1, 8, 6);
    vmm_devices::display::DisplaySource::pace(
        g.gpu.display(),
        Some(Duration::from_secs(10)),
    );
    vmm_devices::display::DisplaySource::pace(g.gpu.display(), None);
    let mut ring = Ring::new();
    ring.post(&g, &fenced_flush(1));
    thread::sleep(Duration::from_millis(5));
    ring.post(&g, &fenced_flush(1));
    assert_eq!(ring.answered(&g), 2);
}

/// A server that keeps up may ask for faster flips than the device's
/// own pace, which is for a guest nobody is watching.
#[test]
fn a_request_may_be_faster_than_the_devices_own_pace() {
    // Far slower than any test thread, so an answer before it is the
    // request's pace and not a slow test.
    let own = Duration::from_secs(10);
    let g = Guest::paced(1024, 768, own);
    g.shown(1, 8, 6);
    vmm_devices::display::DisplaySource::pace(
        g.gpu.display(),
        Some(Duration::from_millis(20)),
    );
    let mut ring = Ring::new();
    let start = Instant::now();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    let answered = ring.wait_for(&g, 2);
    assert!(
        answered - start < own,
        "answered after {:?}",
        answered - start
    );
}

/// But no faster than a display shows: a request below the fastest
/// pace is that pace.
#[test]
fn a_request_is_held_to_the_fastest_pace() {
    let g = Guest::paced(1024, 768, Duration::from_millis(500));
    g.shown(1, 8, 6);
    vmm_devices::display::DisplaySource::pace(
        g.gpu.display(),
        Some(Duration::from_millis(1)),
    );
    let mut ring = Ring::new();
    let start = Instant::now();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    let answered = ring.wait_for(&g, 2);
    assert!(
        answered - start >= FASTEST_FLIP_INTERVAL,
        "answered after {:?}",
        answered - start
    );
}

/// Withdrawn, the request leaves the device's own pace, even when that
/// is slower.
#[test]
fn a_withdrawn_request_restores_a_slower_own_pace() {
    let own = Duration::from_millis(80);
    let g = Guest::paced(1024, 768, own);
    g.shown(1, 8, 6);
    vmm_devices::display::DisplaySource::pace(
        g.gpu.display(),
        Some(Duration::from_millis(20)),
    );
    vmm_devices::display::DisplaySource::pace(g.gpu.display(), None);
    let mut ring = Ring::new();
    let start = Instant::now();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    let answered = ring.wait_for(&g, 2);
    assert!(
        answered - start >= own,
        "answered after {:?}",
        answered - start
    );
}

/// Nor can it stall the guest: past the slowest pace the device keeps,
/// a request is that pace.
#[test]
fn a_request_is_held_to_the_slowest_pace() {
    let g = Guest::paced(1024, 768, Duration::from_millis(1));
    g.shown(1, 8, 6);
    vmm_devices::display::DisplaySource::pace(
        g.gpu.display(),
        Some(Duration::from_secs(3600)),
    );
    let mut ring = Ring::new();
    let start = Instant::now();
    ring.post(&g, &fenced_flush(1));
    ring.post(&g, &fenced_flush(1));
    let answered = ring.wait_for(&g, 2);
    assert!(answered - start >= SLOWEST_FLIP_INTERVAL);
}

#[derive(Default)]
struct Woken(AtomicUsize);

impl vmm_devices::display::DisplayWaker for Woken {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A server waiting on the display, and how often it was woken.
fn waiting_server(g: &Guest) -> Arc<Woken> {
    let woken = Arc::new(Woken::default());
    vmm_devices::display::DisplaySource::on_change(
        g.gpu.display(),
        Arc::downgrade(&woken)
            as std::sync::Weak<dyn vmm_devices::display::DisplayWaker>,
    );
    woken
}

/// A server need not poll: what the guest shows wakes it.
#[test]
fn what_the_guest_shows_wakes_a_server() {
    let g = Guest::new();
    let woken = waiting_server(&g);
    let seen = || woken.0.load(Ordering::SeqCst);

    g.shown(1, 8, 6);
    let after_scanout = seen();
    assert!(after_scanout > 0, "a scanout wakes it");
    assert_eq!(g.flush(1, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    let after_flush = seen();
    assert!(after_flush > after_scanout, "a flush wakes it");
    set_cursor(&g, 2);
    assert!(seen() > after_flush, "a new pointer wakes it");
    let before_unref = seen();
    assert_eq!(g.unref(1), RESP_OK_NODATA);
    assert!(seen() > before_unref, "a scanout going away wakes it");
}

/// So does a blob's scanout and flush, a reset and a migration.
#[test]
fn blobs_resets_and_migrations_wake_a_server() {
    let g = Guest::new();
    let woken = waiting_server(&g);
    let seen = || woken.0.load(Ordering::SeqCst);
    assert_eq!(g.create_blob(1, 4096, &blob_pages(4096)), RESP_OK_NODATA);
    assert_eq!(
        g.set_scanout_blob(blob_scanout(1, 8, 6, 32, 0)),
        RESP_OK_NODATA
    );
    let after_scanout = seen();
    assert!(after_scanout > 0);
    assert_eq!(g.flush(1, rect(0, 0, 8, 6)), RESP_OK_NODATA);
    let after_flush = seen();
    assert!(after_flush > after_scanout);
    VirtioDevice::reset(&g.gpu);
    assert!(seen() > after_flush, "a reset blanks the display");

    let d = Guest::new();
    let woken = waiting_server(&d);
    g.shown(1, 8, 6);
    let state = gpu_state(&g);
    VirtioDevice::restore_device_state(
        &d.gpu,
        Some(&VirtioDeviceMigrateState::Gpu(state)),
        &d.physmap,
    )
    .expect("restore");
    assert!(
        woken.0.load(Ordering::SeqCst) > 0,
        "a restore shows a picture"
    );
}
