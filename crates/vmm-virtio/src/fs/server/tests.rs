// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire-format tests for the FUSE dispatcher.

use super::*;

const GPA: u64 = 0x20_0000;
const LEN: usize = 0x4000;
const REQ: u64 = GPA;
const RESP: u64 = GPA + 0x1000;

fn anon() -> PhysMap {
    PhysMap::new_anon(GPA, LEN).expect("anon region")
}

fn server() -> FuseServer {
    let pt = Passthrough::new(&std::env::temp_dir(), true).expect("open temp");
    FuseServer::new(Arc::new(pt))
}

fn seed(physmap: &PhysMap, gpa: u64, data: &[u8]) {
    physmap
        .lookup(gpa, data.len())
        .expect("mapped")
        .write_bytes(data)
        .expect("seed guest memory");
}

fn fetch(physmap: &PhysMap, gpa: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    physmap
        .lookup(gpa, len)
        .expect("mapped")
        .read_bytes(&mut out)
        .expect("read reply");
    out
}

/// Build a FUSE_INIT request: 40-byte header + 16-byte fuse_init_in.
fn init_request() -> Vec<u8> {
    let body = fuse::FuseInitIn {
        major: 7,
        minor: 33,
        max_readahead: 0x2_0000,
        flags: 0,
    };
    let hdr = fuse::FuseInHeader {
        len: (fuse::FUSE_IN_HEADER_SIZE + 16) as u32,
        opcode: fuse::FUSE_INIT,
        unique: 0x4242,
        nodeid: 0,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    let mut buf = bytes_of(&hdr);
    push_val(&mut buf, &body);
    buf
}

#[test]
fn init_over_a_chain_returns_a_success_header() {
    let physmap = anon();
    let req = init_request();
    seed(&physmap, REQ, &req);

    let bufs = vec![
        ChainBuf::Readable {
            addr: REQ,
            len: req.len() as u32,
        },
        ChainBuf::Writable {
            addr: RESP,
            len: 256,
        },
    ];

    let written = server().handle(&bufs, &physmap);
    assert_eq!(
        written as usize,
        fuse::FUSE_OUT_HEADER_SIZE + size_of::<fuse::FuseInitOut>()
    );

    let out = fetch(&physmap, RESP, written as usize);
    let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");
    assert_eq!(hdr.error, 0);
    assert_eq!(hdr.unique, 0x4242);
    assert_eq!(hdr.len, written);

    let init: fuse::FuseInitOut =
        read_at(&out[fuse::FUSE_OUT_HEADER_SIZE..]).expect("init out");
    assert_eq!(init.major, 7);
    assert_eq!(init.minor, 33);
    assert_eq!(init.max_write, INIT_MAX_WRITE);
}

// A guest can declare any length in the header. Anything past the
// message cap must be refused before a body of that size is allocated.
#[test]
fn oversized_declared_len_is_rejected_with_einval() {
    let physmap = anon();
    let mut req = init_request();
    let bogus = (MAX_MSG_SIZE as u32) + 1;
    req[..4].copy_from_slice(&bogus.to_le_bytes());
    seed(&physmap, REQ, &req);

    let bufs = vec![
        ChainBuf::Readable {
            addr: REQ,
            len: req.len() as u32,
        },
        ChainBuf::Writable {
            addr: RESP,
            len: 256,
        },
    ];

    let written = server().handle(&bufs, &physmap);
    assert_eq!(written as usize, fuse::FUSE_OUT_HEADER_SIZE);

    let out = fetch(&physmap, RESP, written as usize);
    let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");
    assert_eq!(hdr.error, -libc::EINVAL);
}

#[test]
fn a_chain_with_no_readable_segment_writes_nothing() {
    let physmap = anon();
    let bufs = vec![ChainBuf::Writable {
        addr: RESP,
        len: 256,
    }];
    assert_eq!(server().handle(&bufs, &physmap), 0);
}

// A writable window too small for the body still acks the header, so
// the guest sees a reply instead of a stalled request.
#[test]
fn a_short_writable_window_still_acks_the_header() {
    let physmap = anon();
    let req = init_request();
    seed(&physmap, REQ, &req);

    let bufs = vec![
        ChainBuf::Readable {
            addr: REQ,
            len: req.len() as u32,
        },
        ChainBuf::Writable {
            addr: RESP,
            len: fuse::FUSE_OUT_HEADER_SIZE as u32,
        },
    ];

    let written = server().handle(&bufs, &physmap);
    assert_eq!(written as usize, fuse::FUSE_OUT_HEADER_SIZE);
}

#[test]
fn extract_trims_nul() {
    let buf = b"foo\0bar\0";
    let c = extract_name(buf).unwrap();
    assert_eq!(c.as_bytes(), b"foo");
}

#[test]
fn extract_rejects_empty() {
    assert!(extract_name(&[]).is_none());
    assert!(extract_name(b"\0").is_none());
}

// -------------------------------------------------------------------
// Wire-level security properties
//
// `extract_name` does not validate: it stops at the first NUL and
// returns whatever bytes precede it. The guard against a guest walking
// out of the shared directory lives one layer down, in the passthrough
// backend. These tests drive whole requests through `handle` so the
// property is proved at the boundary the guest actually reaches, not
// at the backend call the dispatcher happens to make today.
// -------------------------------------------------------------------

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(
        dir.path(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("restore tempdir mode");
    dir
}

fn server_on(dir: &tempfile::TempDir, read_only: bool) -> FuseServer {
    let pt = Passthrough::new(dir.path(), read_only).expect("open dir");
    FuseServer::new(Arc::new(pt))
}

/// FUSE_MKDIR against the root node, carrying `name` verbatim.
fn mkdir_request(name: &[u8]) -> Vec<u8> {
    let body = fuse::FuseMkdirIn {
        mode: 0o755,
        umask: 0,
    };
    let body_len = size_of::<fuse::FuseMkdirIn>() + name.len() + 1;
    let hdr = fuse::FuseInHeader {
        len: (fuse::FUSE_IN_HEADER_SIZE + body_len) as u32,
        opcode: fuse::FUSE_MKDIR,
        unique: 0x5150,
        nodeid: fuse::FUSE_ROOT_ID,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    let mut buf = bytes_of(&hdr);
    push_val(&mut buf, &body);
    buf.extend_from_slice(name);
    buf.push(0);
    buf
}

/// Run one request through `handle` and return the reply's errno field.
fn reply_error(srv: &FuseServer, req: &[u8]) -> i32 {
    let physmap = anon();
    seed(&physmap, REQ, req);
    let bufs = vec![
        ChainBuf::Readable {
            addr: REQ,
            len: req.len() as u32,
        },
        ChainBuf::Writable {
            addr: RESP,
            len: 512,
        },
    ];
    let written = srv.handle(&bufs, &physmap);
    assert!(written as usize >= fuse::FUSE_OUT_HEADER_SIZE);
    let out = fetch(&physmap, RESP, written as usize);
    let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");
    hdr.error
}

// A writable share must still refuse to leave its root. This is the
// case that matters: read-only would mask the traversal behind EROFS.
#[test]
fn a_dotdot_component_is_refused_on_a_writable_share() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    assert_eq!(reply_error(&srv, &mkdir_request(b"..")), -libc::EINVAL);
    assert!(!dir.path().parent().unwrap().join("escaped").exists());
}

#[test]
fn an_embedded_slash_is_refused_on_a_writable_share() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    assert_eq!(
        reply_error(&srv, &mkdir_request(b"sub/escaped")),
        -libc::EINVAL
    );
    assert_eq!(reply_error(&srv, &mkdir_request(b"/abs")), -libc::EINVAL);
}

// The name check runs before the read-only check, so a traversal name
// reports EINVAL rather than EROFS even on a read-only share. Pinning
// that order keeps a future edit from hiding one guard behind the other.
#[test]
fn the_name_check_precedes_the_read_only_check() {
    let dir = private_dir();
    let srv = server_on(&dir, true);
    assert_eq!(reply_error(&srv, &mkdir_request(b"..")), -libc::EINVAL);
}

#[test]
fn a_mutating_op_is_refused_on_a_read_only_share() {
    let dir = private_dir();
    let srv = server_on(&dir, true);
    assert_eq!(reply_error(&srv, &mkdir_request(b"ordinary")), -libc::EROFS);
    assert!(!dir.path().join("ordinary").exists());
}

// The same request succeeds when the share is writable, so the two
// tests above prove enforcement rather than a broken request shape.
#[test]
fn the_same_mkdir_succeeds_on_a_writable_share() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    assert_eq!(reply_error(&srv, &mkdir_request(b"ordinary")), 0);
    assert!(dir.path().join("ordinary").is_dir());
}

