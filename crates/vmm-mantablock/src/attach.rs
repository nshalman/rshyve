// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hello and Attach.
//!
//! The engine is trusted with this volume's data and nothing else, so
//! nothing it sends is used before it is checked.

#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use manta_block_ring::control::{
    Attach, AttachOk, Message, ATTACH_DURABLE, PROTOCOL_VERSION,
};
use manta_block_ring::layout::LAYOUT_VERSION;
use manta_block_ring::{Geometry, Region};
use memmap2::MmapOptions;
use rustix::fs::FileType;

use crate::conn::{invalid, Conn};
use crate::split::Slots;
use crate::wake::{adopt, End};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Ask {
    pub(crate) volume: [u8; 16],
    /// 0 for a new one; else a reattach, which must get this one back.
    pub(crate) generation: u64,
    pub(crate) queues: u16,
    pub(crate) depth: u32,
    pub(crate) buf_pages: u32,
    pub(crate) max_blocks: u32,
    /// Accept an engine whose writes are not durable when they complete.
    pub(crate) allow_volatile: bool,
}

pub(crate) struct Pipes {
    /// Write end of the engine's pipe.
    pub(crate) wake_engine: OwnedFd,
    /// Read end of ours.
    pub(crate) wake_me: OwnedFd,
}

pub(crate) struct Attached {
    pub(crate) region: Arc<Region>,
    pub(crate) generation: u64,
    pub(crate) volume_blocks: u64,
    pub(crate) durable: bool,
    pub(crate) max_blocks: u32,
    pub(crate) slots: Slots,
    pub(crate) pipes: Vec<Pipes>,
}

/// What an attachment was granted. A reattach must get the same, as the
/// held requests name its queues, tags and buffer pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) generation: u64,
    pub(crate) geometry: Geometry,
    pub(crate) volume_blocks: u64,
    pub(crate) durable: bool,
    pub(crate) max_blocks: u32,
    pub(crate) slots: Slots,
}

impl Attached {
    pub(crate) fn grant(&self) -> Grant {
        Grant {
            generation: self.generation,
            geometry: *self.region.geometry(),
            volume_blocks: self.volume_blocks,
            durable: self.durable,
            max_blocks: self.max_blocks,
            slots: self.slots,
        }
    }
}

impl Grant {
    /// The Attach that asks for this grant again.
    pub(crate) fn ask(&self, volume: [u8; 16], allow_volatile: bool) -> Ask {
        Ask {
            volume,
            generation: self.generation,
            queues: self.geometry.queues(),
            depth: self.geometry.depth(),
            buf_pages: self.geometry.buf_pages(),
            max_blocks: self.max_blocks,
            allow_volatile,
        }
    }
}

pub(crate) fn attach(
    conn: &mut Conn,
    ask: &Ask,
    deadline: Instant,
) -> io::Result<Attached> {
    let hello = Message::Hello {
        min_version: PROTOCOL_VERSION,
        max_version: PROTOCOL_VERSION,
        features: 0,
    };
    conn.send(&hello, deadline)?;
    match next(conn, deadline)? {
        // This build asks for no features, so a granted one is a bug.
        (Message::HelloAck { version, features }, fds)
            if version == PROTOCOL_VERSION
                && features == 0
                && fds.is_empty() => {}
        (m, _) => return Err(refused("Hello", &m)),
    }
    conn.send(
        &Message::Attach(Attach {
            volume: ask.volume,
            generation: ask.generation,
            queues: ask.queues,
            depth: ask.depth,
            buf_pages: ask.buf_pages,
            max_blocks: ask.max_blocks,
        }),
        deadline,
    )?;
    let (ok, mut fds) = match next(conn, deadline)? {
        (Message::AttachOk(ok), fds) => (ok, fds),
        (m, _) => return Err(attach_refused(&m)),
    };
    let geometry = check_ok(ask, &ok).map_err(invalid)?;
    let want = 1 + 2 * usize::from(geometry.queues());
    if fds.len() != want {
        return Err(invalid(format!(
            "AttachOk carried {} descriptors, not {want}",
            fds.len()
        )));
    }
    let region_fd = fds.remove(0);
    let mut pipes = Vec::with_capacity(usize::from(geometry.queues()));
    let mut it = fds.into_iter();
    while let (Some(wake_engine), Some(wake_me)) = (it.next(), it.next()) {
        adopt(&wake_engine, End::Write)?;
        adopt(&wake_me, End::Read)?;
        pipes.push(Pipes {
            wake_engine,
            wake_me,
        });
    }
    let region = map(region_fd, geometry)?;
    let h = region.read_header().map_err(invalid)?;
    if h.geometry != geometry
        || h.volume_blocks != ok.volume_blocks
        || h.attach_generation != ok.generation
    {
        return Err(invalid("the region header does not match AttachOk"));
    }
    let slots = Slots::new(ok.depth, ok.buf_pages, ok.max_blocks)
        .ok_or_else(|| invalid("no buffer slots"))?;
    Ok(Attached {
        region,
        generation: ok.generation,
        volume_blocks: ok.volume_blocks,
        durable: ok.flags & ATTACH_DURABLE != 0,
        max_blocks: ok.max_blocks,
        slots,
        pipes,
    })
}

