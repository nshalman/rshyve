// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! COM1 console and the COM2 marker channel.
//!
//! The default kernel command line names `ttyS0`, so without COM1 a
//! guest produces no output at all. COM2 carries out-of-band markers
//! from the guest. PS/2 and the metadata agent stay rshyve-only.
//!
//! Both ports are served on the stdio this process inherits. The
//! zone brand names a backend for each one, and firehyve accepts that
//! name and ignores it. See `report_serial_backends`.

use std::io::{Read, Write};
use std::sync::Arc;

use slog::{info, warn};

use vmm_core::intr_pins::{IntrPin, LegacyPIC, NoOpPin};
use vmm_core::machine::Machine;
use vmm_devices::uart::lpc::{self, LpcUart, TxSink};

/// The LPC ports this binary serves, both on stdout.
const SERVED_PORTS: [&str; 2] = ["com1", "com2"];

/// Mirror one UART's transmit path to this process's stdout. Both
/// ports share the destination, so a host reads console output and
/// markers from one stream.
fn stdout_sink(port: &'static str) -> TxSink {
    Box::new(move |b: u8| {
        let mut out = std::io::stdout();
        if let Err(e) = out.write_all(&[b]) {
            eprintln!("[firehyve] {port} stdout write error: {e}");
        }
        let _ = out.flush();
    })
}

/// Report each `-l com1,<backend>` / `-l com2,<backend>` this binary
/// accepts but does not use.
///
/// The bhyve zone brand puts a backend on every zone: `com1` comes
/// from the zonecfg attr, which tritond-vmadm always sets to
/// `/dev/zconsole`, and `com2` to `socket,/tmp/vm.ttyb`. firehyve
/// serves both ports on the stdio it inherits from zhyve, so it cannot
/// honour either name. Refusing them would mean an unmodified brand
/// can never start this binary, so they are accepted and logged.
///
/// The log line is the point. zhyve points stdout at `/dev/zfd/1`,
/// which zoneadmd copies into the zone log, NOT at the `/dev/zconsole`
/// the brand asked for. So `zlogin -C` stays dark and the operator has
/// to be told where the bytes went.
///
/// Kept apart from the attach functions so the decision is testable
/// without a live VM, and so `validate_cli` can reach it before the VM
/// exists.
pub(crate) fn report_serial_backends(lpc_args: &[String], log: &slog::Logger) {
    for port in SERVED_PORTS {
        let Some(backend) = vmm_machine::find_lpc_device(lpc_args, port) else {
            continue;
        };
        if backend == "stdio" {
            continue;
        }
        warn!(log, "serial backend ignored";
            "port" => port,
            "requested" => &backend,
            "served_on" => "inherited stdio",
            "note" => "zoneadmd copies stdout to the zone log, \
                       not to the zone console",
        );
    }
}

/// Attach COM1 and connect it to this process's stdio.
pub(crate) fn setup_console(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<LpcUart>> {
    let pin: Arc<dyn IntrPin> = match pic.pin_handle(lpc::COM1_IRQ) {
        Some(p) => p,
        None => Arc::new(NoOpPin),
    };
    let uart = LpcUart::new(pin);
    uart.attach(machine.bus_pio(), lpc::COM1_BASE);

    uart.set_tx_sink(stdout_sink("com1"));

    let rx = Arc::clone(&uart);
    std::thread::Builder::new()
        .name("com1-input".into())
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
            anyhow::anyhow!("failed to spawn com1 input thread: {e}")
        })?;

    info!(log, "COM1 attached to stdio";
        "port" => format!("{:#x}", lpc::COM1_BASE));
    Ok(uart)
}

