// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire-format tests for the FUSE dispatcher.

use super::*;
use fuse::linux_oflags as lx;

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

/// Run one request end to end.
///
/// The device splits these three steps around its guest-access guard.
/// These tests are about the wire format, so they compose them here.
fn handle(srv: &FuseServer, bufs: &[ChainBuf], physmap: &PhysMap) -> u32 {
    let call = srv.read_request(bufs, physmap);
    let reply = srv.run(&call);
    srv.write_reply(bufs, physmap, reply)
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

    let written = handle(&server(), &bufs, &physmap);
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

    let written = handle(&server(), &bufs, &physmap);
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
    assert_eq!(handle(&server(), &bufs, &physmap), 0);
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

    let written = handle(&server(), &bufs, &physmap);
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
    let written = handle(srv, &bufs, &physmap);
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

/// FUSE_OPEN of `nodeid`, carrying the guest's Linux open flags verbatim.
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
    let flags = lx::O_RDONLY | lx::O_TRUNC;

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
// /proc/self/fd. illumos and Linux both have that path; a macOS dev
// host does not. Keep Linux in the gate: it is the only platform CI
// runs on, so an illumos-only gate would run these nowhere.
#[cfg(any(target_os = "illumos", target_os = "linux"))]
#[test]
fn a_plain_read_open_still_works_on_a_read_only_share() {
    let dir = private_dir();
    let srv = server_on(&dir, true);
    let nodeid = seeded_file(&dir, &srv);
    let flags = lx::O_RDONLY;
    assert_eq!(reply_error(&srv, &open_request(nodeid, flags)), 0);
}

#[cfg(any(target_os = "illumos", target_os = "linux"))]
#[test]
fn o_trunc_still_works_on_a_writable_share() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    let nodeid = seeded_file(&dir, &srv);
    let flags = lx::O_WRONLY | lx::O_TRUNC;
    assert_eq!(reply_error(&srv, &open_request(nodeid, flags)), 0);
    assert!(std::fs::read(dir.path().join("payload"))
        .expect("read back")
        .is_empty());
}

/// FUSE_CREATE of `name` under the root, carrying the guest's Linux open
/// flags verbatim.
fn create_request(name: &[u8], flags: u32) -> Vec<u8> {
    let body = fuse::FuseCreateIn {
        flags,
        mode: 0o644,
        umask: 0,
        open_flags: 0,
    };
    let body_len = size_of::<fuse::FuseCreateIn>() + name.len() + 1;
    let hdr = fuse::FuseInHeader {
        len: (fuse::FUSE_IN_HEADER_SIZE + body_len) as u32,
        opcode: fuse::FUSE_CREATE,
        unique: 0x7070,
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

// The wire carries Linux numbers. On illumos `libc::O_EXCL` is 0x400,
// which is Linux's O_APPEND, so a server that masked with host
// constants let a second exclusive create of a lock file succeed.
#[test]
fn o_excl_on_the_wire_refuses_a_second_create() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    let flags = lx::O_WRONLY | lx::O_CREAT | lx::O_EXCL;
    assert_eq!(reply_error(&srv, &create_request(b"lock", flags)), 0);
    assert_eq!(
        reply_error(&srv, &create_request(b"lock", flags)),
        -libc::EEXIST,
        "the second exclusive create of the lock file succeeded"
    );
}

// Without O_EXCL the same create reopens the existing file, so the
// test above proves the flag was honored and not that create is
// broken.
#[test]
fn a_create_without_o_excl_reopens_an_existing_file() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    let flags = lx::O_WRONLY | lx::O_CREAT;
    assert_eq!(reply_error(&srv, &create_request(b"plain", flags)), 0);
    assert_eq!(reply_error(&srv, &create_request(b"plain", flags)), 0);
}

