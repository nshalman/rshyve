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
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use slog::{info, warn, Logger};
use vmm_migrate::MigrationPhase;

use super::listener::{
    authorized, read_request, serve_connections, write_error_response,
    write_response, WRITE_TIMEOUT,
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
    let first = bind(path)?;
    let phase = Arc::new(phase);
    let shutdown = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&shutdown);
    let at = path.to_path_buf();
    let handle = thread::Builder::new()
        .name("import-status".into())
        .spawn(move || supervise(&at, first, &stop, &phase, &log))?;
    Ok(ControlListener::from_parts(
        path.to_path_buf(),
        shutdown,
        handle,
    ))
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    vmm_core::unixsock::bind_restricted(
        path,
        vmm_core::unixsock::SocketPolicy::default(),
    )
}

/// The file `path` names now, if any.
fn named(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

fn supervise<P>(
    path: &Path,
    first: UnixListener,
    stop: &AtomicBool,
    phase: &Arc<P>,
    log: &Logger,
) where
    P: Fn() -> MigrationPhase + Send + Sync + 'static,
{
    let mut next = Some(first);
    while !stop.load(Ordering::Acquire) {
        let listener = match next.take() {
            Some(l) => l,
            None => match bind(path) {
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
        // Unlinked again before it could be read: bind once more.
        let Some(bound) = named(path) else {
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
    if !authorized(stream, log)? {
        return Ok(());
    }
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
}
