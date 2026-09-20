// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vmm-migrate-agent — Global zone migration orchestrator.
//!
//! Runs in the GZ on both source and destination hosts. Handles ZFS
//! disk replication, bridges VMM Unix sockets to inter-host TCP, and
//! triggers the live RAM+state migration.
//!
//! # Flow
//!
//! 1. Dest agent: create Unix socket + TCP listener, wait
//! 2. Source agent: ZFS full send → incrementals → final incremental
//! 3. Source agent: start dest VM via SSH (VMM connects to dest Unix socket)
//! 4. Source agent: connect to dest agent TCP, trigger migrate-source
//! 5. VMM: pre-copy RAM → converge → pause → final RAM → device state → finish
//! 6. Agents bridge VMM↔Unix↔TCP↔Unix↔VMM throughout step 5
//! 7. Dest VMM resumes, guest continues running

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use clap::Parser;

/// The byte the VMM reads as "the destination disk is current".
const ZFS_BARRIER_OK: u8 = 0;
/// Anything else fails the migration; this is what the agent sends.
const ZFS_BARRIER_FAILED: u8 = 1;

/// How long the barrier waits for the VMM to reach its pause.
///
/// Longer than the pre-copy convergence takes, and short enough that a
/// wedged migration does not leave the agent running for ever.
const BARRIER_WAIT: Duration = Duration::from_secs(1800);
const BARRIER_POLL: Duration = Duration::from_millis(100);

#[derive(Parser, Debug)]
#[command(name = "vmm-migrate-agent")]
enum Cli {
    /// Source side: ZFS sync + start dest VM + bridge VMM to remote.
    Source {
        /// Source zone UUID
        #[arg(long)]
        uuid: String,

        /// Destination host IP
        #[arg(long)]
        dest_host: String,

        /// Destination agent TCP port
        #[arg(long, default_value = "4567")]
        dest_port: u16,

        /// Source ZFS disk dataset (default: zones/<uuid>/disk0)
        #[arg(long)]
        disk_dataset: Option<String>,

        /// Destination ZFS disk dataset (default: same as source)
        #[arg(long)]
        dest_dataset: Option<String>,

        /// Max ZFS incremental sync iterations
        #[arg(long, default_value = "3")]
        max_syncs: u32,

        /// SSH key for remote ZFS operations
        #[arg(long)]
        ssh_key: PathBuf,

        /// Path to VMM control socket (default: /zones/<uuid>/root/tmp/vmm-migrate.sock)
        #[arg(long)]
        control_socket: Option<PathBuf>,

        /// Skip ZFS sync (disk already pre-synced)
        #[arg(long)]
        skip_zfs: bool,
    },

    /// Destination side: create Unix socket, listen TCP, bridge.
    Dest {
        /// Destination zone UUID
        #[arg(long)]
        uuid: String,

        /// TCP port to listen on
        #[arg(long, default_value = "4567")]
        port: u16,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli {
        Cli::Source {
            uuid,
            dest_host,
            dest_port,
            disk_dataset,
            dest_dataset,
            max_syncs,
            ssh_key,
            control_socket,
            skip_zfs,
        } => {
            let src_disk =
                disk_dataset.unwrap_or_else(|| format!("zones/{uuid}/disk0"));
            let dst_disk = dest_dataset.unwrap_or_else(|| src_disk.clone());
            // These names reach a remote shell through ssh, so they are
            // held to the characters ZFS and DNS allow.
            check_name("uuid", &uuid)?;
            check_name("dest-host", &dest_host)?;
            check_name("disk-dataset", &src_disk)?;
            check_name("dest-dataset", &dst_disk)?;
            let ctl = control_socket.unwrap_or_else(|| {
                PathBuf::from(format!(
                    "/zones/{uuid}/root/tmp/vmm-migrate.sock"
                ))
            });
            let mig_sock =
                format!("/zones/{uuid}/root/tmp/vmm-migrate-data.sock");
            let ssh = Ssh {
                key: ssh_key,
                host: dest_host.clone(),
            };

            run_source(
                &uuid, &dest_host, dest_port, &src_disk, &dst_disk, max_syncs,
                &ssh, &ctl, &mig_sock, skip_zfs,
            )
        }
        Cli::Dest { uuid, port } => {
            // The uuid builds a path under the zone root, so it is
            // checked here as it is on the source side.
            check_name("uuid", &uuid)?;
            let mig_sock =
                format!("/zones/{uuid}/root/tmp/vmm-migrate-data.sock");
            run_dest(port, &mig_sock)
        }
    }
}

fn log(msg: &str) {
    eprintln!("[{}] {}", now(), msg);
}

fn now() -> String {
    chrono::Local::now().format("%T").to_string()
}

/// Accept only the characters a dataset, a UUID, or a host name is
/// made of. Anything else could reach the remote shell as syntax.
///
/// A leading `-` becomes an option to whatever the value is handed to
/// (`--disk-dataset -R` reaches `zfs` as a flag), and `..` climbs out
/// of the zone root the paths are built from.
fn check_name(what: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '_' | '-' | '.' | '/' | ':')
            }),
        "{what} {value:?} holds a character that is not allowed"
    );
    ensure!(
        !value.starts_with('-'),
        "{what} {value:?} would be read as an option"
    );
    ensure!(
        !value.split('/').any(|part| part == ".."),
        "{what} {value:?} traverses out of its directory"
    );
    Ok(())
}