// O_NOFOLLOW and O_DIRECTORY are lookup flags the guest kernel has
// already acted on. They must pass the translator rather than read as
// unknown bits, or every `open(O_NOFOLLOW)` in the guest fails.
#[test]
fn lookup_flags_on_the_wire_are_accepted_by_create() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    let flags = lx::O_RDWR | lx::O_CREAT | lx::O_NOFOLLOW | lx::O_LARGEFILE;
    assert_eq!(reply_error(&srv, &create_request(b"nf", flags)), 0);
    assert!(dir.path().join("nf").is_file());
}

// A bit this server has not translated must not reach `openat` with
// whatever the host reads it as.
#[test]
fn an_unknown_open_flag_on_the_wire_is_refused() {
    let dir = private_dir();
    let srv = server_on(&dir, false);
    let o_path = 0o10000000;
    assert_eq!(
        reply_error(&srv, &create_request(b"odd", lx::O_CREAT | o_path)),
        -libc::EINVAL
    );
    assert!(!dir.path().join("odd").exists());
}

#[cfg(any(target_os = "illumos", target_os = "linux"))]
#[test]
fn o_directory_on_the_wire_is_accepted_by_open() {
    let dir = private_dir();
    let srv = server_on(&dir, true);
    let nodeid = seeded_file(&dir, &srv);
    let flags = lx::O_RDONLY | lx::O_DIRECTORY | lx::O_NOFOLLOW;
    assert_eq!(reply_error(&srv, &open_request(nodeid, flags)), 0);
}

// -------------------------------------------------------------------
// Directory listings over the wire
// -------------------------------------------------------------------

/// Run one request through `handle` and return the whole reply.
fn reply_bytes(srv: &FuseServer, req: &[u8]) -> Vec<u8> {
    let physmap = anon();
    seed(&physmap, REQ, req);
    let bufs = vec![
        ChainBuf::Readable {
            addr: REQ,
            len: req.len() as u32,
        },
        ChainBuf::Writable {
            addr: RESP,
            len: 0x2000,
        },
    ];
    let written = handle(srv, &bufs, &physmap);
    assert!(written as usize >= fuse::FUSE_OUT_HEADER_SIZE);
    fetch(&physmap, RESP, written as usize)
}

fn header_only(
    opcode: u32,
    nodeid: u64,
    body_len: usize,
) -> fuse::FuseInHeader {
    fuse::FuseInHeader {
        len: (fuse::FUSE_IN_HEADER_SIZE + body_len) as u32,
        opcode,
        unique: 0x8080,
        nodeid,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    }
}

/// FUSE_OPENDIR of the root, returning the handle the reply carries.
fn opendir_root(srv: &FuseServer) -> u64 {
    let req = bytes_of(&header_only(fuse::FUSE_OPENDIR, fuse::FUSE_ROOT_ID, 0));
    let out = reply_bytes(srv, &req);
    let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");
    assert_eq!(hdr.error, 0, "OPENDIR failed");
    let open: fuse::FuseOpenOut =
        read_at(&out[fuse::FUSE_OUT_HEADER_SIZE..]).expect("open out");
    open.fh
}

/// One READDIRPLUS entry as the guest parses it.
#[derive(Debug, PartialEq, Eq)]
struct PlusEntry {
    name: Vec<u8>,
    nodeid: u64,
}

