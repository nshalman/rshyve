// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control socket of a `--migrate-listen` import, before the VM
//! controller exists. It answers `status` and `migrate-status` only.
//!
//! The target agent reports the import as committed to the orchestrator
//! only on this answer, and the orchestrator makes this host the home of
//! the ring disks only on that report. With no answer here, the ring
//! attach waits for a home that never comes and the import fails.

use std::io::BufReader;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use slog::{info, warn, Logger};
use vmm_migrate::MigrationPhase;

use super::listener::{
    read_request, serve_connections, write_error_response, write_response,
    WRITE_TIMEOUT,
};
use super::protocol::{Command, Response};
use super::{ControlError, ControlListener, VmRunState};

const IMPORT_RUNNING: &str =
    "the migration import runs; only status and migrate-status are served \
     until the guest starts";

/// How often the socket path is checked.
const WATCH_EVERY: Duration = Duration::from_millis(200);

/// Serve the import's phase at `path` until the listener drops.
///
/// The final `zfs recv` of the zone root at the cutover gate replaces
/// the filesystem that holds `path`, which unlinks the socket while this
/// process still listens on it. So the path is watched, and bound again
/// when it no longer names the socket bound last.
pub fn spawn_import_status<P>(
    path: &Path,
    phase: P,
    log: Logger,
) -> std::io::Result<ControlListener>
where
    P: Fn() -> MigrationPhase + Send + Sync + 'static,
{
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        // A link here would make its target the home every later bind
        // is held to.
        if std::fs::symlink_metadata(parent)?.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is a symbolic link", parent.display()),
            ));
        }
    }
    let home = resolved_parent(path)?;
    let first = bind(path, &home)?;
    // Checked here, before the caller announces the path: a client that
    // connected first would take the probe's place.
    let first_bound = ours(path, &first).ok_or_else(|| {
        std::io::Error::other(format!(
            "{} does not reach the socket just bound",
            path.display()
        ))
    })?;
    let phase = Arc::new(phase);
    let shutdown = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&shutdown);
    let at = path.to_path_buf();
    let handle = thread::Builder::new().name("import-status".into()).spawn(
        move || {
            supervise(&at, &home, (first, first_bound), &stop, &phase, &log)
        },
    )?;
    Ok(ControlListener::from_parts(
        path.to_path_buf(),
        shutdown,
        handle,
    ))
}

/// Bind `path` for this process, whatever the received zone root left
/// there.
///
/// The path is this VM's control socket, so whatever is at it is
/// removed first: a file or a socket served from inside the zone must
/// not keep the status from binding. A parent that resolves anywhere
/// but `home`, where it resolved at the first bind (a symlink the
/// received zone root planted), is refused, so the bind cannot be sent
/// elsewhere.
fn bind(path: &Path, home: &Path) -> std::io::Result<UnixListener> {
    at_home(path, home)?;
    match std::fs::symlink_metadata(path) {
        // remove_dir_all does not follow links inside the tree.
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(_) => {}
    }
    let listener = vmm_core::unixsock::bind_restricted(
        path,
        vmm_core::unixsock::SocketPolicy::default(),
    )?;
    // Again after the bind: a parent swapped in between sent it
    // elsewhere.
    at_home(path, home)?;
    Ok(listener)
}

/// Refuse a parent that no longer resolves to `home`, or that another
/// user could put a socket in.
fn at_home(path: &Path, home: &Path) -> std::io::Result<()> {
    let real = resolved_parent(path)?;
    if real != home {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} now resolves to {}, not {}",
                path.display(),
                real.display(),
                home.display()
            ),
        ));
    }
    let meta = std::fs::metadata(&real)?;
    // SAFETY: geteuid takes no argument and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if !private_home(meta.uid(), meta.mode(), euid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a directory only this user controls",
                real.display()
            ),
        ));
    }
    Ok(())
}

/// Ours, and writable only by us or sticky. The zone's /tmp is the
/// sticky case: another user there cannot remove or rename our socket.
fn private_home(uid: u32, mode: u32, euid: u32) -> bool {
    uid == euid && (mode & 0o022 == 0 || mode & 0o1000 != 0)
}

/// Where the directory that holds `path` really is.
fn resolved_parent(path: &Path) -> std::io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::canonicalize(parent)
}

/// The inode `path` names, once a connect through it reached
/// `listener`. Read before and after the connect, so a swap in between
/// is seen and nothing is taken for ours that is not.
fn ours(path: &Path, listener: &UnixListener) -> Option<(u64, u64)> {
    let before = named(path)?;
    let probe = UnixStream::connect(path).ok()?;
    listener.set_nonblocking(true).ok()?;
    let reached = probe_queued(listener, std::process::id());
    listener.set_nonblocking(false).ok()?;
    drop(probe);
    (reached && named(path) == Some(before)).then_some(before)
}

/// The most connections one probe check takes off the queue.
const PROBE_ACCEPTS: usize = 64;

