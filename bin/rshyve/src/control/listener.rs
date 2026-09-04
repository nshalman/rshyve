// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unix socket listener: connection limits, framing and peer authorisation.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use slog::{error, info, warn, Logger};

use super::dispatch::dispatch;
use super::protocol::{Command, Response};
use super::{ControlError, VmController};
use crate::peercred::PeerCred;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the control socket listener thread.
///
/// Binds a Unix domain socket at `path`, removes any stale socket
/// file first. Returns the thread handle. The thread runs until the
/// process exits.
pub fn spawn_control_thread(
    path: &Path,
    ctrl: Arc<VmController>,
    log: Logger,
) -> io::Result<(thread::JoinHandle<()>, PathBuf)> {
    let listener = vmm_core::unixsock::bind_restricted(
        path,
        vmm_core::unixsock::SocketPolicy::default(),
    )?;

    let sock_path = path.to_path_buf();
    let cleanup_path = sock_path.clone();

    let handle = thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            let connection_log = log.clone();
            serve_connections(listener, log, move |stream| {
                if let Err(e) = handle_connection(&ctrl, &stream, &connection_log) {
                    warn!(connection_log, "control connection error"; "error" => %e);
                }
            });
        })?;

    Ok((handle, cleanup_path))
}

struct ConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn acquire_connection(active: &Arc<AtomicUsize>) -> Option<ConnectionGuard> {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < MAX_CONNECTIONS).then_some(current + 1)
        })
        .ok()?;
    Some(ConnectionGuard {
        active: Arc::clone(active),
    })
}

fn set_connection_deadlines(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(())
}

fn serve_connections<F>(listener: UnixListener, log: Logger, handler: F)
where
    F: Fn(UnixStream) + Send + Sync + 'static,
{
    let active = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                error!(log, "control accept error"; "error" => %e);
                break;
            }
        };

        if let Err(e) = set_connection_deadlines(&stream) {
            warn!(log, "failed to set control connection deadlines"; "error" => %e);
            continue;
        }

        let Some(connection_guard) = acquire_connection(&active) else {
            if let Err(e) =
                write_error_response(&stream, "too many connections")
            {
                warn!(log, "failed to reject excess control connection"; "error" => %e);
            }
            continue;
        };

        let handler = Arc::clone(&handler);
        let thread_log = log.clone();
        if let Err(e) = thread::Builder::new()
            .name("control-connection".into())
            .spawn(move || {
                let _guard = connection_guard;
                handler(stream);
            })
        {
            warn!(thread_log, "failed to spawn control connection thread"; "error" => %e);
        }
    }
}

/// illumos global zone id. A global-zone peer already has full authority
/// over this zone, so refusing it would protect nothing and would break
/// vmm-migrate-agent, which runs in the GZ and drives the control socket
/// of a VMM inside a zone.
const GLOBAL_ZONEID: i32 = 0;

fn peer_is_authorized(peer: &PeerCred, euid: u32, zoneid: Option<i32>) -> bool {
    if peer.uid != euid {
        return false;
    }
    match (peer.zoneid, zoneid) {
        (Some(peer_zone), Some(own_zone)) => {
            peer_zone == own_zone || peer_zone == GLOBAL_ZONEID
        }
        // Platforms without zones: the uid check is the whole policy.
        (None, None) => true,
        // One side reported a zone and the other did not. Fail closed.
        _ => false,
    }
}

fn request_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "control request exceeds maximum size",
    )
}

/// Read one newline-delimited request without allowing `buf` to exceed the cap.
/// Returns `false` only when EOF is reached before any request bytes are read.
fn read_request<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    buf.clear();

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n')
        {
            let consumed = newline + 1;
            if consumed > MAX_REQUEST_BYTES - buf.len() {
                return Err(request_too_large());
            }
            buf.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(true);
        }

        if available.len() > MAX_REQUEST_BYTES - buf.len() {
            return Err(request_too_large());
        }
        let consumed = available.len();
        buf.extend_from_slice(available);
        reader.consume(consumed);
    }
}

fn write_error_response(
    stream: &UnixStream,
    message: &str,
) -> Result<(), ControlError> {
    let response = Response::err(message);
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    let mut writer = stream;
    writer.write_all(&out)?;
    writer.flush()?;
    Ok(())
}

