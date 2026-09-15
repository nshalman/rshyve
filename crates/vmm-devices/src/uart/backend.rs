// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host-side backends for an emulated UART.
//!
//! An emulated serial port needs a destination for the bytes the guest
//! transmits and a source for the bytes it receives. Two backends are
//! shared by every binary here:
//!
//! - [`SerialBackend::Stdio`], which a hypervisor started from a
//!   terminal uses, and
//! - [`SerialBackend::Device`], which is what the bhyve zone brand
//!   names when it emits `-l com1,/dev/zconsole`. That path is the
//!   `zcons` slave, and it is the only channel `zlogin -C` reads. A
//!   process's inherited stdout goes to `/dev/zfd/1`, which zoneadmd
//!   copies into the zone log instead, so a binary that serves COM1 on
//!   stdio leaves the operator with a dead console.
//!
//! Both live here so the zone console has one implementation.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slog::{info, warn};
use vmm_core::poll::{wait_readable, Readiness};

use super::lpc::LpcUart;

/// How many transmit errors are logged before the backend goes quiet.
/// A console whose reader has gone away fails on every byte, and an
/// unbounded log would bury everything else in the zone log.
const TX_ERROR_LOG_LIMIT: u32 = 3;

/// Receive poll timeout. Bounded so the thread notices a closed
/// backend instead of parking for ever.
const POLL_TIMEOUT: Duration = Duration::from_secs(1);

/// Where one emulated serial port's bytes come from and go to.
pub enum SerialBackend<'a> {
    /// This process's own stdio: transmit to stdout, receive from
    /// stdin.
    Stdio,
    /// This process's stdout, with no receive path. Used for a one-way
    /// marker channel, and for the second port when the first one
    /// already owns stdin.
    StdoutOnly,
    /// A character device or pty, opened read/write. `/dev/zconsole`
    /// is the case that matters.
    Device(&'a str),
}

/// Connect `uart` to `backend`.
///
/// `name` labels the port in log records and in the receive thread's
/// name.
pub fn attach(
    uart: &Arc<LpcUart>,
    backend: SerialBackend<'_>,
    name: &str,
    log: &slog::Logger,
) -> anyhow::Result<()> {
    match backend {
        SerialBackend::Stdio => {
            attach_stdout(uart, name);
            spawn_stdin_reader(uart, name)?;
            info!(log, "serial port connected"; "port" => name,
                "backend" => "stdio");
            Ok(())
        }
        SerialBackend::StdoutOnly => {
            attach_stdout(uart, name);
            info!(log, "serial port connected"; "port" => name,
                "backend" => "stdout");
            Ok(())
        }
        SerialBackend::Device(path) => attach_device(uart, path, name, log),
    }
}

/// Send this port's transmit bytes to stdout.
fn attach_stdout(uart: &Arc<LpcUart>, name: &str) {
    let port = name.to_string();
    uart.set_tx_sink(Box::new(move |b: u8| {
        let mut out = std::io::stdout();
        if let Err(e) = out.write_all(&[b]) {
            eprintln!("[serial] {port} stdout write error: {e}");
        }
        let _ = out.flush();
    }));
}

/// Feed stdin into this port's receive FIFO.
///
/// Only one port may do this: a second reader would take bytes away
/// from the first.
fn spawn_stdin_reader(uart: &Arc<LpcUart>, name: &str) -> anyhow::Result<()> {
    let rx = Arc::clone(uart);
    std::thread::Builder::new()
        .name(format!("{name}-input"))
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut buf = [0u8; 1];
            loop {
                match stdin.lock().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        rx.input_byte(buf[0]);
                    }
                }
            }
        })
        .map_err(|e| {
            anyhow::anyhow!("cannot spawn {name} input thread: {e}")
        })?;
    Ok(())
}