fn next(
    conn: &mut Conn,
    deadline: Instant,
) -> io::Result<(Message, Vec<OwnedFd>)> {
    let f = conn
        .recv(deadline)?
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    if !f.fds.is_empty() && !matches!(f.msg, Message::AttachOk(_)) {
        return Err(invalid("descriptors with a frame that takes none"));
    }
    Ok((f.msg, f.fds))
}

fn refused(step: &str, m: &Message) -> io::Error {
    match m {
        Message::Error { code, text } => io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("{step}: engine error {code}: {text}"),
        ),
        other => invalid(format!("{step}: unexpected reply {other:?}")),
    }
}

/// The engine's Error code for a generation it never takes again.
const GENERATION: u16 = 4;

/// An Attach the engine refused for good.
#[derive(Debug)]
struct Final(String);

impl std::fmt::Display for Final {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Final {}

fn attach_refused(m: &Message) -> io::Error {
    let e = refused("Attach", m);
    match m {
        Message::Error {
            code: GENERATION, ..
        } => io::Error::new(e.kind(), Final(e.to_string())),
        _ => e,
    }
}

/// True when the engine refused the Attach for good, so a retry cannot
/// help.
pub(crate) fn is_final(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|e| e.is::<Final>())
}

pub(crate) fn check_ok(ask: &Ask, ok: &AttachOk) -> Result<Geometry, String> {
    if ok.layout_version != LAYOUT_VERSION {
        return Err(format!("region layout {}", ok.layout_version));
    }
    if ok.generation == 0 {
        return Err("generation 0".into());
    }
    if ask.generation != 0 && ok.generation != ask.generation {
        return Err(format!("generation {} on a reattach", ok.generation));
    }
    if ok.queues > ask.queues
        || ok.depth > ask.depth
        || ok.buf_pages > ask.buf_pages
        || ok.max_blocks > ask.max_blocks
    {
        return Err("the engine granted more than was asked".into());
    }
    if ok.max_blocks == 0 || ok.max_blocks > ok.buf_pages {
        return Err(format!("max blocks {}", ok.max_blocks));
    }
    if ok.volume_blocks == 0 {
        return Err("an empty volume".into());
    }
    if ok.volume_blocks.checked_mul(4096).is_none() {
        return Err("the volume is too large".into());
    }
    if ok.flags & ATTACH_DURABLE == 0 && !ask.allow_volatile {
        return Err("the engine's writes are not durable".into());
    }
    let g = Geometry::new(ok.queues, ok.depth, ok.buf_pages)
        .map_err(|e| e.to_string())?;
    if g.region_len() as u64 != ok.region_len {
        return Err("the region length does not match the geometry".into());
    }
    Ok(g)
}

