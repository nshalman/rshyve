// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `--ring-config`: MantaBlock disks and migration settings from a file.
//!
//! The bhyve brand caps `bhyve-extra-opts` near 250 bytes, and one ring
//! disk with its `ring-shape=` takes about 240 of them. The agent puts
//! the ring disk specs in a file in the zone root instead, and only the
//! path goes on the command line.
//!
//! The file is JSON:
//!
//! ```json
//! {"version": 1,
//!  "disks": ["7,nvme,mantablock=/mb/<v>.sock,volume=<v>"],
//!  "migrate": {"ring_attach_timeout_ms": 120000,
//!              "allow_unproven_engine": false}}
//! ```

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use vmm_config::Cli;

use crate::bootorder::strip_bootindex;
use crate::pcidev::is_mantablock_nvme;

/// The only format version this binary reads.
pub const VERSION: u64 = 1;

/// Bounds the read, so a wrong path cannot fill memory.
const MAX_BYTES: u64 = 64 * 1024;

/// More disks than PCI slots the brand leaves free.
const MAX_DISKS: usize = 24;

/// The source stops waiting for the start soon after this, so a
/// longer attach could start a guest the source no longer counts on.
const MAX_ATTACH_TIMEOUT_MS: u64 =
    vmm_migrate::wire::MAX_DEST_ATTACH.as_millis() as u64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RingConfig {
    version: u64,
    #[serde(default)]
    disks: Vec<String>,
    #[serde(default)]
    migrate: MigrateOpts,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrateOpts {
    ring_attach_timeout_ms: Option<u64>,
    /// Taken only as `false`. The zone can write this file, so it must
    /// not turn on a lab flag; `--migrate-allow-unproven-engine` does.
    #[serde(default)]
    allow_unproven_engine: bool,
}

/// Read `cli.ring_config`, if given, and merge it into `cli`. The
/// checks on the result run with or without the file.
pub fn apply(cli: &mut Cli) -> anyhow::Result<()> {
    let from = match cli.ring_config.clone() {
        Some(path) => {
            let bytes = read(&path, effective_uid())?;
            let cfg = parse(&bytes)
                .with_context(|| format!("ring config {}", path.display()))?;
            merge(cli, cfg);
            format!("ring config {}", path.display())
        }
        None => "the command line".to_string(),
    };
    refuse_shared_volumes(&cli.pci_slot).with_context(|| from.clone())?;
    let ms = cli.migrate_ring_attach_timeout_ms;
    anyhow::ensure!(
        (1..=MAX_ATTACH_TIMEOUT_MS).contains(&ms),
        "{from}: the ring attach timeout {ms} ms is not in \
         1..={MAX_ATTACH_TIMEOUT_MS}"
    );
    Ok(())
}

/// Two disks on one volume would show the guest one disk twice and hand
/// the volume over twice in a migration. Volumes are compared as the
/// device parses them, so two spellings of one UUID are one volume.
fn refuse_shared_volumes(specs: &[String]) -> anyhow::Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for spec in specs {
        let (stripped, _) = strip_bootindex(spec)?;
        if !is_mantablock_nvme(&stripped) {
            continue;
        }
        let (_, _, config) = vmm_machine::devspec::split(&stripped)?;
        let vmm_machine::devspec::NvmeSpec::MantaBlock(opts) =
            vmm_machine::devspec::parse_nvme_config(config)?
        else {
            continue;
        };
        anyhow::ensure!(
            seen.insert(opts.volume),
            "volume {} has two ring disks",
            opts.volume
        );
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// The file must be a regular file of this process's user that no one
/// else can read or write: it names the sockets rshyve connects to.
fn read(path: &Path, owner: u32) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        path.is_absolute(),
        "--ring-config needs an absolute path, got {}",
        path.display()
    );
    // O_NONBLOCK so a FIFO planted at the path cannot hang the boot.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    anyhow::ensure!(
        meta.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    anyhow::ensure!(
        meta.uid() == owner,
        "{} is owned by uid {}, not {owner}",
        path.display(),
        meta.uid()
    );
    let mode = meta.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "{} has mode {mode:#o}; only its owner may have access",
        path.display()
    );
    anyhow::ensure!(
        meta.len() <= MAX_BYTES,
        "{} is {} bytes, over {MAX_BYTES}",
        path.display(),
        meta.len()
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "{} grew past {MAX_BYTES} bytes",
        path.display()
    );
    Ok(bytes)
}