/// The remote side of every ssh call.
struct Ssh {
    key: PathBuf,
    host: String,
}

impl Ssh {
    /// A command that runs `argv` on the remote host.
    fn remote(&self, argv: &[&str]) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-i")
            .arg(&self.key)
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "IdentitiesOnly=yes",
            ])
            .arg(format!("root@{}", self.host))
            .args(argv);
        cmd
    }
}

/// Run one local `zfs` command and report a non-zero exit.
fn zfs(argv: &[&str]) -> anyhow::Result<()> {
    let st = Command::new("zfs")
        .args(argv)
        .status()
        .with_context(|| format!("run zfs {argv:?}"))?;
    if !st.success() {
        bail!("exit {}: zfs {argv:?}", st.code().unwrap_or(-1));
    }
    Ok(())
}

/// Destroy a snapshot that may not exist. Nothing to report either way.
fn zfs_destroy_quiet(snapshot: &str) {
    let _ = Command::new("zfs")
        .args(["destroy", snapshot])
        .stderr(Stdio::null())
        .status();
}

/// The same on the remote host.
fn remote_zfs_destroy_quiet(ssh: &Ssh, snapshot: &str) {
    let _ = ssh
        .remote(&["zfs", "destroy", snapshot])
        .stderr(Stdio::null())
        .status();
}

/// Pipe `zfs send <send_args>` into `zfs recv -F <dst>` on the remote
/// host. Both ends must exit clean.
fn zfs_send_recv(
    send_args: &[&str],
    ssh: &Ssh,
    dst: &str,
) -> anyhow::Result<()> {
    let mut send = Command::new("zfs")
        .arg("send")
        .args(send_args)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("run zfs send {send_args:?}"))?;
    let stream = send.stdout.take().expect("stdout is piped");
    let recv = ssh
        .remote(&["zfs", "recv", "-F", dst])
        .stdin(stream)
        .status()
        .context("run remote zfs recv")?;
    let sent = send.wait().context("wait for zfs send")?;
    if !sent.success() {
        bail!("exit {}: zfs send {send_args:?}", sent.code().unwrap_or(-1));
    }
    if !recv.success() {
        bail!("exit {}: zfs recv -F {dst}", recv.code().unwrap_or(-1));
    }
    Ok(())
}