/// A file shorter than the geometry would raise SIGBUS on a later
/// access past its end.
fn map(fd: OwnedFd, geometry: Geometry) -> io::Result<Arc<Region>> {
    let st = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(invalid("the region is not a regular file"));
    }
    let len = geometry.region_len();
    if !u64::try_from(st.st_size).is_ok_and(|size| size >= len as u64) {
        return Err(invalid("the region file is shorter than its geometry"));
    }
    let file = File::from(fd);
    let map = MmapOptions::new().len(len).map_raw(&file)?;
    let base = map.as_mut_ptr();
    // SAFETY: `map` is a shared read-write mapping of `len` bytes, page
    // aligned by mmap. The Region keeps `map` until it drops, so the
    // mapping outlives every ring handle. This process reaches the bytes
    // only through the Region, which uses atomics, so the engine writing
    // them at any time is sound. The engine is trusted not to shrink the
    // file while attached, and not to leave it sparse on a tmpfs that can
    // fill: either raises SIGBUS on a later access, which nothing guards
    // against.
    let region = unsafe { Region::with_owner(base, len, geometry, map) }
        .map_err(invalid)?;
    Ok(Arc::new(region))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> Ask {
        Ask {
            volume: [1; 16],
            generation: 0,
            queues: 2,
            depth: 64,
            buf_pages: 1024,
            max_blocks: 32,
            allow_volatile: false,
        }
    }

    fn ok() -> AttachOk {
        let g = Geometry::new(2, 64, 1024).unwrap();
        AttachOk {
            generation: 3,
            queues: 2,
            depth: 64,
            buf_pages: 1024,
            max_blocks: 32,
            volume_blocks: 1000,
            region_len: g.region_len() as u64,
            layout_version: LAYOUT_VERSION,
            flags: ATTACH_DURABLE,
        }
    }

    #[test]
    fn a_grant_within_the_ask_is_taken() {
        assert!(check_ok(&ask(), &ok()).is_ok());
        let mut less = ok();
        less.queues = 1;
        less.depth = 16;
        less.buf_pages = 64;
        less.max_blocks = 8;
        less.region_len = Geometry::new(1, 16, 64).unwrap().region_len() as u64;
        assert!(check_ok(&ask(), &less).is_ok());
    }

    #[test]
    fn a_grant_the_spec_does_not_allow_is_refused() {
        let cases: [fn(&mut AttachOk); 9] = [
            |o| o.layout_version = 2,
            |o| o.generation = 0,
            |o| o.queues = 3,
            |o| o.depth = 128,
            |o| o.buf_pages = 2048,
            |o| o.max_blocks = 0,
            |o| o.volume_blocks = u64::MAX,
            |o| o.region_len += 4096,
            |o| o.flags = 0,
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut o = ok();
            f(&mut o);
            assert!(check_ok(&ask(), &o).is_err(), "case {i}");
        }
        let mut again = ask();
        again.generation = 3;
        assert!(check_ok(&again, &ok()).is_ok());
        again.generation = 4;
        assert!(check_ok(&again, &ok()).is_err());
        let mut volatile = ask();
        volatile.allow_volatile = true;
        let mut o = ok();
        o.flags = 0;
        assert!(check_ok(&volatile, &o).is_ok());
    }

    #[test]
    fn a_short_or_special_region_file_is_not_mapped() {
        let g = Geometry::new(1, 16, 4).unwrap();
        let f = tempfile::tempfile().unwrap();
        f.set_len(g.region_len() as u64 - 4096).unwrap();
        assert!(map(OwnedFd::from(f), g).is_err());
        let (rx, _tx) = rustix::pipe::pipe().unwrap();
        assert!(map(rx, g).is_err());
        let f = tempfile::tempfile().unwrap();
        f.set_len(g.region_len() as u64).unwrap();
        assert!(map(OwnedFd::from(f), g).is_ok());
    }

    /// Plays the engine's side of Hello and Attach over a real socket,
    /// sending `fds` with AttachOk.
    fn engine(
        theirs: std::os::unix::net::UnixStream,
        reply: AttachOk,
        fds: Vec<OwnedFd>,
    ) -> std::thread::JoinHandle<()> {
        use std::io::{IoSlice, Read, Write};
        use std::mem::MaybeUninit;
        use std::os::fd::{AsFd, BorrowedFd};

        use rustix::net::{
            SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
        };

        std::thread::spawn(move || {
            let mut s = theirs;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            for step in 0..2 {
                let msg = loop {
                    if let Some((m, _, used)) = Message::decode(&buf).unwrap() {
                        buf.drain(..used);
                        break m;
                    }
                    let n = s.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                };
                if step == 0 {
                    assert!(matches!(msg, Message::Hello { .. }));
                    let ack = Message::HelloAck {
                        version: PROTOCOL_VERSION,
                        features: 0,
                    };
                    s.write_all(&ack.encode(1)).unwrap();
                    continue;
                }
                assert!(matches!(msg, Message::Attach(_)));
                let frame = Message::AttachOk(reply.clone()).encode(2);
                let borrowed: Vec<BorrowedFd<'_>> =
                    fds.iter().map(|f| f.as_fd()).collect();
                let mut space =
                    [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(64))];
                let mut anc = SendAncillaryBuffer::new(&mut space);
                assert!(anc.push(SendAncillaryMessage::ScmRights(&borrowed)));
                rustix::net::sendmsg(
                    &s,
                    &[IoSlice::new(&frame)],
                    &mut anc,
                    SendFlags::empty(),
                )
                .unwrap();
            }
            // Hold the socket until the other side is done with it.
            let _ = s.read(&mut chunk);
        })
    }

    /// A region file for `g` with its header, the AttachOk that names
    /// it, and the ask that fits.
    fn granted() -> (Ask, AttachOk, File) {
        let g = Geometry::new(1, 16, 4).unwrap();
        let file = tempfile::tempfile().unwrap();
        file.set_len(g.region_len() as u64).unwrap();
        let region = map(OwnedFd::from(file.try_clone().unwrap()), g).unwrap();
        region.write_header(&manta_block_ring::Header {
            geometry: g,
            volume_blocks: 100,
            attach_generation: 9,
        });
        let ask = Ask {
            volume: [1; 16],
            generation: 0,
            queues: 1,
            depth: 16,
            buf_pages: 4,
            max_blocks: 1,
            allow_volatile: false,
        };
        let ok = AttachOk {
            generation: 9,
            queues: 1,
            depth: 16,
            buf_pages: 4,
            max_blocks: 1,
            volume_blocks: 100,
            region_len: g.region_len() as u64,
            layout_version: LAYOUT_VERSION,
            flags: ATTACH_DURABLE,
        };
        (ask, ok, file)
    }

    fn attach_with(
        fds: impl FnOnce(File) -> Vec<OwnedFd>,
    ) -> io::Result<Attached> {
        let (ask, ok, file) = granted();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let eng = engine(theirs, ok, fds(file));
        let mut conn = Conn::new(ours).unwrap();
        let got = attach(
            &mut conn,
            &ask,
            Instant::now() + std::time::Duration::from_secs(5),
        );
        drop(conn);
        eng.join().unwrap();
        got
    }

    #[test]
    fn attach_takes_the_region_and_pipes_with_scm_rights() {
        let att = attach_with(|file| {
            let (_, engine_tx) = rustix::pipe::pipe().unwrap();
            let (vmm_rx, _) = rustix::pipe::pipe().unwrap();
            vec![OwnedFd::from(file), engine_tx, vmm_rx]
        })
        .unwrap();
        assert_eq!((att.generation, att.pipes.len()), (9, 1));
        assert!(att.durable);
    }

    #[test]
    fn attach_refuses_the_wrong_descriptor_count() {
        let e = attach_with(|file| vec![OwnedFd::from(file)]).err().unwrap();
        assert!(e.to_string().contains("descriptors"), "{e}");
    }

    #[test]
    fn attach_refuses_truncated_descriptors() {
        // More than any frame may carry, so the control data is cut.
        let e = attach_with(|file| {
            let file = OwnedFd::from(file);
            (0..40).map(|_| file.try_clone().unwrap()).collect()
        })
        .err()
        .unwrap();
        assert!(e.to_string().contains("truncated"), "{e}");
    }
}
