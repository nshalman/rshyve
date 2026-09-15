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
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use clap::Parser;

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
            let mig_sock =
                format!("/zones/{uuid}/root/tmp/vmm-migrate-data.sock");
            run_dest(&uuid, port, &mig_sock)
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
fn check_name(what: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':')
            }),
        "{what} {value:?} holds a character that is not allowed"
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
            .args(["-o", "StrictHostKeyChecking=no", "-o", "IdentitiesOnly=yes"])
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
fn zfs_send_recv(send_args: &[&str], ssh: &Ssh, dst: &str) -> anyhow::Result<()> {
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

/// Send one control command to the VMM and log its answer.
fn control_command(ctl: &Path, command: &serde_json::Value) -> anyhow::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(ctl)
        .with_context(|| format!("connect to {}", ctl.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{command}")?;
    let mut reply = String::new();
    // The command has been delivered once the write returns; a slow
    // or missing answer is worth a log line, not a failure.
    if BufReader::new(&stream).read_line(&mut reply).is_ok() {
        log(&format!("  VMM answered: {}", reply.trim_end()));
    }
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
    match ssh.remote(&["vmadm", "start", uuid]).status() {
        Ok(st) if st.success() => log("  dest VM started"),
        Ok(st) => log(&format!(
            "  dest VM start exited {} (may be already running)",
            st.code().unwrap_or(-1)
        )),
        Err(e) => log(&format!("  dest VM start failed to run: {e}")),
    }
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
        let tcp = tokio::net::TcpStream::connect(&dest_addr).await
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
        let barrier_gz_path = format!("/zones/{uuid}/root/tmp/vmm-migrate-zfs-barrier.sock");
        let barrier_zone_path = "/tmp/vmm-migrate-zfs-barrier.sock";
        let _ = std::fs::remove_file(&barrier_gz_path);
        let barrier_listener = std::os::unix::net::UnixListener::bind(&barrier_gz_path)
            .context("bind ZFS barrier socket")?;
        barrier_listener.set_nonblocking(true).ok();
        log(&format!("  ZFS barrier at {barrier_gz_path}"));

        let trigger = serde_json::json!({
            "command": "migrate-source",
            "target_addr": zone_mig_path,
            "zfs_barrier": barrier_zone_path,
        });
        // Spawn background thread for post-pause ZFS barrier sync.
        // Blocks on accept() until VMM connects (vCPUs paused), then
        // does final ZFS incremental, then signals VMM to continue.
        let zfs_src = src_disk.to_string();
        let zfs_dst = dst_disk.to_string();
        let zfs_ssh = Ssh {
            key: ssh.key.clone(),
            host: ssh.host.clone(),
        };
        let zfs_skip = skip_zfs;
        let barrier_cleanup = barrier_gz_path.clone();
        let zfs_handle = std::thread::Builder::new()
            .name("zfs-barrier".into())
            .spawn(move || {
                // Switch to blocking mode for accept
                barrier_listener.set_nonblocking(false).ok();
                barrier_listener.set_nonblocking(false).ok();
                match barrier_listener.accept() {
                    Ok((mut conn, _)) => {
                        log("  ZFS barrier: VMM paused, starting final sync");
                        if !zfs_skip {
                            let snap = format!("{zfs_src}@vmm-mig-postpause");
                            let fin = format!("{zfs_src}@vmm-mig-final");
                            zfs_destroy_quiet(&snap);
                            if let Err(e) = zfs(&["snapshot", &snap]) {
                                log(&format!("  ZFS barrier: snapshot failed: {e}"));
                            } else if let Err(e) = zfs_send_recv(
                                &["-i", &fin, &snap],
                                &zfs_ssh,
                                &zfs_dst,
                            ) {
                                log(&format!("  ZFS barrier: send failed: {e}"));
                            } else {
                                log("  ZFS barrier: final incremental sent");
                            }
                        } else {
                            log("  ZFS barrier: skipped (--skip-zfs)");
                        }
                        // Signal VMM to continue
                        let _ = conn.write_all(&[1u8]);
                    }
                    Err(e) => log(&format!("  ZFS barrier: accept failed: {e}")),
                }
                let _ = std::fs::remove_file(&barrier_cleanup);
            })
            .ok();

        log(&format!("  triggering: {trigger}"));
        control_command(ctl, &trigger)?;
        log("  migration triggered, waiting for VMM...");

        let (unix, _) = unix_listener.accept().await
            .context("accept VMM")?;
        log("  VMM connected, bridging...");

        let (mut ur, mut uw) = tokio::io::split(unix);
        let (mut tr, mut tw) = tokio::io::split(tcp);

        let to_dest = tokio::io::copy(&mut ur, &mut tw);
        let from_dest = tokio::io::copy(&mut tr, &mut uw);

        tokio::select! {
            r = to_dest => log(&format!("  VMM→dest done: {:?}", r)),
            r = from_dest => log(&format!("  dest→VMM done: {:?}", r)),
        }

        let _ = std::fs::remove_file(mig_sock);
        if let Some(h) = zfs_handle { let _ = h.join(); }
        Ok::<(), anyhow::Error>(())
    })?;

    if !skip_zfs {
        zfs_destroy_quiet(&format!("{src_disk}@vmm-mig-final"));
        zfs_destroy_quiet(&format!("{src_disk}@vmm-mig-postpause"));
    }

    log("=== MIGRATION COMPLETE ===");
    Ok(())
}

fn run_dest(_uuid: &str, port: u16, mig_sock: &str) -> anyhow::Result<()> {
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
                let (s, peer) = tcp_listener.accept().await.unwrap();
                log(&format!("  source agent connected from {peer}"));
                s
            },
            async {
                let (s, _) = unix_listener.accept().await.unwrap();
                log("  dest VMM connected");
                s
            }
        );

        let (mut tr, mut tw) = tokio::io::split(tcp);
        let (mut ur, mut uw) = tokio::io::split(unix);

        let to_vmm = tokio::io::copy(&mut tr, &mut uw);
        let from_vmm = tokio::io::copy(&mut ur, &mut tw);

        tokio::select! {
            r = to_vmm => log(&format!("  src→VMM done: {:?}", r)),
            r = from_vmm => log(&format!("  VMM→src done: {:?}", r)),
        }

        log("=== DEST MIGRATION COMPLETE ===");
        Ok::<(), anyhow::Error>(())
    })
}