/// Handle a single client connection: read JSON lines, dispatch, respond.
fn handle_connection(
    ctrl: &Arc<VmController>,
    stream: &UnixStream,
    log: &Logger,
) -> Result<(), ControlError> {
    let peer = crate::peercred::peer_cred(stream)?;
    let euid = unsafe { libc::geteuid() };
    let zoneid = crate::peercred::process_zoneid()?;
    if !peer_is_authorized(&peer, euid, zoneid) {
        warn!(log, "rejected control connection"; "uid" => peer.uid);
        return Ok(());
    }

    let mut reader = BufReader::new(stream);
    let mut writer = stream;
    let mut request = Vec::with_capacity(256);

    loop {
        match read_request(&mut reader, &mut request) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                write_error_response(stream, "request too large")?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        if request.is_empty() {
            continue;
        }

        match serde_json::from_slice::<Command>(&request) {
            Ok(cmd) => {
                info!(log, "control command"; "command" => cmd.name());

                // Metrics commands return raw text, bypass Response serialization
                let raw = match &cmd {
                    Command::Metrics => {
                        let uptime = ctrl.start_time.elapsed().as_secs();
                        let vcpus: Vec<String> = ctrl
                            .vcpu_metrics
                            .iter()
                            .enumerate()
                            .map(|(i, v)| v.to_json(i as i32))
                            .collect();
                        Some(format!(
                            r#"{{"success":true,"uptime_secs":{},"vcpus":[{}]}}"#,
                            uptime,
                            vcpus.join(","),
                        ))
                    }
                    Command::MetricsPrometheus => {
                        let mut out = format!(
                            "# HELP vmm_uptime_seconds VMM uptime\n\
                             # TYPE vmm_uptime_seconds gauge\n\
                             vmm_uptime_seconds {}\n",
                            ctrl.start_time.elapsed().as_secs(),
                        );
                        for (i, v) in ctrl.vcpu_metrics.iter().enumerate() {
                            out.push_str(&v.to_prometheus(i as i32));
                        }
                        Some(out)
                    }
                    _ => None,
                };

                let mut out = if let Some(raw) = raw {
                    raw.into_bytes()
                } else {
                    let response = dispatch(ctrl, cmd);
                    serde_json::to_vec(&response)?
                };
                out.push(b'\n');
                writer.write_all(&out)?;
                writer.flush()?;
            }
            Err(e) => {
                warn!(log, "invalid control command";
                    "error" => %e,
                    "bytes" => request.len(),
                );
                let response = Response::err("invalid command");
                let mut out = serde_json::to_vec(&response)?;
                out.push(b'\n');
                writer.write_all(&out)?;
                writer.flush()?;
            }
        }
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    #[test]
    fn read_request_rejects_oversize_line() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(1024, Cursor::new(input));
        let mut request = Vec::new();

        let error = read_request(&mut reader, &mut request)
            .expect_err("oversize request must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(request.len() <= MAX_REQUEST_BYTES);
    }

    #[test]
    fn read_request_accepts_max_size_line() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES - 1];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(1024, Cursor::new(input));
        let mut request = Vec::new();

        let present = read_request(&mut reader, &mut request)
            .expect("maximum-size request must be accepted");

        assert!(present);
        assert_eq!(request.len(), MAX_REQUEST_BYTES - 1);
    }

    #[test]
    fn peer_authorization_checks_uid_and_zone() {
        let local = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: None,
        };
        let foreign_uid = PeerCred {
            uid: 43,
            gid: 7,
            zoneid: None,
        };
        let same_zone = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: Some(3),
        };
        let foreign_zone = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: Some(4),
        };

        assert!(peer_is_authorized(&local, 42, None));
        assert!(!peer_is_authorized(&foreign_uid, 42, None));
        assert!(peer_is_authorized(&same_zone, 42, Some(3)));
        assert!(!peer_is_authorized(&foreign_zone, 42, Some(3)));
    }

    #[test]
    fn global_zone_peer_is_authorized_against_a_zoned_vmm() {
        // vmm-migrate-agent runs in the GZ and drives the control socket
        // of a VMM inside a zone. Rejecting it would break migration.
        let gz_peer = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(GLOBAL_ZONEID),
        };
        assert!(peer_is_authorized(&gz_peer, 0, Some(7)));

        // A sibling zone is still refused.
        let sibling = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(8),
        };
        assert!(!peer_is_authorized(&sibling, 0, Some(7)));

        // A GZ peer with the wrong uid is still refused.
        let wrong_uid = PeerCred {
            uid: 1,
            gid: 0,
            zoneid: Some(GLOBAL_ZONEID),
        };
        assert!(!peer_is_authorized(&wrong_uid, 0, Some(7)));
    }

    #[test]
    fn mismatched_zone_reporting_fails_closed() {
        let zoned = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(0),
        };
        assert!(!peer_is_authorized(&zoned, 0, None));

        let unzoned = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: None,
        };
        assert!(!peer_is_authorized(&unzoned, 0, Some(0)));
    }

    #[test]
    fn second_client_is_served_while_first_is_idle() {
        static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);

        let id = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!(
            "vmm-control-concurrency-{}-{id}",
            std::process::id(),
        ));
        std::fs::create_dir(&temp_dir).expect("create control test directory");
        let path = temp_dir.join("control.sock");
        let listener =
            UnixListener::bind(&path).expect("bind control test socket");
        let log = Logger::root(slog::Discard, slog::o!());
        let first_connection = Arc::new(AtomicBool::new(true));
        let first_for_handler = Arc::clone(&first_connection);
        let (started_tx, started_rx) = mpsc::channel();

        thread::spawn(move || {
            serve_connections(listener, log, move |stream| {
                if first_for_handler.swap(false, Ordering::AcqRel) {
                    started_tx.send(()).expect("signal first connection start");
                }
                let mut reader = BufReader::new(&stream);
                let mut request = Vec::new();
                if matches!(read_request(&mut reader, &mut request), Ok(true)) {
                    let mut writer = &stream;
                    writer.write_all(b"ok\n").expect("write test response");
                }
            });
        });

        let _idle = UnixStream::connect(&path).expect("connect idle client");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idle client handler must start");

        let mut second =
            UnixStream::connect(&path).expect("connect second client");
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set test read timeout");
        second.write_all(b"status\n").expect("write second request");
        let mut response = String::new();
        BufReader::new(&second)
            .read_line(&mut response)
            .expect("read second response");

        assert_eq!(response, "ok\n");
        std::fs::remove_file(&path).expect("remove control test socket");
        std::fs::remove_dir(&temp_dir).expect("remove control test directory");
    }
}
