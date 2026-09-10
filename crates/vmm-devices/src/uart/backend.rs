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

use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use slog::{info, warn};

use super::lpc::LpcUart;

/// How many transmit errors are logged before the backend goes quiet.
/// A console whose reader has gone away fails on every byte, and an
/// unbounded log would bury everything else in the zone log.
const TX_ERROR_LOG_LIMIT: u32 = 3;

/// Receive poll timeout. Bounded so the thread notices a closed
/// backend instead of parking for ever.
const POLL_TIMEOUT_MS: libc::c_int = 1000;

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
///
/// The open uses `libc` rather than `std::fs` to match what C bhyve
/// does, because a pty on illumos needs the STREAMS modules that path
/// sets up.
fn attach_device(
    uart: &Arc<LpcUart>,
    path: &str,
    name: &str,
    log: &slog::Logger,
) -> anyhow::Result<()> {
    let c_path = std::ffi::CString::new(path)
        .map_err(|_| anyhow::anyhow!("{name} device path contains a NUL"))?;
    // O_NONBLOCK on the open itself, so a device with no carrier does
    // not park the whole process here.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(anyhow::anyhow!(
            "cannot open {name} device {path}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let guard = FdGuard(fd);

    // A console must not translate: the guest owns the line discipline.
    // A backend that is not a terminal has no attributes to set, so a
    // failed tcgetattr is not an error.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) == 0 {
            libc::cfmakeraw(&mut tio);
            // CLOCAL: no modem, so never wait on carrier.
            tio.c_cflag |= libc::CLOCAL;
            libc::tcsetattr(fd, libc::TCSANOW, &tio);
        }
        // Keep the description non-blocking. A transmit must never
        // stall a vCPU because nobody is draining the console.
        libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
    }

    // Separate descriptors for the two directions, both close-on-exec
    // so a guest-reset re-exec does not inherit them.
    let write_fd = dup_cloexec(fd)
        .map_err(|e| anyhow::anyhow!("cannot dup {name} device: {e}"))?;
    let read_fd = dup_cloexec(fd).map_err(|e| {
        unsafe { libc::close(write_fd) };
        anyhow::anyhow!("cannot dup {name} device: {e}")
    })?;
    // The dups keep the open file description alive.
    drop(guard);

    let tx_errors = AtomicU32::new(0);
    let tx_log = log.clone();
    let tx_name = name.to_string();
    uart.set_tx_sink(Box::new(move |b: u8| {
        let ret =
            unsafe { libc::write(write_fd, std::ptr::from_ref(&b).cast(), 1) };
        if ret < 0
            && tx_errors.fetch_add(1, Ordering::Relaxed) < TX_ERROR_LOG_LIMIT
        {
            warn!(tx_log, "serial transmit failed"; "port" => &tx_name,
                "error" => %std::io::Error::last_os_error());
        }
    }));

    spawn_device_reader(uart, read_fd, name)?;
    info!(log, "serial port connected"; "port" => name, "backend" => path);
    Ok(())
}

/// Read from `read_fd` into the port's receive FIFO until it closes.
fn spawn_device_reader(
    uart: &Arc<LpcUart>,
    read_fd: RawFd,
    name: &str,
) -> anyhow::Result<()> {
    let rx = Arc::clone(uart);
    let spawned = std::thread::Builder::new()
        .name(format!("{name}-input"))
        .spawn(move || {
            let _owned = FdGuard(read_fd);
            let mut buf = [0u8; 64];
            loop {
                let mut pfd = libc::pollfd {
                    fd: read_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
                if ready < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if ready == 0 {
                    continue;
                }
                if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    break;
                }
                if pfd.revents & libc::POLLIN == 0 {
                    continue;
                }
                let n = unsafe {
                    libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len())
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) {
                        continue;
                    }
                    break;
                }
                if n == 0 {
                    break;
                }
                for byte in buf.iter().take(n as usize) {
                    rx.input_byte(*byte);
                }
            }
        });
    match spawned {
        Ok(_) => Ok(()),
        Err(e) => {
            unsafe { libc::close(read_fd) };
            Err(anyhow::anyhow!("cannot spawn {name} input thread: {e}"))
        }
    }
}

fn dup_cloexec(fd: RawFd) -> std::io::Result<RawFd> {
    let out = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if out < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(out)
}

/// Closes a descriptor when it goes out of scope, so an error path
/// between `open` and the two dups cannot leak one.
struct FdGuard(RawFd);

impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
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
        master: RawFd,
        slave_path: String,
    }

    impl Pty {
        fn open() -> Self {
            let ptmx = std::ffi::CString::new("/dev/ptmx").expect("ptmx path");
            let master = unsafe {
                libc::open(ptmx.as_ptr(), libc::O_RDWR | libc::O_NOCTTY)
            };
            assert!(master >= 0, "open /dev/ptmx: {}", last_error());
            assert_eq!(unsafe { libc::grantpt(master) }, 0, "grantpt");
            assert_eq!(unsafe { libc::unlockpt(master) }, 0, "unlockpt");
            let name = unsafe { libc::ptsname(master) };
            assert!(!name.is_null(), "ptsname");
            let slave_path = unsafe { std::ffi::CStr::from_ptr(name) }
                .to_str()
                .expect("slave path is utf8")
                .to_string();
            Self { master, slave_path }
        }

        fn write(&self, bytes: &[u8]) {
            let n = unsafe {
                libc::write(self.master, bytes.as_ptr().cast(), bytes.len())
            };
            assert_eq!(n, bytes.len() as isize, "write: {}", last_error());
        }

        /// Read until `want` bytes arrive or the deadline passes.
        fn read_exact(&self, want: usize) -> Vec<u8> {
            let deadline = Instant::now() + RX_DEADLINE;
            let mut out = Vec::new();
            let mut buf = [0u8; 64];
            while out.len() < want && Instant::now() < deadline {
                let mut pfd = libc::pollfd {
                    fd: self.master,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut pfd, 1, 200) } <= 0 {
                    continue;
                }
                let n = unsafe {
                    libc::read(self.master, buf.as_mut_ptr().cast(), buf.len())
                };
                if n <= 0 {
                    continue;
                }
                out.extend_from_slice(&buf[..n as usize]);
            }
            out
        }
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            unsafe { libc::close(self.master) };
        }
    }

    fn last_error() -> std::io::Error {
        std::io::Error::last_os_error()
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