fn parse(bytes: &[u8]) -> anyhow::Result<RingConfig> {
    // The version is read first, so a newer file is refused by its
    // version and not by the first field this binary does not know.
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("not JSON")?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    anyhow::ensure!(
        version == Some(VERSION),
        "version {} is not {VERSION}",
        value
            .get("version")
            .map_or_else(|| "(none)".to_string(), ToString::to_string)
    );
    let cfg: RingConfig = serde_json::from_value(value)?;
    debug_assert_eq!(cfg.version, VERSION);
    anyhow::ensure!(
        cfg.disks.len() <= MAX_DISKS,
        "{} disks, over {MAX_DISKS}",
        cfg.disks.len()
    );
    for spec in &cfg.disks {
        let (stripped, _) = strip_bootindex(spec)?;
        anyhow::ensure!(
            is_mantablock_nvme(&stripped),
            "{spec:?} is not a MantaBlock nvme disk; the file carries no \
             other device"
        );
    }
    if let Some(ms) = cfg.migrate.ring_attach_timeout_ms {
        anyhow::ensure!(
            (1..=MAX_ATTACH_TIMEOUT_MS).contains(&ms),
            "ring_attach_timeout_ms {ms} is not in 1..={MAX_ATTACH_TIMEOUT_MS}"
        );
    }
    anyhow::ensure!(
        !cfg.migrate.allow_unproven_engine,
        "allow_unproven_engine is a lab flag for the command line only \
         (--migrate-allow-unproven-engine)"
    );
    Ok(cfg)
}

fn merge(cli: &mut Cli, cfg: RingConfig) {
    let listening = cli.migrate_listen.is_some();
    for spec in cfg.disks {
        // A guest reset re-runs rshyve without --migrate-listen. If the
        // agent has not yet rewritten the file, the shapes the
        // destination needed are still in it, and a normal boot takes
        // its geometry from the engine.
        let spec = if listening {
            spec
        } else {
            without_ring_shape(&spec)
        };
        cli.pci_slot.push(spec);
    }
    if let Some(ms) = cfg.migrate.ring_attach_timeout_ms {
        cli.migrate_ring_attach_timeout_ms = ms;
    }
}

