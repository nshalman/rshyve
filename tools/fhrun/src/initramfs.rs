// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Build an in-memory cpio initramfs that contains everything the guest
//! needs to launch a single user process.
//!
//! Layout inside the archive:
//!
//! ```text
//! /init               the fhrun-init binary, renamed
//! /firehyve-spec.json the serialized GuestSpec
//! /app                the user binary, always at /app for the spec
//! /proc /sys /dev /tmp /run  empty mount points
//! /lib/...            optional extra files
//! ```

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};

use crate::cpio::{self, Entry, MODE_DIR, MODE_EXEC, MODE_FILE};
use crate::manifest::Manifest;

/// In-guest path for the user binary. `/init` execvs this directly
/// after parsing the spec.
pub const IN_GUEST_BIN: &str = "/app";

/// Build the initramfs as an in-memory blob.
///
/// We materialize file contents up front because cpio (newc) wants the
/// length in the header before the data. Keeping it in memory is fine
/// at this scale. A typical init plus a small server is 5 to 20 MiB.
pub fn build(manifest: &Manifest) -> Result<Vec<u8>> {
    let init_data = std::fs::read(&manifest.init)
        .with_context(|| format!("read init: {}", manifest.init.display()))?;
    let bin_data = std::fs::read(&manifest.bin)
        .with_context(|| format!("read bin: {}", manifest.bin.display()))?;

    let spec = manifest.to_guest_spec(IN_GUEST_BIN);
    let spec_json =
        serde_json::to_vec_pretty(&spec).context("serialize guest spec")?;

    // Read every extra file once, paired with its destination.
    // Preserve the source file's executable bit so binaries shipped
    // via `extra_files` (e.g. /usr/sbin/nft) come out runnable.
    let mut extras: Vec<(String, Vec<u8>, u32)> =
        Vec::with_capacity(manifest.extra_files.len());
    for (dest, src) in &manifest.extra_files {
        let data = std::fs::read(src).with_context(|| {
            format!("read extra_file {}: {}", dest, src.display())
        })?;
        let meta = std::fs::metadata(src).with_context(|| {
            format!("stat extra_file {}: {}", dest, src.display())
        })?;
        // POSIX mode bits: keep the low 9, force "regular file" type
        // (0o100_000), and AND with rwxr-xr-x to keep group/other
        // read+exec aligned.
        use std::os::unix::fs::PermissionsExt;
        let src_mode = meta.permissions().mode() & 0o777;
        let mode = if src_mode & 0o100 != 0 {
            // The source has an exec bit, so the copy is executable.
            0o100_000 | (src_mode | 0o755)
        } else {
            0o100_000 | (src_mode | 0o644)
        };
        extras.push((dest.clone(), data, mode));
    }

    // Collect every directory implied by the entries we want, plus the
    // standard mount points that init expects to exist.
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for d in ["proc", "sys", "dev", "tmp", "run", "etc"] {
        dirs.insert(d.to_string());
    }
    for (dest, _, _) in &extras {
        for parent in iter_parents(dest) {
            dirs.insert(parent);
        }
    }

    let mut buf: Vec<u8> = Vec::with_capacity(
        init_data.len() + bin_data.len() + spec_json.len() + 64 * 1024,
    );

    // Build the entry list. cpio newc requires parent directories to
    // appear before their children in the archive, so write the
    // directories first
    // (sorted by path, so shorter parents naturally come first).
    let mut entries: Vec<Entry<'_>> = Vec::new();
    for d in &dirs {
        entries.push(Entry {
            name: d.as_str(),
            mode: MODE_DIR,
            data: &[],
        });
    }
    entries.push(Entry {
        name: "init",
        mode: MODE_EXEC,
        data: &init_data,
    });
    entries.push(Entry {
        // Strip the leading slash: cpio names are relative.
        name: &IN_GUEST_BIN[1..],
        mode: MODE_EXEC,
        data: &bin_data,
    });
    entries.push(Entry {
        name: "firehyve-spec.json",
        mode: MODE_FILE,
        data: &spec_json,
    });
    for (dest, data, mode) in &extras {
        entries.push(Entry {
            name: dest.as_str(),
            mode: *mode,
            data,
        });
    }

    cpio::write_archive(&mut buf, &entries).context("write cpio archive")?;
    Ok(buf)
}

/// For "lib/x86_64-linux-gnu/libfoo.so", yield "lib", "lib/x86_64-linux-gnu".
fn iter_parents(path: &str) -> impl Iterator<Item = String> + '_ {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let count = parts.len().saturating_sub(1);
    (0..count).map(move |i| parts[..=i].join("/"))
}

/// Write the initramfs to a host path. Convenience wrapper.
pub fn build_to_path(manifest: &Manifest, out: &Path) -> Result<()> {
    let blob = build(manifest)?;
    std::fs::write(out, blob)
        .with_context(|| format!("write initramfs: {}", out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_of_nested_path() {
        let got: Vec<String> = iter_parents("lib/x86_64/libfoo.so").collect();
        assert_eq!(got, vec!["lib".to_string(), "lib/x86_64".to_string()]);
    }

    #[test]
    fn parents_of_top_level() {
        let got: Vec<String> = iter_parents("foo.txt").collect();
        assert!(got.is_empty());
    }
}