/// Open `path` read/write and serve both directions of this port on it.
fn attach_device(
    uart: &Arc<LpcUart>,
    path: &str,
    name: &str,
    log: &slog::Logger,
) -> anyhow::Result<()> {
    // O_NONBLOCK on the open itself, so a device with no carrier does
    // not park the whole process here. It stays on the description, so
    // a transmit never stalls a vCPU because nobody drains the console.
    let device = File::options()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            anyhow::anyhow!("cannot open {name} device {path}: {e}")
        })?;

    make_raw(&device);

    // Separate descriptors for the two directions. `try_clone` makes
    // them close-on-exec, so a guest-reset re-exec does not inherit
    // them.
    let write_half = device
        .try_clone()
        .map_err(|e| anyhow::anyhow!("cannot dup {name} device: {e}"))?;
    let read_half = device
        .try_clone()
        .map_err(|e| anyhow::anyhow!("cannot dup {name} device: {e}"))?;
    drop(device);

    let tx_errors = AtomicU32::new(0);
    let tx_log = log.clone();
    let tx_name = name.to_string();
    uart.set_tx_sink(Box::new(move |b: u8| {
        if let Err(e) = (&write_half).write(&[b]) {
            if tx_errors.fetch_add(1, Ordering::Relaxed) < TX_ERROR_LOG_LIMIT {
                warn!(tx_log, "serial transmit failed"; "port" => &tx_name,
                    "error" => %e);
            }
        }
    }));

    spawn_device_reader(uart, read_half, name)?;
    info!(log, "serial port connected"; "port" => name, "backend" => path);
    Ok(())
}

/// Stop the line discipline translating anything: the guest owns it.
///
/// A backend that is not a terminal has no attributes to set, so a
/// failed `tcgetattr` is not an error.
fn make_raw(device: &File) {
    let fd = device.as_raw_fd();
    // Safety: `tio` is a whole termios and `fd` is open for the calls.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) == 0 {
            libc::cfmakeraw(&mut tio);
            // CLOCAL: no modem, so never wait on carrier.
            tio.c_cflag |= libc::CLOCAL;
            libc::tcsetattr(fd, libc::TCSANOW, &tio);
        }
    }
}