/// FUSE_READDIRPLUS of `fh` from offset 0, parsed the way
/// `fs/fuse/readdir.c` walks the buffer. The error field is returned
/// with the entries so a refused listing is visible as such.
fn readdirplus(srv: &FuseServer, fh: u64) -> (i32, Vec<PlusEntry>) {
    let body = fuse::FuseReadIn {
        fh,
        offset: 0,
        size: 4096,
        read_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    };
    let mut req = bytes_of(&header_only(
        fuse::FUSE_READDIRPLUS,
        fuse::FUSE_ROOT_ID,
        size_of::<fuse::FuseReadIn>(),
    ));
    push_val(&mut req, &body);
    let out = reply_bytes(srv, &req);
    let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");

    let mut entries = Vec::new();
    let mut at = fuse::FUSE_OUT_HEADER_SIZE;
    while at < out.len() {
        let entry: fuse::FuseEntryOut = read_at(&out[at..]).expect("entry");
        at += size_of::<fuse::FuseEntryOut>();
        let dirent: fuse::FuseDirent = read_at(&out[at..]).expect("dirent");
        let namelen = dirent.namelen as usize;
        let name = out[at + fuse::FUSE_DIRENT_SIZE..][..namelen].to_vec();
        at += fuse::dirent_size(namelen);
        entries.push(PlusEntry {
            name,
            nodeid: entry.nodeid,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    (hdr.error, entries)
}

fn mkfifo(path: &std::path::Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .expect("path");
    // SAFETY: `c` owns its NUL-terminated bytes for the whole call,
    // which only reads them.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
}

// The server advertises READDIRPLUS, so Linux lists a directory with
// it. An entry the passthrough will not serve, here a FIFO, used to
// fail the whole listing with ENOTSUP. It has to go out with nodeid 0
// instead: Linux reads that as "look this one up yourself".
#[test]
fn readdirplus_lists_past_an_entry_the_passthrough_refuses() {
    let dir = private_dir();
    std::fs::write(dir.path().join("a"), b"x").expect("seed a");
    mkfifo(&dir.path().join("fifo"));
    std::fs::write(dir.path().join("z"), b"x").expect("seed z");
    let srv = server_on(&dir, true);

    let fh = opendir_root(&srv);
    let (error, entries) = readdirplus(&srv, fh);

    assert_eq!(error, 0, "the FIFO took the whole listing down");
    let names: Vec<&[u8]> = entries.iter().map(|e| e.name.as_slice()).collect();
    assert_eq!(names, [b"a".as_slice(), b"fifo", b"z"]);
    assert_eq!(entries[1].nodeid, 0, "the FIFO was given a nodeid");
    assert_ne!(entries[0].nodeid, 0, "a regular file lost its nodeid");
    assert_ne!(entries[2].nodeid, 0, "the entry after the FIFO was lost");
}

// A DAX window would hand the guest a mapping of host files that
// outlives the driver session: a device reset can take back the ring,
// but not a mapping the guest already holds. The export offers none,
// so the two mapping opcodes must stay undispatched and the INIT
// reply must keep advertising no map alignment.
//
// FUSE_SETUPMAPPING is 48 and FUSE_REMOVEMAPPING is 49 (FUSE 7.31).
#[test]
fn the_dax_mapping_opcodes_are_refused() {
    const FUSE_SETUPMAPPING: u32 = 48;
    const FUSE_REMOVEMAPPING: u32 = 49;

    for opcode in [FUSE_SETUPMAPPING, FUSE_REMOVEMAPPING] {
        let physmap = anon();
        let hdr = fuse::FuseInHeader {
            len: fuse::FUSE_IN_HEADER_SIZE as u32,
            opcode,
            unique: 0x51,
            nodeid: fuse::FUSE_ROOT_ID,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let req = bytes_of(&hdr);
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

        let written = handle(&server(), &bufs, &physmap);
        let out = fetch(&physmap, RESP, written as usize);
        let reply: fuse::FuseOutHeader = read_at(&out).expect("out header");
        // The guest is Linux, so the reply carries a Linux errno.
        assert_eq!(
            reply.error,
            -fuse::to_linux_errno(libc::ENOSYS),
            "opcode {opcode} reached a handler"
        );
    }

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
    let written = handle(&server(), &bufs, &physmap);
    let out = fetch(&physmap, RESP, written as usize);
    let init: fuse::FuseInitOut =
        read_at(&out[fuse::FUSE_OUT_HEADER_SIZE..]).expect("init out");
    assert_eq!(init.map_alignment, 0, "the export advertised a DAX window");
}
