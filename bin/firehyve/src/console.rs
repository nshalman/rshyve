// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! COM1 console and the COM2 marker channel.
//!
//! The default kernel command line names `ttyS0`, so without COM1 a
//! guest produces no output at all. COM2 carries out-of-band markers
//! from the guest. PS/2 and the metadata agent stay rshyve-only.
//!
//! stdio is the only backend for both. rshyve's zconsole and
//! Unix-socket paths belong to a supervised zone, which is not the
//! shape firehyve runs in.

use std::io::{Read, Write};
use std::sync::Arc;

use slog::info;

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

/// Reject a `-l com1,<backend>` or `-l com2,<backend>` this binary
/// cannot serve.
///
/// Kept apart from the attach functions so the one behavioral decision
/// here is testable without a live VM, and so `validate_cli` can reach
/// it before the VM exists.
pub(crate) fn require_stdio_backend(lpc_args: &[String]) -> anyhow::Result<()> {
    for port in SERVED_PORTS {
        match vmm_machine::find_lpc_device(lpc_args, port).as_deref() {
            None | Some("stdio") => {}
            Some(other) => anyhow::bail!(
                "firehyve serves {port} on stdio only; \
                 '-l {port},{other}' is rshyve only"
            ),
        }
    }
    Ok(())
}

/// Attach COM1 and connect it to this process's stdio.
pub(crate) fn setup_console(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    cli: &vmm_config::Cli,
    log: &slog::Logger,
) -> anyhow::Result<Arc<LpcUart>> {
    require_stdio_backend(&cli.lpc)?;

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

    fn lpc(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn an_absent_com1_entry_defaults_to_stdio() {
        require_stdio_backend(&lpc(&[])).expect("default is stdio");
    }

    #[test]
    fn an_explicit_stdio_backend_is_accepted() {
        require_stdio_backend(&lpc(&["com1,stdio"])).expect("stdio");
    }

    #[test]
    fn any_other_backend_is_rejected_at_startup() {
        // rshyve serves zconsole and a Unix socket on COM1. firehyve
        // has neither, so an operator learns now instead of finding a
        // console that produces no output.
        let err = require_stdio_backend(&lpc(&["com1,/dev/zconsole"]))
            .expect_err("zconsole is rshyve only");
        assert!(err.to_string().contains("/dev/zconsole"), "got: {err}");

        assert!(require_stdio_backend(&lpc(&["com1,socket,/tmp/c"])).is_err());
    }

    #[test]
    fn com2_also_serves_stdio_only() {
        // COM2 carries fhrun's boot markers to the host on stdout. A
        // zconsole backend would send them where firehyve cannot read
        // them, so the operator learns at startup.
        require_stdio_backend(&lpc(&["com2,stdio"])).expect("stdio");

        let err = require_stdio_backend(&lpc(&["com2,/dev/zconsole"]))
            .expect_err("zconsole is rshyve only");
        assert!(err.to_string().contains("com2"), "got: {err}");
    }

    #[test]
    fn an_lpc_entry_for_another_device_is_ignored() {
        require_stdio_backend(&lpc(&["bootrom,/usr/share/bhyve/uefi-rom.bin"]))
            .expect("not a serial port");
    }
}