/// Send one control command to the VMM and take its verdict.
///
/// A refused command means the migration never starts. Ignoring the
/// answer left the agent waiting for a VMM connection that was never
/// coming.
fn control_command(
    ctl: &Path,
    command: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(ctl)
        .with_context(|| format!("connect to {}", ctl.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{command}")?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .context("the VMM did not answer the control command")?;
    log(&format!("  VMM answered: {}", reply.trim_end()));
    let parsed: serde_json::Value = serde_json::from_str(reply.trim_end())
        .context("the VMM's answer is not JSON")?;
    ensure!(
        parsed.get("success").and_then(|v| v.as_bool()) == Some(true),
        "the VMM refused the command: {}",
        parsed
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("no reason given"),
    );
    Ok(())
}

fn run_source(
    uuid: &str,
    dest_host: &str,
    dest_port: u16,
    src_disk: &str,
    dst_disk: &str,
    max_syncs: u32,
    ssh: &Ssh,
    ctl: &Path,
    mig_sock: &str,
    skip_zfs: bool,
) -> anyhow::Result<()> {
    // ── Phase 1-3: ZFS sync ─────────────────────────────────────
    // Dest VM must NOT be running during ZFS recv (zvol would be busy).
    // The source agent starts the dest VM after ZFS is complete.
    if !skip_zfs {
        log("Phase 1: ZFS snapshot + full send");
        let zone_ds = src_disk
            .rsplit_once('/')
            .map(|(p, _)| p)
            .unwrap_or(src_disk);
        let _ = zfs(&["set", "quota=none", zone_ds]);
        let _ = zfs(&["set", "refreservation=none", src_disk]);

        // Clean ALL old migration snapshots on both sides
        for snap in &[
            "vmm-mig-base",
            "vmm-mig-final",
            "vmm-mig-iter-1",
            "vmm-mig-iter-2",
            "vmm-mig-iter-3",
            "vmm-mig-live",
            "vmm-mig-fresh",
        ] {
            zfs_destroy_quiet(&format!("{src_disk}@{snap}"));
            remote_zfs_destroy_quiet(ssh, &format!("{dst_disk}@{snap}"));
        }

        let base = format!("{src_disk}@vmm-mig-base");
        zfs(&["snapshot", &base])?;
        zfs_send_recv(&[&base], ssh, dst_disk)?;
        log("  full send done");

        let mut last = "vmm-mig-base".to_string();
        for i in 1..=max_syncs {
            log(&format!("Phase 2: incremental sync {i}/{max_syncs}"));
            let snap = format!("vmm-mig-iter-{i}");
            let from = format!("{src_disk}@{last}");
            let to = format!("{src_disk}@{snap}");
            zfs(&["snapshot", &to])?;
            zfs_send_recv(&["-i", &from, &to], ssh, dst_disk)?;
            zfs_destroy_quiet(&from);
            last = snap;
            std::thread::sleep(Duration::from_secs(1));
        }

        log("Phase 3: final ZFS incremental");
        let from = format!("{src_disk}@{last}");
        let fin = format!("{src_disk}@vmm-mig-final");
        zfs(&["snapshot", &fin])?;
        zfs_send_recv(&["-i", &from, &fin], ssh, dst_disk)?;
        zfs_destroy_quiet(&from);
        log("  ZFS sync complete");
    } else {
        log("ZFS sync skipped (--skip-zfs)");
    }

    // ── Phase 3.5: Start dest VM ────────────────────────────────
    // Now that ZFS is synced, boot the dest VM. The VMM will see
    // --migrate-listen and connect to the dest agent's Unix socket.
    log("Phase 3.5: starting dest VM");
    // A dest VM that did not start never connects to the dest agent,
    // and the migration would wait on it for ever.
    let started = ssh
        .remote(&["vmadm", "start", uuid])
        .status()
        .context("run vmadm start on the destination")?;
    ensure!(
        started.success(),
        "vmadm start {uuid} exited {} on the destination",
        started.code().unwrap_or(-1),
    );
    log("  dest VM started");
    // Give the VMM a moment to boot and connect to the Unix socket
    std::thread::sleep(Duration::from_secs(3));

    // ── Phase 4: Bridge + trigger migration ─────────────────────
    log("Phase 4: setting up bridge and triggering migration");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let dest_addr = format!("{dest_host}:{dest_port}");
        log(&format!("  connecting to dest agent at {dest_addr}..."));
        let tcp = tokio::net::TcpStream::connect(&dest_addr)
            .await
            .context("connect to dest agent")?;
        log("  connected to dest agent");

        let _ = std::fs::remove_file(mig_sock);
        let unix_listener = tokio::net::UnixListener::bind(mig_sock)
            .context("bind migration socket")?;
        log(&format!("  listening on {mig_sock}"));

        let zone_mig_path = mig_sock
            .strip_prefix(&format!("/zones/{uuid}/root"))
            .unwrap_or("/tmp/vmm-migrate-data.sock");

        // Create ZFS barrier socket for post-pause sync
        let barrier_gz_path =
            format!("/zones/{uuid}/root/tmp/vmm-migrate-zfs-barrier.sock");
        let barrier_zone_path = "/tmp/vmm-migrate-zfs-barrier.sock";
        let _ = std::fs::remove_file(&barrier_gz_path);
        let barrier_listener =
            std::os::unix::net::UnixListener::bind(&barrier_gz_path)
                .context("bind ZFS barrier socket")?;
        log(&format!("  ZFS barrier at {barrier_gz_path}"));

        let trigger = serde_json::json!({
            "command": "migrate-source",
            "target_addr": zone_mig_path,
            "zfs_barrier": barrier_zone_path,
        });
        // The barrier thread waits for the VMM to connect, which is
        // its signal that the vCPUs are paused. It then runs the final
        // incremental and reports the verdict as one byte.
        let zfs_src = src_disk.to_string();
        let zfs_dst = dst_disk.to_string();
        let zfs_ssh = Ssh {
            key: ssh.key.clone(),
            host: ssh.host.clone(),
        };
        let zfs_skip = skip_zfs;
        let barrier_cleanup = barrier_gz_path.clone();
        let bridge_done = Arc::new(AtomicBool::new(false));
        let barrier_giveup = Arc::clone(&bridge_done);
        let zfs_handle = std::thread::Builder::new()
            .name("zfs-barrier".into())
            .spawn(move || {
                let outcome = run_zfs_barrier(
                    &barrier_listener,
                    &barrier_giveup,
                    &zfs_src,
                    &zfs_dst,
                    &zfs_ssh,
                    zfs_skip,
                );
                // The socket is removed here whatever happened, so a
                // migration that never reached the pause does not leave
                // it behind.
                let _ = std::fs::remove_file(&barrier_cleanup);
                outcome
            })
            .context("spawn the ZFS barrier thread")?;

        log(&format!("  triggering: {trigger}"));
        control_command(ctl, &trigger)?;
        log("  migration triggered, waiting for VMM...");

        let (unix, _) = unix_listener.accept().await.context("accept VMM")?;
        log("  VMM connected, bridging...");

        let (mut ur, mut uw) = tokio::io::split(unix);
        let (mut tr, mut tw) = tokio::io::split(tcp);

        let to_dest = tokio::io::copy(&mut ur, &mut tw);
        let from_dest = tokio::io::copy(&mut tr, &mut uw);

        let bridged = tokio::select! {
            r = to_dest => r.context("bridging VMM to dest"),
            r = from_dest => r.context("bridging dest to VMM"),
        };

        // Whatever the bridge did, the barrier is not going to be
        // reached again, so the thread stops waiting.
        bridge_done.store(true, Ordering::Release);
        let _ = std::fs::remove_file(mig_sock);
        let barrier = zfs_handle
            .join()
            .map_err(|_| anyhow::anyhow!("the ZFS barrier thread panicked"))?;
        bridged?;
        barrier?;
        Ok::<(), anyhow::Error>(())
    })?;

    if !skip_zfs {
        zfs_destroy_quiet(&format!("{src_disk}@vmm-mig-final"));
        zfs_destroy_quiet(&format!("{src_disk}@vmm-mig-postpause"));
    }

    log("=== MIGRATION COMPLETE ===");
    Ok(())
}