/// Read from `device` into the port's receive FIFO until it closes.
fn spawn_device_reader(
    uart: &Arc<LpcUart>,
    device: File,
    name: &str,
) -> anyhow::Result<()> {
    let rx = Arc::clone(uart);
    std::thread::Builder::new()
        .name(format!("{name}-input"))
        .spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match wait_readable(device.as_fd(), POLL_TIMEOUT) {
                    Readiness::Readable => {}
                    Readiness::Idle => continue,
                    Readiness::Gone => break,
                }
                match (&device).read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for byte in &buf[..n] {
                            rx.input_byte(*byte);
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| {
            anyhow::anyhow!("cannot spawn {name} input thread: {e}")
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use vmm_core::intr_pins::IntrPin;
    use vmm_core::pio::PioBus;

    use super::super::lpc::{COM1_BASE, REGISTER_LEN};

    /// How long a test waits for the receive thread to move a byte.
    const RX_DEADLINE: Duration = Duration::from_secs(5);

    struct TestPin(AtomicBool);

    impl IntrPin for TestPin {
        fn assert(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn deassert(&self) {
            self.0.store(false, Ordering::SeqCst);
        }
        fn is_asserted(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn quiet_logger() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// A pty pair, which is the shape `/dev/zconsole` has: zoneadmd
    /// holds the master, and the backend opens the slave by path.
    struct Pty {
        master: File,
        slave_path: String,
    }

    impl Pty {
        fn open() -> Self {
            let master = File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open("/dev/ptmx")
                .expect("open /dev/ptmx");
            let fd = master.as_raw_fd();
            // Safety: `fd` is the open master for the whole block.
            let slave_path = unsafe {
                assert_eq!(libc::grantpt(fd), 0, "grantpt");
                assert_eq!(libc::unlockpt(fd), 0, "unlockpt");
                let name = libc::ptsname(fd);
                assert!(!name.is_null(), "ptsname");
                std::ffi::CStr::from_ptr(name)
                    .to_str()
                    .expect("slave path is utf8")
                    .to_string()
            };
            Self { master, slave_path }
        }

        fn write(&self, bytes: &[u8]) {
            (&self.master)
                .write_all(bytes)
                .expect("write to the master");
        }

        /// Read until `want` bytes arrive or the deadline passes.
        fn read_exact(&self, want: usize) -> Vec<u8> {
            let deadline = Instant::now() + RX_DEADLINE;
            let mut out = Vec::new();
            let mut buf = [0u8; 64];
            while out.len() < want && Instant::now() < deadline {
                if wait_readable(
                    self.master.as_fd(),
                    Duration::from_millis(200),
                ) != Readiness::Readable
                {
                    continue;
                }
                let Ok(n) = (&self.master).read(&mut buf) else {
                    continue;
                };
                out.extend_from_slice(&buf[..n]);
            }
            out
        }
    }

    fn uart_on_bus() -> (Arc<LpcUart>, PioBus) {
        let bus = PioBus::new();
        let uart = LpcUart::new(Arc::new(TestPin(AtomicBool::new(false))));
        uart.attach(&bus, COM1_BASE);
        (uart, bus)
    }

    /// Drain the guest-visible receive register until `want` bytes are
    /// read or the deadline passes.
    fn guest_read(bus: &PioBus, want: usize) -> Vec<u8> {
        const LSR_DATA_READY: u32 = 0x01;
        let deadline = Instant::now() + RX_DEADLINE;
        let mut out = Vec::new();
        while out.len() < want && Instant::now() < deadline {
            let lsr = bus.handle_in(COM1_BASE + 5, 1);
            if lsr & LSR_DATA_READY == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            out.push(bus.handle_in(COM1_BASE, 1) as u8);
        }
        out
    }

    /// The whole point of the device backend: what the guest writes has
    /// to leave through the device the brand named, not through this
    /// process's stdout.
    ///
    /// Mutation this kills: transmitting to stdout, or installing no
    /// transmit sink at all, for a `Device` backend.
    #[test]
    fn a_device_backend_transmits_to_the_device() {
        let pty = Pty::open();
        let (uart, bus) = uart_on_bus();
        attach(
            &uart,
            SerialBackend::Device(&pty.slave_path),
            "com1",
            &quiet_logger(),
        )
        .expect("attach to the pty slave");

        for b in b"OK" {
            bus.handle_out(COM1_BASE, 1, u32::from(*b));
        }

        assert_eq!(pty.read_exact(2), b"OK", "device saw no guest output");
        assert_eq!(REGISTER_LEN, 8, "port window unchanged");
    }

    /// Input is the half that makes `zlogin -C` a console rather than a
    /// log. Without it an operator can watch a guest and never type at
    /// it.
    ///
    /// Mutation this kills: dropping the receive thread, or reading the
    /// device and discarding the bytes instead of calling input_byte.
    #[test]
    fn a_device_backend_feeds_the_guest_what_arrives() {
        let pty = Pty::open();
        let (uart, bus) = uart_on_bus();
        attach(
            &uart,
            SerialBackend::Device(&pty.slave_path),
            "com1",
            &quiet_logger(),
        )
        .expect("attach to the pty slave");

        pty.write(b"hi");

        assert_eq!(guest_read(&bus, 2), b"hi", "guest saw no input");
    }

    /// A path that cannot be opened must be reported, not swallowed.
    /// The caller decides what to do; a silent failure would leave a
    /// console that is dead for no stated reason.
    ///
    /// Mutation this kills: returning Ok(()) when open fails.
    #[test]
    fn a_device_that_cannot_be_opened_is_an_error() {
        let (uart, _bus) = uart_on_bus();
        let err = attach(
            &uart,
            SerialBackend::Device("/nonexistent/nwboot/zconsole"),
            "com1",
            &quiet_logger(),
        )
        .expect_err("a missing device must not report success");
        let text = err.to_string();
        assert!(
            text.contains("/nonexistent/nwboot/zconsole"),
            "error names no path: {text}"
        );
    }

    /// A NUL in the path would truncate at the C boundary and open a
    /// different file from the one named.
    ///
    /// Mutation this kills: building the CString with unwrap-on-error
    /// removed, or trimming at the NUL.
    #[test]
    fn a_device_path_with_an_embedded_nul_is_refused() {
        let (uart, _bus) = uart_on_bus();
        attach(
            &uart,
            SerialBackend::Device("/dev/zconsole\0/x"),
            "com1",
            &quiet_logger(),
        )
        .expect_err("an embedded NUL must be refused");
    }
}
