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
use std::os::unix::net::UnixStream;
use std::path::Path;

use slog::{warn, Logger};
use vmm_migrate::MigrationPhase;

use super::listener::{
    authorized, read_request, spawn_listener, write_error_response,
    write_response, WRITE_TIMEOUT,
};
use super::protocol::{Command, Response};
use super::{ControlError, ControlListener, VmRunState};

const IMPORT_RUNNING: &str =
    "the migration import runs; only status and migrate-status are served \
     until the guest starts";

/// Serve the import's phase at `path` until the listener drops.
pub fn spawn_import_status<P>(
    path: &Path,
    phase: P,
    log: Logger,
) -> std::io::Result<ControlListener>
where
    P: Fn() -> MigrationPhase + Send + Sync + 'static,
{
    let connection_log = log.clone();
    spawn_listener(path, log, move |stream| {
        if let Err(e) = handle(&stream, &phase, &connection_log) {
            warn!(connection_log, "import status connection error";
                "error" => %e);
        }
    })
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
        let phase =
            std::sync::Arc::new(std::sync::Mutex::new(MigrationPhase::Finish));
        let read = std::sync::Arc::clone(&phase);
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
        drop(listener);
    }
}