/// Whether a connection from process `me` is queued on `listener`.
///
/// A connection queued before the path was swapped can come first, and
/// its peer can relay whatever the probe sends, so only the peer's pid
/// tells the probe apart. Every other connection taken here is dropped.
fn probe_queued(listener: &UnixListener, me: u32) -> bool {
    for _ in 0..PROBE_ACCEPTS {
        let Ok((conn, _)) = listener.accept() else {
            return false;
        };
        if vmm_core::unixsock::peercred::peer_pid(&conn).ok() == Some(me) {
            return true;
        }
    }
    false
}

/// The file `path` names now, if any.
fn named(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

fn supervise<P>(
    path: &Path,
    home: &Path,
    first: (UnixListener, (u64, u64)),
    stop: &AtomicBool,
    phase: &Arc<P>,
    log: &Logger,
) where
    P: Fn() -> MigrationPhase + Send + Sync + 'static,
{
    let (first, first_bound) = first;
    let mut next = Some(first);
    let mut checked = Some(first_bound);
    while !stop.load(Ordering::Acquire) {
        let listener = match next.take() {
            Some(l) => l,
            None => match bind(path, home) {
                Ok(l) => {
                    info!(log, "import status socket bound again";
                        "path" => %path.display());
                    l
                }
                Err(e) => {
                    warn!(log, "import status socket not bound";
                        "path" => %path.display(), "error" => %e);
                    thread::sleep(WATCH_EVERY);
                    continue;
                }
            },
        };
        // Unlinked or swapped before it could be checked: bind again.
        let Some(bound) = checked.take().or_else(|| ours(path, &listener))
        else {
            continue;
        };
        let round = Arc::new(AtomicBool::new(false));
        let serving = Arc::clone(&round);
        let answer = Arc::clone(phase);
        let connection_log = log.clone();
        let accept_log = log.clone();
        let accept = thread::Builder::new()
            .name("import-status-accept".into())
            .spawn(move || {
                serve_connections(listener, &serving, accept_log, move |s| {
                    if let Err(e) = handle(&s, &*answer, &connection_log) {
                        warn!(connection_log, "import status connection error";
                            "error" => %e);
                    }
                });
            });
        let accept = match accept {
            Ok(t) => t,
            Err(e) => {
                warn!(log, "no import status thread"; "error" => %e);
                thread::sleep(WATCH_EVERY);
                continue;
            }
        };
        while !stop.load(Ordering::Acquire) && named(path) == Some(bound) {
            thread::sleep(WATCH_EVERY);
        }
        round.store(true, Ordering::Release);
        let _ = accept.join();
    }
}

fn handle<P>(
    stream: &UnixStream,
    phase: &P,
    log: &Logger,
) -> Result<(), ControlError>
where
    P: Fn() -> MigrationPhase,
{
    // The accept loop already applied the process peer policy.
    let mut reader = BufReader::new(stream);
    let mut request = Vec::with_capacity(256);
    loop {
        match read_request(&mut reader, &mut request) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                write_error_response(
                    stream,
                    "request too large",
                    WRITE_TIMEOUT,
                )?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        if request.is_empty() {
            continue;
        }
        let response = match serde_json::from_slice::<Command>(&request) {
            Ok(cmd) => answer(&cmd, phase()),
            Err(_) => Response::err("invalid command"),
        };
        let mut out = serde_json::to_vec(&response)?;
        out.push(b'\n');
        write_response(stream, &out, WRITE_TIMEOUT)?;
    }
}

/// `import-committed` once the source committed: the guest is here and
/// waits for its ring disks. Before that, `migrating`.
fn answer(cmd: &Command, phase: MigrationPhase) -> Response {
    match cmd {
        Command::Status | Command::MigrateStatus => {
            let mut r = Response::ok();
            r.state = Some(if phase == MigrationPhase::Committed {
                VmRunState::ImportCommitted
            } else {
                VmRunState::Migrating
            });
            r.migrate_phase = Some(phase.to_string());
            r
        }
        _ => Response::err(IMPORT_RUNNING),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};

    fn json(r: &Response) -> serde_json::Value {
        serde_json::to_value(r).expect("response serializes")
    }

    #[test]
    fn the_state_is_import_committed_only_after_the_source_commit() {
        for phase in [
            MigrationPhase::Sync,
            MigrationPhase::RamPushPrePause,
            MigrationPhase::DeviceState,
            MigrationPhase::Finish,
        ] {
            let r = json(&answer(&Command::Status, phase));
            assert_eq!(r["state"], "migrating", "{phase}");
        }
        let r = json(&answer(&Command::Status, MigrationPhase::Committed));
        assert_eq!(r["success"], true);
        assert_eq!(r["state"], "import-committed");
        assert_eq!(r["migrate_phase"], "committed");
        let r =
            json(&answer(&Command::MigrateStatus, MigrationPhase::Committed));
        assert_eq!(r["state"], "import-committed");
    }

    /// The exact line the target agent reads; its tests hold the same.
    #[test]
    fn the_committed_answer_is_this_line() {
        let r = answer(&Command::Status, MigrationPhase::Committed);
        assert_eq!(
            serde_json::to_string(&r).expect("json"),
            r#"{"success":true,"state":"import-committed","migrate_phase":"committed"}"#
        );
    }

    #[test]
    fn nothing_but_status_runs_during_the_import() {
        for cmd in [Command::Stop, Command::MigrateResumeSource] {
            let r = json(&answer(&cmd, MigrationPhase::Committed));
            assert_eq!(r["success"], false);
            assert!(r.get("state").is_none());
        }
    }

    #[test]
    fn a_client_reads_the_phase_over_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rs.sock");
        let phase = Arc::new(std::sync::Mutex::new(MigrationPhase::Finish));
        let read = Arc::clone(&phase);
        let log = Logger::root(slog::Discard, slog::o!());
        let listener = spawn_import_status(
            &path,
            move || *read.lock().expect("phase"),
            log,
        )
        .expect("listener");
        let ask = || {
            let mut s = UnixStream::connect(&path).expect("connect");
            s.write_all(b"{\"command\":\"status\"}\n").expect("write");
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).expect("read");
            serde_json::from_str::<serde_json::Value>(&line).expect("json")
        };
        assert_eq!(ask()["state"], "migrating");
        *phase.lock().expect("phase") = MigrationPhase::Committed;
        assert_eq!(ask()["state"], "import-committed");
        // As the final zfs recv of the zone root does.
        std::fs::remove_file(&path).expect("unlink");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&path).is_err() {
            assert!(std::time::Instant::now() < deadline, "not bound again");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(ask()["state"], "import-committed");
        drop(listener);
    }

    #[test]
    fn a_file_left_at_the_path_does_not_block_the_bind() {
        // A received zone root can hold anything at the socket path.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rs.sock");
        std::fs::write(&path, b"planted").expect("plant");
        let home = resolved_parent(&path).expect("home");
        let listener = bind(&path, &home).expect("bound over the file");
        assert!(ours(&path, &listener).is_some());
    }

    #[test]
    fn a_parent_swapped_for_a_link_is_refused() {
        // The zone root recv replaced the directory with a link.
        let dir = tempfile::tempdir().expect("tempdir");
        let sockdir = dir.path().join("run");
        std::fs::create_dir(&sockdir).expect("dir");
        let path = sockdir.join("rs.sock");
        let home = resolved_parent(&path).expect("home");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("dir");
        std::fs::remove_dir(&sockdir).expect("rmdir");
        std::os::unix::fs::symlink(&elsewhere, &sockdir).expect("link");
        bind(&path, &home).expect_err("through a link");
        assert!(!elsewhere.join("rs.sock").exists());
    }

    #[test]
    fn a_socket_another_listener_owns_is_not_taken_for_ours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rs.sock");
        let home = resolved_parent(&path).expect("home");
        let mine = bind(&path, &home).expect("bind");
        // Swapped for another listener's socket after the bind.
        std::fs::remove_file(&path).expect("unlink");
        let _theirs = UnixListener::bind(&path).expect("theirs");
        assert!(ours(&path, &mine).is_none());
    }

    #[test]
    fn a_connection_from_another_process_is_not_the_probe() {
        // A peer that queued first and relays the probe's bytes still
        // has its own pid.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rs.sock");
        let home = resolved_parent(&path).expect("home");
        let listener = bind(&path, &home).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let _queued = UnixStream::connect(&path).expect("connect");
        assert!(!probe_queued(&listener, std::process::id() + 1));
        let _probe = UnixStream::connect(&path).expect("connect");
        assert!(probe_queued(&listener, std::process::id()));
    }

    #[test]
    fn a_parent_that_is_a_link_is_refused_at_the_first_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("dir");
        let link = dir.path().join("run");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("link");
        let log = Logger::root(slog::Discard, slog::o!());
        let refused = spawn_import_status(
            &link.join("rs.sock"),
            || MigrationPhase::Sync,
            log,
        );
        assert!(refused.is_err());
        assert!(!elsewhere.join("rs.sock").exists());
    }

    #[test]
    fn only_a_home_this_user_controls_is_taken() {
        assert!(private_home(0, 0o40700, 0));
        assert!(private_home(0, 0o41777, 0), "a sticky /tmp");
        assert!(!private_home(1000, 0o40700, 0), "another owner");
        assert!(!private_home(0, 0o40777, 0), "world writable, not sticky");
        assert!(!private_home(0, 0o40770, 0), "group writable");
    }

    #[test]
    fn a_directory_left_at_the_path_does_not_block_the_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rs.sock");
        std::fs::create_dir_all(path.join("inner")).expect("plant");
        let home = resolved_parent(&path).expect("home");
        let listener = bind(&path, &home).expect("bound over the dir");
        assert!(ours(&path, &listener).is_some());
    }
}