fn run_dest(port: u16, mig_sock: &str) -> anyhow::Result<()> {
    log(&format!("Destination agent on port {port}"));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let _ = std::fs::remove_file(mig_sock);
        let unix_listener = tokio::net::UnixListener::bind(mig_sock)
            .context("bind VMM socket")?;
        log(&format!("  created {mig_sock}"));

        let tcp_listener =
            tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
                .await
                .context("bind TCP")?;
        log(&format!("  listening on 0.0.0.0:{port}"));
        log("  waiting for source agent + dest VMM connections...");

        let (tcp, unix) = tokio::join!(
            async {
                let (s, peer) = tcp_listener
                    .accept()
                    .await
                    .context("accept the source agent")?;
                log(&format!("  source agent connected from {peer}"));
                Ok::<_, anyhow::Error>(s)
            },
            async {
                let (s, _) =
                    unix_listener.accept().await.context("accept the VMM")?;
                log("  dest VMM connected");
                Ok::<_, anyhow::Error>(s)
            }
        );
        let (tcp, unix) = (tcp?, unix?);

        let (mut tr, mut tw) = tokio::io::split(tcp);
        let (mut ur, mut uw) = tokio::io::split(unix);

        let to_vmm = tokio::io::copy(&mut tr, &mut uw);
        let from_vmm = tokio::io::copy(&mut ur, &mut tw);

        let bridged = tokio::select! {
            r = to_vmm => r.context("bridging source to VMM"),
            r = from_vmm => r.context("bridging VMM to source"),
        };
        let _ = std::fs::remove_file(mig_sock);
        bridged?;

        log("=== DEST MIGRATION COMPLETE ===");
        Ok::<(), anyhow::Error>(())
    })
}

