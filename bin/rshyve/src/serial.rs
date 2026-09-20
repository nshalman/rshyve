// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! COM1 and COM2: the UARTs, their host-side backends, and the mdata
//! agent that serves COM2 when argv names no backend for it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::sync::Arc;
use std::thread;

use anyhow::Context;
use slog::{info, warn, Logger};

use vmm_config::Cli;
use vmm_core::intr_pins::LegacyPIC;
use vmm_core::machine::Machine;
use vmm_devices::uart::backend::SerialBackend;
use vmm_devices::uart::lpc::{self, LpcUart};
use vmm_machine::find_lpc_device;

use crate::mdata;

/// Guest output taken off the UART on the vCPU thread and handed to a
/// consumer thread.
///
/// The TX sink runs inside the PIO handler with the UART lock held, so
/// it may do nothing that waits. It pushes into a bounded channel and
/// drops when the channel is full; the consumer blocks on the other
/// end, so an idle port costs no wakeups.
pub struct TxQueue {
    pub bytes: Receiver<u8>,
    dropped: Arc<AtomicU64>,
}

impl TxQueue {
    /// Install the sink on `uart`, buffering up to `capacity` bytes.
    pub fn install(uart: &LpcUart, capacity: usize) -> Self {
        let (tx, bytes) = mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        uart.set_tx_sink(Box::new(move |b: u8| match tx.try_send(b) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            // The consumer is gone; there is nobody to count for.
            Err(TrySendError::Disconnected(_)) => {}
        }));
        Self { bytes, dropped }
    }

    /// Bytes dropped since the last call.
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

/// Create and attach COM1/COM2 UARTs, set up serial I/O paths, and
/// optionally start the mdata agent on COM2.
pub fn setup_serial(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    cli: &Cli,
    log: &Logger,
) -> anyhow::Result<(Arc<LpcUart>, Arc<LpcUart>)> {
    let com1 =
        vmm_machine::attach_uart(machine, pic, lpc::COM1_BASE, lpc::COM1_IRQ);
    info!(log, "COM1 attached"; "port" => format!("{:#x}", lpc::COM1_BASE));
    let com2 =
        vmm_machine::attach_uart(machine, pic, lpc::COM2_BASE, lpc::COM2_IRQ);
    info!(log, "COM2 attached"; "port" => format!("{:#x}", lpc::COM2_BASE));

    // Parse COM port paths from -l flags
    let com1_path = find_lpc_device(&cli.lpc, "com1");
    let com2_path = find_lpc_device(&cli.lpc, "com2");

    // Set up COM1 serial I/O based on -l com1,<config>
    setup_serial_io(&com1, com1_path.as_deref(), "com1", log)?;

    // COM2 serves the backend the argv named, or the built-in mdata
    // agent when it named none.
    if let Some(ref path) = com2_path {
        setup_serial_io(&com2, Some(path.as_str()), "com2", log)?;
    } else {
        // Standalone mode: start built-in mdata agent using CLI flags.
        let mdata_store = Arc::new(mdata::build_store_from_zone_config(
            &cli.vm_name,
            cli.uuid.as_deref(),
            cli.mdata_nics.as_deref(),
            cli.mdata_resolvers.as_deref(),
            cli.mdata_ssh_keys.as_deref(),
            cli.mdata_root_pw.as_deref(),
        ));
        mdata::start_mdata_agent(com2.clone(), mdata_store, log.clone())
            .context("failed to start the mdata agent")?;
        info!(log, "mdata agent started on COM2");
    }

    Ok((com1, com2))
}

/// Set up serial I/O for a COM port.
///
/// Supports:
/// - `stdio` or absent → stdout/stdin
/// - `socket,/path` → Unix domain socket (host metadata agent connects)
/// - `/dev/zconsole` or any path → open as read/write device
///
/// The last two forms of backend live in `vmm_devices::uart::backend`,
/// so this binary and firehyve serve a zone console the same way.
fn setup_serial_io(
    uart: &Arc<LpcUart>,
    config: Option<&str>,
    name: &str,
    log: &Logger,
) -> anyhow::Result<()> {
    // Handle "socket,/path" — create a Unix domain socket listener
    // that the host-side metadata agent connects to.
    if let Some(path) = config.and_then(|c| c.strip_prefix("socket,")) {
        setup_serial_socket(uart, path, name, log)?;
        return Ok(());
    }

    let backend = match config {
        None | Some("stdio") => SerialBackend::Stdio,
        Some(path) => SerialBackend::Device(path),
    };
    // A console that cannot be opened is not worth failing a boot for:
    // the guest still runs, and the warning names the path.
    if let Err(e) = vmm_devices::uart::backend::attach(uart, backend, name, log)
    {
        warn!(log, "serial backend unusable"; "port" => name,
            "error" => %e);
    }
    Ok(())
}

/// Set up a Unix domain socket backend for a COM port.
///
/// Creates a listening socket at `path`, accepts one connection at a
/// time, and bridges it to the UART. The host-side metadata agent
/// connects to this socket to serve the SmartOS metadata protocol
/// (sdc:nics, SSH keys, etc.) to the guest via COM2.
fn setup_serial_socket(
    uart: &Arc<LpcUart>,
    path: &str,
    name: &str,
    log: &Logger,
) -> std::io::Result<()> {
    use std::io::{Read, Write};

    let listener = vmm_core::unixsock::bind_restricted(
        std::path::Path::new(path),
        vmm_core::unixsock::SocketPolicy::default(),
    )?;
    info!(log, "{name} socket listening"; "path" => path);

    // TX: buffer writes until a client connects. Use a shared
    // optional stream protected by a mutex.
    let tx_stream: Arc<
        std::sync::Mutex<Option<std::os::unix::net::UnixStream>>,
    > = Arc::new(std::sync::Mutex::new(None));
    let tx_clone = Arc::clone(&tx_stream);
    uart.set_tx_sink(Box::new(move |b: u8| {
        if let Ok(guard) = tx_clone.lock() {
            if let Some(ref stream) = *guard {
                // best-effort write; drop bytes if client disconnected
                let _ = (&*stream).write_all(&[b]);
            }
        }
    }));

    // Accept loop: each connection gets bridged to the UART.
    // When a client disconnects, wait for the next one.
    let uart_rx = uart.clone();
    let tx_accept = Arc::clone(&tx_stream);
    let name_owned = name.to_string();
    let log_clone = log.clone();
    thread::Builder::new()
        .name(format!("{name}-socket"))
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        info!(log_clone, "{name_owned} client connected");
                        // Install this stream for TX
                        if let Ok(mut guard) = tx_accept.lock() {
                            *guard = stream.try_clone().ok();
                        }
                        // RX: read from client → UART input
                        // Back-pressure: if the RX FIFO is full, wait
                        // for the guest to drain it before pushing more
                        // bytes.  c-bhyve does this via mevent_disable/
                        // enable; we use a simple retry loop instead.
                        let mut reader = stream;
                        let mut buf = [0u8; 64];
                        loop {
                            match reader.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    for &b in &buf[..n] {
                                        while !uart_rx.input_byte(b) {
                                            thread::sleep(
                                                std::time::Duration::from_micros(100),
                                            );
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        info!(log_clone, "{name_owned} client disconnected");
                        // Clear TX stream so bytes are dropped until next client
                        if let Ok(mut guard) = tx_accept.lock() {
                            *guard = None;
                        }
                    }
                    Err(e) => {
                        slog::warn!(log_clone, "{name_owned} accept error";
                            "error" => %e);
                    }
                }
            }
        })
        .expect("failed to spawn serial socket thread");

    Ok(())
}
