// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An RDP server for one display, on IronRDP.
//!
//! The server runs on its own thread with its own single-threaded tokio
//! runtime, so nothing else in the VMM has to be async. It reads the
//! display through [`DisplaySource`] and delivers input through the
//! [`InputBroker`], as the VNC server does. It plays the guest's audio
//! over the rdpsnd channel, as the sink bound to an [`AudioBroker`].
//!
//! The server insists on TLS and on one fixed username and password,
//! whether it listens on TCP or on a 0600 Unix socket.

mod bridge;
mod display;
mod egfx;
mod input;
mod overlay;
mod progressive;
mod rlgr1;
mod sound;
mod trace;

#[cfg(test)]
mod testsupport;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use ironrdp_pdu::rdp::capability_sets::{
    server_codecs_capabilities, BitmapCodecs, Codec, CodecProperty, NsCodec,
};
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator,
    Credentials, TlsIdentityCtx,
};
use tokio::sync::Notify;
use vmm_devices::audio::AudioBroker;
use vmm_devices::display::DisplaySource;
use vmm_devices::InputBroker;

/// Where the RDP server listens.
pub use vmm_core::listen::Listen;

/// How to serve RDP.
pub struct RdpConfig {
    /// Where to listen.
    pub addr: Listen,
    /// TLS certificate and private key, PEM.
    pub cert: PathBuf,
    pub key: PathBuf,
    /// The one account a client must present.
    pub username: String,
    pub password: String,
}

/// The bitmap codecs the server offers: IronRDP's defaults, RemoteFX
/// among them, which Windows and FreeRDP clients take, and NSCodec,
/// the only one the macOS Windows App offers. The client picks.
fn codecs() -> anyhow::Result<BitmapCodecs> {
    let mut codecs = server_codecs_capabilities(&[])
        .map_err(|e| anyhow::anyhow!("RDP: the default codec list: {e}"))?;
    codecs.0.push(Codec {
        // The client assigns codec ids; a server entry carries 0.
        id: 0,
        property: CodecProperty::NsCodec(NsCodec {
            is_dynamic_fidelity_allowed: true,
            is_subsampling_allowed: true,
            color_loss_level: 3,
        }),
    });
    Ok(codecs)
}

/// The environment variable that sets how much of IronRDP's own
/// logging reaches the VMM's log: error, warn, info (the default),
/// debug or trace.
pub const TRACE_ENV: &str = "VMM_RDP_TRACE";

fn trace_level(log: &slog::Logger) -> tracing::Level {
    match std::env::var(TRACE_ENV) {
        Err(_) => tracing::Level::INFO,
        Ok(value) => value.parse().unwrap_or_else(|_| {
            slog::warn!(log, "RDP trace level not understood, using info";
                "variable" => TRACE_ENV, "value" => value);
            tracing::Level::INFO
        }),
    }
}

/// Accepts the one account and lifts the connection's pre-login limits.
/// Compares in constant time, unlike IronRDP's exact-match validator.
struct Admit {
    account: Credentials,
    budget: Arc<bridge::InboundBudget>,
}

#[async_trait::async_trait]
impl CredentialValidator for Admit {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        let user = same(&credentials.username, &self.account.username);
        let password = same(&credentials.password, &self.account.password);
        let domain = same(
            credentials.domain.as_deref().unwrap_or(""),
            self.account.domain.as_deref().unwrap_or(""),
        );
        if user & password & domain {
            self.budget.lift();
            Ok(CredentialDecision::Accept)
        } else {
            Ok(CredentialDecision::Reject)
        }
    }
}

/// Whether `a` and `b` are equal, in time that depends only on their
/// lengths.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    std::hint::black_box(diff) == 0
}