/// Attach COM2 and mirror it to stdout as a one-way marker channel.
///
/// Separate from COM1 on purpose. A guest that opens `/dev/ttyS1`
/// writes bytes that never enter the kernel console or printk path, so
/// the host still reads boot markers when the guest drops `console=`
/// from its command line for speed. `fhrun-init` sends `init-start`
/// and `ready` this way, and the fhrun boot harness times them.
///
/// Transmit only. COM1 owns the single stdin reader, and a second
/// reader would take bytes away from it.
pub(crate) fn setup_com2(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<LpcUart>> {
    let pin: Arc<dyn IntrPin> = match pic.pin_handle(lpc::COM2_IRQ) {
        Some(p) => p,
        None => Arc::new(NoOpPin),
    };
    let uart = LpcUart::new(pin);
    uart.attach(machine.bus_pio(), lpc::COM2_BASE);
    uart.set_tx_sink(stdout_sink("com2"));

    info!(log, "COM2 attached to stdio";
        "port" => format!("{:#x}", lpc::COM2_BASE));
    Ok(uart)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn lpc(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    /// Keeps every record so a test can prove firehyve logged an
    /// ignored option instead of dropping it quietly.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Recorder {
        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("recorder lock").clone()
        }

        fn logger(&self) -> slog::Logger {
            slog::Logger::root(self.clone(), slog::o!())
        }
    }

    /// Flattens a record and its key/value pairs into one string.
    struct Flatten(String);

    impl slog::Serializer for Flatten {
        fn emit_arguments(
            &mut self,
            key: slog::Key,
            val: &std::fmt::Arguments<'_>,
        ) -> slog::Result {
            use std::fmt::Write as _;
            let _ = write!(self.0, " {key}={val}");
            Ok(())
        }
    }

    impl slog::Drain for Recorder {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            values: &slog::OwnedKVList,
        ) -> Result<Self::Ok, Self::Err> {
            use slog::KV as _;
            let mut out =
                Flatten(format!("{} {}", record.level(), record.msg()));
            let _ = record.kv().serialize(record, &mut out);
            let _ = values.serialize(record, &mut out);
            self.0.lock().expect("recorder lock").push(out.0);
            Ok(())
        }
    }

    #[test]
    fn a_stdio_backend_or_no_entry_at_all_reports_nothing() {
        // Nothing is ignored in either case, so a log line here would
        // be noise on the one path that needs none.
        for args in [lpc(&[]), lpc(&["com1,stdio"]), lpc(&["com2,stdio"])] {
            let rec = Recorder::default();
            report_serial_backends(&args, &rec.logger());
            assert!(rec.lines().is_empty(), "reported: {:?}", rec.lines());
        }
    }

    /// tritond-vmadm sets the `com1` zonecfg attr to `/dev/zconsole` on
    /// every bhyve zone, so refusing it means an unmodified brand can
    /// never start firehyve. It is accepted, but it must not be
    /// accepted quietly: the bytes go to the zone log, not to the
    /// zone console, and only this line tells the operator that.
    ///
    /// Mutation this kills: deleting the warn! and keeping the loop, or
    /// dropping the "requested" pair so the line does not say what was
    /// ignored.
    #[test]
    fn the_com1_backend_the_brand_emits_is_accepted_and_logged() {
        let rec = Recorder::default();
        report_serial_backends(&lpc(&["com1,/dev/zconsole"]), &rec.logger());

        let lines = rec.lines();
        assert_eq!(lines.len(), 1, "expected one line, got {lines:?}");
        let line = &lines[0];
        assert!(line.starts_with("WARN"), "must be a warning: {line}");
        assert!(line.contains("/dev/zconsole"), "names no backend: {line}");
        assert!(line.contains("com1"), "names no port: {line}");
    }

    /// `-l com2,socket,/tmp/vm.ttyb` is on every bhyve zone too, and
    /// firehyve has no COM2 agent to serve it.
    ///
    /// Mutation this kills: reporting com1 only.
    #[test]
    fn the_com2_backend_the_brand_emits_is_accepted_and_logged() {
        let rec = Recorder::default();
        report_serial_backends(
            &lpc(&["com2,socket,/tmp/vm.ttyb"]),
            &rec.logger(),
        );

        let lines = rec.lines();
        assert_eq!(lines.len(), 1, "expected one line, got {lines:?}");
        assert!(lines[0].contains("com2"), "names no port: {}", lines[0]);
        assert!(
            lines[0].contains("/tmp/vm.ttyb"),
            "names no backend: {}",
            lines[0]
        );
    }

    /// Both ports at once, which is what the brand actually builds.
    #[test]
    fn both_brand_serial_backends_are_reported_one_line_each() {
        let rec = Recorder::default();
        report_serial_backends(
            &lpc(&["com1,/dev/zconsole", "com2,socket,/tmp/vm.ttyb"]),
            &rec.logger(),
        );
        assert_eq!(rec.lines().len(), 2, "got {:?}", rec.lines());
    }

    /// Mutation this kills: matching on any `-l` entry instead of the
    /// two serial ports, which would report the bootrom here as well
    /// as in validate_cli.
    #[test]
    fn an_lpc_entry_for_another_device_is_not_reported() {
        let rec = Recorder::default();
        report_serial_backends(
            &lpc(&["bootrom,/usr/share/bhyve/uefi-rom.bin"]),
            &rec.logger(),
        );
        assert!(rec.lines().is_empty(), "reported: {:?}", rec.lines());
    }
}