/// Wait for the VMM to reach its pause, run the final incremental, and
/// report the verdict as one byte.
///
/// The VMM treats anything but [`ZFS_BARRIER_OK`], a timeout, or a
/// closed socket as a failed migration and resumes its guest, so a
/// failure here never lets the destination take a disk that is older
/// than the memory image.
fn run_zfs_barrier(
    listener: &std::os::unix::net::UnixListener,
    give_up: &AtomicBool,
    src_disk: &str,
    dst_disk: &str,
    ssh: &Ssh,
    skip_zfs: bool,
) -> anyhow::Result<()> {
    let Some(mut conn) = accept_before(listener, give_up)? else {
        // The migration never reached the pause. Nothing to sync, and
        // the VMM is not waiting on a byte.
        log("  ZFS barrier: the VMM never paused");
        return Ok(());
    };
    log("  ZFS barrier: VMM paused, starting final sync");
    let outcome = if skip_zfs {
        log("  ZFS barrier: skipped (--skip-zfs)");
        Ok(())
    } else {
        final_incremental(src_disk, dst_disk, ssh)
    };
    let status = match &outcome {
        Ok(()) => ZFS_BARRIER_OK,
        Err(e) => {
            log(&format!("  ZFS barrier: {e:#}"));
            ZFS_BARRIER_FAILED
        }
    };
    conn.write_all(&[status])
        .context("report the ZFS barrier verdict to the VMM")?;
    outcome
}

/// One incremental from the pre-pause snapshot to a fresh one.
fn final_incremental(
    src_disk: &str,
    dst_disk: &str,
    ssh: &Ssh,
) -> anyhow::Result<()> {
    let snap = format!("{src_disk}@vmm-mig-postpause");
    let fin = format!("{src_disk}@vmm-mig-final");
    zfs_destroy_quiet(&snap);
    zfs(&["snapshot", &snap])?;
    zfs_send_recv(&["-i", &fin, &snap], ssh, dst_disk)?;
    log("  ZFS barrier: final incremental sent");
    Ok(())
}

/// Take one connection, or give up when `give_up` is set or the
/// deadline passes.
///
/// A blocking `accept` here outlived every failed migration: the VMM
/// never connects when the migration is refused, and the thread stayed
/// in the call for ever.
fn accept_before(
    listener: &std::os::unix::net::UnixListener,
    give_up: &AtomicBool,
) -> anyhow::Result<Option<std::os::unix::net::UnixStream>> {
    let deadline = std::time::Instant::now() + BARRIER_WAIT;
    listener
        .set_nonblocking(true)
        .context("set the barrier listener non-blocking")?;
    while std::time::Instant::now() < deadline {
        if give_up.load(Ordering::Acquire) {
            return Ok(None);
        }
        match listener.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(false)
                    .context("set the barrier connection blocking")?;
                return Ok(Some(conn));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(BARRIER_POLL);
            }
            Err(e) => {
                return Err(e).context("accept on the ZFS barrier socket")
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_dataset_name_is_accepted() {
        check_name("disk-dataset", "zones/abc-123/disk0").expect("plain name");
        check_name("uuid", "3fe75ae1-8624-4764-80a3-2f4a332f3604")
            .expect("a uuid");
        check_name("dest-host", "10.0.0.1").expect("an address");
    }

    #[test]
    fn a_leading_dash_is_refused() {
        // `--disk-dataset -R` would reach zfs as a flag, locally and
        // over ssh.
        check_name("disk-dataset", "-R").expect_err("an option-looking value");
        check_name("uuid", "-x").expect_err("an option-looking value");
    }

    #[test]
    fn a_traversing_value_is_refused() {
        // The paths are built as /zones/<uuid>/root/tmp/..., and the
        // agent removes and binds files there.
        check_name("uuid", "../../x").expect_err("traversal");
        check_name("uuid", "..").expect_err("traversal");
        check_name("disk-dataset", "zones/../../etc").expect_err("traversal");
    }

    #[test]
    fn a_shell_metacharacter_is_refused() {
        for value in ["a;b", "a b", "a$b", "a`b`", "a|b", "a&b", ""] {
            check_name("uuid", value).unwrap_err();
        }
    }

    #[test]
    fn a_dotted_snapshot_name_is_still_accepted() {
        // `..` is refused as a whole path element, not as two dots.
        check_name("disk-dataset", "zones/a.b/disk0").expect("dots are fine");
    }
}