/// Serve the connections that reach `listener`, one at a time, each on a
/// server `new_server` builds for it. A fresh server per connection drops
/// whatever the last one left queued, as IronRDP's own accept loop does
/// between connections.
async fn serve(
    listener: bridge::Listener,
    mut new_server: impl FnMut(
        Arc<bridge::InboundBudget>,
    ) -> ironrdp_server::RdpServer,
    log: &slog::Logger,
) {
    let rt = tokio::runtime::Handle::current();
    let mut failing = false;
    loop {
        let conn = match listener.try_accept() {
            Ok(Some(conn)) => {
                failing = false;
                conn
            }
            Ok(None) => {
                tokio::time::sleep(ACCEPT_POLL).await;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if !failing {
                    slog::warn!(log, "RDP accept failed"; "error" => %e);
                    failing = true;
                }
                tokio::time::sleep(ACCEPT_POLL).await;
                continue;
            }
        };
        let budget = Arc::new(bridge::InboundBudget::new(PRE_LOGIN_BYTES));
        let bridged = bridge::bridge(conn, &rt, log, Arc::clone(&budget));
        let (stream, closer) = match bridged {
            Ok(bridged) => bridged,
            Err(e) => {
                slog::warn!(log, "RDP connection not bridged"; "error" => %e);
                continue;
            }
        };
        let mut server = new_server(Arc::clone(&budget));
        tokio::select! {
            result = server.run_connection(stream) => {
                if let Err(e) = result {
                    slog::warn!(log, "RDP connection ended"; "error" => format!("{e:#}"));
                }
            }
            why = watchdog(&budget, &closer, LIMITS) => {
                slog::info!(log, "RDP connection dropped"; "reason" => why);
            }
        }
        // Shuts the socket down, which ends the bridge threads.
        drop(closer);
    }
}

/// How often the listener is checked for a new connection.
const ACCEPT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a client may take to log in, and how long a write to it may
/// stall. Connections are served one at a time, so a client that does
/// neither would hold RDP for everyone.
#[derive(Debug, Clone, Copy)]
struct Limits {
    login: std::time::Duration,
    write_stall: std::time::Duration,
}

const LIMITS: Limits = Limits {
    login: std::time::Duration::from_secs(30),
    write_stall: std::time::Duration::from_secs(30),
};

/// Returns why a connection should be dropped, once it should be.
async fn watchdog(
    budget: &bridge::InboundBudget,
    closer: &bridge::Closer,
    limits: Limits,
) -> &'static str {
    let login_by = tokio::time::Instant::now() + limits.login;
    loop {
        tokio::time::sleep(WATCH_TICK).await;
        if !budget.lifted() && tokio::time::Instant::now() >= login_by {
            return "no login in time";
        }
        if closer
            .stalled_for()
            .is_some_and(|waited| waited >= limits.write_stall)
        {
            return "client stopped reading";
        }
    }
}

const WATCH_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// What a client may send before it logs in. Setup is a few tens of
/// kilobytes; a client's bitmap-cache lists can add a few hundred more.
const PRE_LOGIN_BYTES: u64 = 4 * 1024 * 1024;

/// An RDP server for one display, on its own thread.
pub struct RdpServer {
    stop: Arc<Notify>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
    log: slog::Logger,
}

