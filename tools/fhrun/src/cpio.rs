// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal cpio "newc" archive writer.
//!
//! The Linux kernel reads initramfs as a (possibly gzip'd) cpio archive in
//! the SVR4 "newc" format. The format is dead simple: a 110-byte ASCII
//! header per entry, followed by the NUL-terminated name (4-byte aligned),
//! then file data (4-byte aligned). The archive ends with a sentinel entry
//! named `TRAILER!!!`.
//!
//! We emit the archive uncompressed. The kernel handles uncompressed cpio
//! initramfs natively, and skipping gzip avoids one host-side dep and one
//! guest-side decompress step on every cold start.

use std::io::Write;

/// Mode bits for a regular executable file (0100755).
pub const MODE_EXEC: u32 = 0o100_755;
/// Mode bits for a regular non-exec file (0100644).
pub const MODE_FILE: u32 = 0o100_644;
/// Mode bits for a directory (040755).
pub const MODE_DIR: u32 = 0o040_755;
/// Mode bits for a symlink (0120777). Kept here for future entries
/// even though no current call site uses it.
#[allow(dead_code)]
pub const MODE_SYMLINK: u32 = 0o120_777;

/// One archive entry to be written.
pub struct Entry<'a> {
    /// Absolute path inside the archive, without leading slash
    /// (e.g., "init", "bin/app", "etc/firehyve-spec.json").
    pub name: &'a str,
    /// File mode plus type bits. Use one of the `MODE_*` constants.
    pub mode: u32,
    /// File contents. For directories, leave empty. For symlinks, this
    /// is the link target as raw bytes.
    pub data: &'a [u8],
}

/// Write a cpio newc archive containing `entries` to `out`.
///
/// Entries are emitted in order. The caller is responsible for emitting
/// any parent directory entries before their children. The kernel's
/// initramfs unpacker walks the archive linearly and creates files in
/// the order it sees them.
pub fn write_archive<W: Write>(
    out: &mut W,
    entries: &[Entry<'_>],
) -> std::io::Result<()> {
    // Each regular file/dir/symlink gets a unique inode. The kernel
    // doesn't strictly require uniqueness for initramfs (it deduplicates
    // hardlinks by ino+dev pair, which we don't use), but giving each
    // entry a fresh ino keeps `cpio -t` and friends happy if anyone
    // inspects the archive.
    let mut ino: u32 = 1;
    for e in entries {
        write_entry(out, e, ino)?;
        ino = ino.wrapping_add(1);
    }
    write_trailer(out)?;
    Ok(())
}

fn write_entry<W: Write>(
    out: &mut W,
    e: &Entry<'_>,
    ino: u32,
) -> std::io::Result<()> {
    let name_bytes = e.name.as_bytes();
    // namesize includes the trailing NUL.
    let namesize = name_bytes.len() + 1;
    let filesize = e.data.len();

    // Header is 6 bytes of magic plus 13 fields of 8 hex chars,
    // 110 bytes in total.
    out.write_all(b"070701")?;
    write_hex8(out, ino)?;
    write_hex8(out, e.mode)?;
    write_hex8(out, 0)?; // uid
    write_hex8(out, 0)?; // gid
    write_hex8(out, 1)?; // nlink
    write_hex8(out, 0)?; // mtime
    write_hex8(out, filesize as u32)?;
    write_hex8(out, 0)?; // devmajor
    write_hex8(out, 0)?; // devminor
    write_hex8(out, 0)?; // rdevmajor
    write_hex8(out, 0)?; // rdevminor
    write_hex8(out, namesize as u32)?;
    write_hex8(out, 0)?; // check (unused for newc)

    // Name + NUL, then padding so total (header + name) is 4-byte aligned.
    out.write_all(name_bytes)?;
    out.write_all(&[0u8])?;
    let header_plus_name = 110 + namesize;
    pad4(out, header_plus_name)?;

    // Data + padding to 4-byte alignment.
    if filesize > 0 {
        out.write_all(e.data)?;
        pad4(out, filesize)?;
    }
    Ok(())
}

fn write_trailer<W: Write>(out: &mut W) -> std::io::Result<()> {
    let trailer = Entry {
        name: "TRAILER!!!",
        mode: 0,
        data: &[],
    };
    // Trailer always has ino 0; some unpackers care.
    write_entry(out, &trailer, 0)?;
    Ok(())
}

fn write_hex8<W: Write>(out: &mut W, v: u32) -> std::io::Result<()> {
    let s = format!("{v:08X}");
    out.write_all(s.as_bytes())
}

fn pad4<W: Write>(out: &mut W, written: usize) -> std::io::Result<()> {
    let rem = written & 3;
    if rem != 0 {
        let pad = [0u8; 4];
        out.write_all(&pad[..(4 - rem)])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_archive_has_only_trailer() {
        let mut buf = Vec::new();
        write_archive(&mut buf, &[]).unwrap();
        // 110-byte header + 11-byte name ("TRAILER!!!" + NUL) padded
        // to 4 = 124 bytes.
        assert_eq!(buf.len(), 124);
        assert_eq!(&buf[0..6], b"070701");
        // Locate the name immediately after the 110-byte header.
        assert_eq!(&buf[110..120], b"TRAILER!!!");
    }

    #[test]
    fn single_file_round_trip() {
        let mut buf = Vec::new();
        write_archive(
            &mut buf,
            &[Entry {
                name: "hello",
                mode: MODE_FILE,
                data: b"hi\n",
            }],
        )
        .unwrap();
        // First header magic.
        assert_eq!(&buf[0..6], b"070701");
        // The file name follows the header at offset 110.
        assert_eq!(&buf[110..115], b"hello");
        // Locate the data: header(110) + name(6 with NUL) padded to 4 = 116
        // (110+6 = 116, already aligned).
        assert_eq!(&buf[116..119], b"hi\n");
        // Trailer should end the archive.
        let trailer_pos = buf
            .windows(10)
            .position(|w| w == b"TRAILER!!!")
            .expect("trailer present");
        assert!(trailer_pos > 0);
    }

    #[test]
    fn padding_is_correct_for_odd_name() {
        // "ab" plus NUL is 3 bytes, so header(110) + 3 = 113, then
        // 3 bytes of pad to 116.
        let mut buf = Vec::new();
        write_archive(
            &mut buf,
            &[Entry {
                name: "ab",
                mode: MODE_FILE,
                data: b"x",
            }],
        )
        .unwrap();
        // Bytes 113..116 must be NUL pad.
        assert_eq!(&buf[113..116], &[0, 0, 0]);
        // Data byte sits at 116.
        assert_eq!(buf[116], b'x');
        // Then 3 bytes of NUL pad to align before the next entry.
        assert_eq!(&buf[117..120], &[0, 0, 0]);
    }
}
