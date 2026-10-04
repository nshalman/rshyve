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
    pub(crate) slots: Slots,
    pub(crate) pipes: Vec<Pipes>,
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
            // 0 asks for a new generation. There is no reattach yet.
            generation: 0,
            queues: ask.queues,
            depth: ask.depth,
            buf_pages: ask.buf_pages,
            max_blocks: ask.max_blocks,
        }),
        deadline,
    )?;
    let (ok, mut fds) = match next(conn, deadline)? {
        (Message::AttachOk(ok), fds) => (ok, fds),
        (m, _) => return Err(refused("Attach", &m)),
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

pub(crate) fn check_ok(ask: &Ask, ok: &AttachOk) -> Result<Geometry, String> {
    if ok.layout_version != LAYOUT_VERSION {
        return Err(format!("region layout {}", ok.layout_version));
    }
    if ok.generation == 0 {
        return Err("generation 0".into());
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
    // them at any time is sound.
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
}