impl RdpServer {
    /// Serve `source` on a thread called `name`, and bind this server
    /// as `audio`'s sink.
    ///
    /// # Errors
    ///
    /// Returns an error if the certificate or key cannot be loaded, the
    /// runtime cannot be built or the thread cannot start. Errors while
    /// listening or serving are logged.
    pub fn start(
        name: &str,
        source: Arc<dyn DisplaySource>,
        input: Arc<InputBroker>,
        audio: &AudioBroker,
        config: RdpConfig,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>> {
        let tls = TlsIdentityCtx::init_from_paths(&config.cert, &config.key)
            .context("RDP: loading the TLS certificate and key")?
            .make_acceptor()
            .context("RDP: building the TLS acceptor")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .context("RDP: building the runtime")?;

        let stop = Arc::new(Notify::new());
        let srv = Arc::new(Self {
            stop: Arc::clone(&stop),
            thread: Mutex::new(None),
            log: log.clone(),
        });

        let credentials = Credentials {
            username: config.username,
            password: config.password,
            domain: None,
        };
        let listen = config.addr;
        let listener = bridge::Listener::bind(&listen)?;
        // IronRDP's builder wants an address, which it binds only in a
        // run() this server never calls.
        let addr = match &listen {
            Listen::Tcp(addr) => *addr,
            Listen::Unix(_) => SocketAddr::from(([127, 0, 0, 1], 0)),
        };
        let codecs = codecs()?;
        let sound = sound::Sound::new();
        let sink = Arc::clone(&sound);
        let level = trace_level(&log);
        // Frames are encoded on a thread of their own: the server loop
        // sends the audio, and a frame encoded there stalled it.
        let egfx = egfx::Egfx::new(log.new(slog::o!()));
        let egfx_worker = egfx
            .start_worker(Arc::clone(&source))
            .context("RDP: spawning the EGFX worker")?;
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                // IronRDP's own reports (a rejected login, a failed
                // handshake) reach the VMM's log only through this. The
                // runtime runs every task on this thread.
                let _trace = tracing::subscriber::set_default(
                    trace::SlogSubscriber::new(log.new(slog::o!()), level),
                );
                // Stops when this thread does.
                let _egfx_worker = egfx_worker;
                let new_server = |budget| {
                    ironrdp_server::RdpServer::builder()
                        .with_addr(addr)
                        .with_tls(tls.clone())
                        .with_input_handler(input::Input::new(
                            Arc::clone(&input),
                            Arc::clone(&source),
                            log.clone(),
                        ))
                        .with_display_handler(display::Display {
                            source: Arc::clone(&source),
                            egfx: Some(Arc::clone(&egfx)),
                            log: log.clone(),
                        })
                        .with_credential_validator(Some(Arc::new(Admit {
                            account: credentials.clone(),
                            budget,
                        })))
                        .with_bitmap_codecs(codecs.clone())
                        .with_sound_factory(Some(Box::new(sound::Factory(
                            Arc::clone(&sound),
                        ))))
                        .with_gfx_factory(Some(Box::new(egfx::Factory(
                            Arc::clone(&egfx),
                        ))))
                        .build()
                };
                runtime.block_on(async {
                    slog::info!(log, "RDP server listening"; "addr" => %listen);
                    tokio::select! {
                        () = serve(listener, new_server, &log) => {}
                        () = stop.notified() => {}
                    }
                });
            })
            .context("RDP: spawning the server thread")?;
        *srv.thread.lock().expect("rdp: thread lock") = Some(handle);
        // Only once the server runs: a failed start leaves the VM's
        // audio with the sink it had.
        audio.set_sink(sink as _);
        Ok(srv)
    }

    /// Stop serving and wait for the server thread to leave. The listener
    /// closes with it, and the connection's socket is shut down; its two
    /// bridge threads exit on their own once their blocking calls return.
    pub fn shutdown(&self) {
        // A stored permit, so a stop that lands before the server waits
        // on it is not lost.
        self.stop.notify_one();
        let handle = self.thread.lock().expect("rdp: thread lock").take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                slog::warn!(self.log, "RDP server thread panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::testsupport::FakeDisplay;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn config(addr: SocketAddr) -> RdpConfig {
        RdpConfig {
            addr: Listen::Tcp(addr),
            cert: fixture("test-cert.pem"),
            key: fixture("test-key.pem"),
            username: "user".into(),
            password: "secret".into(),
        }
    }

    fn start(config: RdpConfig) -> anyhow::Result<Arc<RdpServer>> {
        RdpServer::start(
            "rdp-test",
            Arc::new(FakeDisplay::new(64, 48)),
            Arc::new(InputBroker::default()),
            &AudioBroker::default(),
            config,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    /// A port nothing listens on, for a server to take.
    fn free_addr() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .expect("an ephemeral port")
            .local_addr()
            .expect("its address")
    }

    fn connects(addr: SocketAddr) -> bool {
        TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
    }

    fn bridged(
        rt: &tokio::runtime::Runtime,
    ) -> (
        tempfile::TempDir,
        std::os::unix::net::UnixStream,
        tokio::io::DuplexStream,
        bridge::Closer,
    ) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("s");
        let listener =
            bridge::Listener::bind(&Listen::Unix(path.clone())).expect("bind");
        let client =
            std::os::unix::net::UnixStream::connect(&path).expect("connect");
        let conn = loop {
            if let Some(conn) = listener.try_accept().expect("accept") {
                break conn;
            }
        };
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let budget = Arc::new(bridge::InboundBudget::new(0));
        budget.lift();
        let (server, closer) =
            bridge::bridge(conn, rt.handle(), &log, budget).expect("bridge");
        (dir, client, server, closer)
    }

    fn watch_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
    }

    const SHORT: Limits = Limits {
        login: Duration::from_millis(300),
        write_stall: Duration::from_millis(300),
    };

    #[test]
    fn a_client_that_does_not_log_in_is_dropped() {
        let rt = watch_runtime();
        let (_dir, _client, _server, closer) = bridged(&rt);
        let budget = bridge::InboundBudget::new(1024);
        let why = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                watchdog(&budget, &closer, SHORT),
            )
            .await
        });
        assert_eq!(why, Ok("no login in time"));
    }

    #[test]
    fn a_logged_in_client_may_sit_idle() {
        let rt = watch_runtime();
        let (_dir, _client, _server, closer) = bridged(&rt);
        let budget = bridge::InboundBudget::new(0);
        budget.lift();
        let why = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                watchdog(&budget, &closer, SHORT),
            )
            .await
        });
        assert!(why.is_err(), "an idle session was dropped: {why:?}");
    }

    #[test]
    fn a_client_that_stops_reading_is_dropped() {
        use tokio::io::AsyncWriteExt;
        let rt = watch_runtime();
        let (_dir, _client, mut server, closer) = bridged(&rt);
        let budget = bridge::InboundBudget::new(0);
        budget.lift();
        let chunk = vec![0u8; 64 * 1024];
        let why = rt.block_on(async {
            let flood = async {
                loop {
                    if server.write_all(&chunk).await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    why = watchdog(&budget, &closer, SHORT) => why,
                    () = flood => unreachable!(),
                }
            })
            .await
        });
        assert_eq!(why, Ok("client stopped reading"));
    }

    #[test]
    fn credentials_must_match_exactly() {
        assert!(same("console", "console"));
        assert!(same("", ""));
        for (a, b) in [("console", "consolf"), ("console", "consol"), ("", "x")]
        {
            assert!(!same(a, b), "{a:?} == {b:?}");
        }
    }

    /// Windows and FreeRDP clients take RemoteFX; the macOS Windows App
    /// offers only NSCodec, and without it falls back to RLE bitmaps.
    #[test]
    fn the_server_offers_remotefx_and_nscodec() {
        let offered = codecs().expect("the codec list");
        let has = |f: fn(&CodecProperty) -> bool| {
            offered.0.iter().any(|c| f(&c.property))
        };
        assert!(has(|p| matches!(p, CodecProperty::RemoteFx(_))));
        assert!(has(|p| matches!(p, CodecProperty::NsCodec(_))));
    }

    /// A bad certificate has to stop the VM at start, not surface at
    /// the first client.
    #[test]
    fn a_missing_certificate_fails_at_start() {
        let mut cfg = config(free_addr());
        cfg.cert = fixture("no-such-cert.pem");
        assert!(start(cfg).is_err());
    }

    #[test]
    fn the_server_listens_until_shut_down() {
        let addr = free_addr();
        let server = start(config(addr)).expect("start");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !connects(addr) {
            assert!(Instant::now() < deadline, "never listened on {addr}");
            thread::sleep(Duration::from_millis(20));
        }

        let started = Instant::now();
        server.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown took {:?}",
            started.elapsed()
        );
        assert!(!connects(addr), "still listening after shutdown");
    }

    #[test]
    fn the_server_listens_on_a_private_unix_socket() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixStream;

        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("vm.rdp");
        let mut cfg = config(free_addr());
        cfg.addr = Listen::Unix(path.clone());
        let server = start(cfg).expect("start");

        let mode = std::fs::metadata(&path)
            .expect("the socket")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            UnixStream::connect(&path).is_ok(),
            "nothing bound at {}",
            path.display()
        );
        server.shutdown();
    }

    #[test]
    fn listen_names_its_transport() {
        assert_eq!(
            Listen::Unix("/tmp/vm.rdp".into()).to_string(),
            "unix:/tmp/vm.rdp"
        );
        assert_eq!(
            Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 3389))).to_string(),
            "127.0.0.1:3389"
        );
    }
}