fn without_ring_shape(spec: &str) -> String {
    spec.split(',')
        .filter(|field| !field.starts_with("ring-shape="))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const VOL: &str = "11111111-2222-3333-4444-555555555555";

    fn disk(shape: bool) -> String {
        let mut s = format!("7,nvme,mantablock=/mb/{VOL}.sock,volume={VOL}");
        if shape {
            s.push_str(",ring-shape=1:256:4096:1:8:16:8:2:d");
        }
        s
    }

    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["rshyve"];
        argv.extend_from_slice(args);
        argv.push("vm");
        Cli::try_parse_named("rshyve", argv).expect("argv parses")
    }

    fn file(body: &[u8], mode: u32) -> tempfile::TempPath {
        let mut f = tempfile::NamedTempFile::new().expect("temp file");
        f.write_all(body).expect("write");
        let path = f.into_temp_path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("chmod");
        path
    }

    fn body(disks: &[String], migrate: &str) -> Vec<u8> {
        format!(
            r#"{{"version":1,"disks":{},"migrate":{migrate}}}"#,
            serde_json::to_string(disks).expect("json")
        )
        .into_bytes()
    }

    #[test]
    fn a_destination_keeps_the_shape_and_takes_the_migration_settings() {
        let path = file(
            &body(
                &[disk(true)],
                r#"{"ring_attach_timeout_ms":90000,"allow_unproven_engine":false}"#,
            ),
            0o600,
        );
        let mut c = cli(&[
            "--ring-config",
            path.to_str().expect("utf8"),
            "--migrate-listen",
            "/tmp/lmd.sock",
            "-s",
            "0,hostbridge",
        ]);
        apply(&mut c).expect("applies");
        assert_eq!(c.pci_slot, vec!["0,hostbridge".to_string(), disk(true)]);
        assert_eq!(c.migrate_ring_attach_timeout_ms, 90_000);
        assert!(!c.migrate_allow_unproven_engine);
    }

    #[test]
    fn the_file_cannot_turn_on_the_lab_flag() {
        // The zone can write the file; only the command line may.
        let err = parse(&body(&[], r#"{"allow_unproven_engine":true}"#))
            .expect_err("lab flag from the file");
        assert!(format!("{err:#}").contains("lab flag"), "{err:#}");
        let mut c = cli(&["--migrate-allow-unproven-engine"]);
        apply(&mut c).expect("the command line may");
        assert!(c.migrate_allow_unproven_engine);
    }

    #[test]
    fn an_attach_past_the_source_wait_is_refused() {
        let over = MAX_ATTACH_TIMEOUT_MS + 1;
        parse(&body(
            &[],
            &format!(r#"{{"ring_attach_timeout_ms":{over}}}"#),
        ))
        .expect_err("over the source's wait");
        let flag = over.to_string();
        let mut c = cli(&["--migrate-ring-attach-timeout-ms", &flag]);
        apply(&mut c).expect_err("the flag is checked too");
    }

    #[test]
    fn a_normal_boot_drops_the_shape() {
        let path = file(&body(&[disk(true)], "{}"), 0o600);
        let mut c = cli(&["--ring-config", path.to_str().expect("utf8")]);
        apply(&mut c).expect("applies");
        assert_eq!(c.pci_slot, vec![disk(false)]);
        assert!(!c.migrate_allow_unproven_engine);
    }

    #[test]
    fn no_flag_reads_nothing() {
        let mut c = cli(&["-s", "0,hostbridge"]);
        apply(&mut c).expect("applies");
        assert_eq!(c.pci_slot, vec!["0,hostbridge".to_string()]);
    }

    #[test]
    fn only_ring_disks_are_taken() {
        for bad in [
            "8,nvme,/dev/zvol/rdsk/zones/x/disk0",
            "8,passthru,/dev/ppt0",
            "8,virtio-blk,/etc/shadow",
        ] {
            let err = parse(&body(&[bad.to_string()], "{}")).expect_err(bad);
            assert!(format!("{err:#}").contains("not a MantaBlock"), "{err:#}");
        }
        let boot = format!(
            "0:4:0,nvme,bootindex=1,mantablock=/mb/{VOL}.sock,volume={VOL}"
        );
        parse(&body(&[boot], "{}")).expect("bootindex anywhere");
    }

    #[test]
    fn another_version_or_field_is_refused() {
        let err =
            parse(br#"{"version":2,"disks":[],"more":1}"#).expect_err("v2");
        assert!(format!("{err:#}").contains("version 2"), "{err:#}");
        parse(br#"{"disks":[]}"#).expect_err("no version");
        parse(br#"{"version":1,"disks":[],"extra":1}"#).expect_err("unknown");
        parse(br#"{"version":1,"migrate":{"ring_attach_timeout_ms":0}}"#)
            .expect_err("zero timeout");
        let many: Vec<String> = (0..=MAX_DISKS).map(|_| disk(false)).collect();
        parse(&body(&many, "{}")).expect_err("too many");
    }

    #[test]
    fn a_file_others_can_reach_is_refused() {
        let path = file(&body(&[disk(false)], "{}"), 0o644);
        let err = read(&path, effective_uid()).expect_err("0644");
        assert!(format!("{err:#}").contains("mode"), "{err:#}");
    }

    #[test]
    fn a_file_of_another_user_is_refused() {
        let path = file(&body(&[disk(false)], "{}"), 0o600);
        let other = effective_uid().wrapping_add(1);
        let err = read(&path, other).expect_err("owner");
        assert!(format!("{err:#}").contains("owned by"), "{err:#}");
    }

    #[test]
    fn a_symlink_or_a_directory_is_refused() {
        let real = file(&body(&[disk(false)], "{}"), 0o600);
        let dir = tempfile::tempdir().expect("dir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        read(&link, effective_uid()).expect_err("symlink");
        read(dir.path(), effective_uid()).expect_err("directory");
        read(Path::new("relative"), effective_uid()).expect_err("relative");
    }

    #[test]
    fn one_volume_cannot_back_two_disks() {
        let path = file(&body(&[disk(false)], "{}"), 0o600);
        let other_slot = disk(false).replacen("7,", "8,", 1);
        let mut c = cli(&[
            "--ring-config",
            path.to_str().expect("utf8"),
            "-s",
            &other_slot,
        ]);
        let err = apply(&mut c).expect_err("shared volume");
        assert!(format!("{err:#}").contains("two ring disks"), "{err:#}");
    }

    #[test]
    fn two_spellings_of_one_volume_are_one_volume() {
        // Checked with no file too: the command line alone can do it.
        let upper = format!(
            "8,nvme,mantablock=/mb/other.sock,volume={}",
            VOL.to_uppercase().replace('-', "")
        );
        let mut c = cli(&["-s", &disk(false), "-s", &upper]);
        let err = apply(&mut c).expect_err("one volume, two spellings");
        assert!(format!("{err:#}").contains("two ring disks"), "{err:#}");
    }

    #[test]
    fn an_oversized_file_is_refused() {
        let path = file(&vec![b' '; MAX_BYTES as usize + 1], 0o600);
        read(&path, effective_uid()).expect_err("size");
    }
}