/// FUSE_OPEN of `nodeid`, carrying the guest's open flags verbatim.
fn open_request(nodeid: u64, flags: u32) -> Vec<u8> {
    let body = fuse::FuseOpenIn { flags, unused: 0 };
    let hdr = fuse::FuseInHeader {
        len: (fuse::FUSE_IN_HEADER_SIZE + size_of::<fuse::FuseOpenIn>()) as u32,
        opcode: fuse::FUSE_OPEN,
        unique: 0x6060,
        nodeid,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    let mut buf = bytes_of(&hdr);
    push_val(&mut buf, &body);
    buf
}

/// Seed a file with content and return its nodeid, via a real LOOKUP.
fn seeded_file(dir: &tempfile::TempDir, srv: &FuseServer) -> u64 {
    std::fs::write(dir.path().join("payload"), b"KEEP THIS DATA")
        .expect("seed file");
    let pt = srv.passthrough();
    let name = std::ffi::CString::new("payload").expect("name");
    pt.lookup(fuse::FUSE_ROOT_ID, &name).expect("lookup").nodeid
}

// O_TRUNC destroys a file whatever the access mode claims: illumos
// honors O_RDONLY | O_TRUNC and truncates. A guest that sends it at a
// read-only share must be refused, or the share is not read-only.
#[test]
fn o_trunc_cannot_destroy_a_file_on_a_read_only_share() {
    let dir = private_dir();
    let srv = server_on(&dir, true);
    let nodeid = seeded_file(&dir, &srv);
    let flags = (libc::O_RDONLY | libc::O_TRUNC) as u32;

    assert_eq!(
        reply_error(&srv, &open_request(nodeid, flags)),
        -libc::EROFS
    );
    assert_eq!(
        std::fs::read(dir.path().join("payload")).expect("read back"),
        b"KEEP THIS DATA",
        "a read-only share truncated the host file"
    );
}

// A plain read open is still allowed, so the guard above rejects the
// truncation rather than every open.
//
// This and the next test reach `reopen_fd`, which reopens through
// /proc/self/fd. That path does not exist on a macOS dev host, so both
// run only on the deployment target.
#[cfg(target_os = "illumos")]
#[test]
fn a_plain_read_open_still_works_on_a_read_only_share() {
    let dir = private_dir();
    let srv = server_on(&dir, true);
    let nodeid = seeded_file(&dir, &srv);
    let flags = libc::O_RDONLY as u32;
    assert_eq!(reply_error(&srv, &open_request(nodeid, flags)), 0);
}

#[cfg(target_os = "illumos")]
#[test]
fn o_trunc_still_works_on_a_writable_share() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    let nodeid = seeded_file(&dir, &srv);
    let flags = (libc::O_WRONLY | libc::O_TRUNC) as u32;
    assert_eq!(reply_error(&srv, &open_request(nodeid, flags)), 0);
    assert!(std::fs::read(dir.path().join("payload"))
        .expect("read back")
        .is_empty());
}
