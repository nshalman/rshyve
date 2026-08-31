// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest-reset handling through process replacement.
//!
//! illumos bhyve establishes the fresh-process precedent in
//! `amd64/vmexit.c:538-539`: `case VM_SUSPEND_RESET: exit(0);`. This VMM follows
//! that model because current device lifecycle implementations provide no
//! cold-reset behavior beyond generic delegation. The normal startup pipeline
//! reconstructs every BAR, MSI-X table, virtio queue, and backend from scratch.
//!
//! Before re-exec, the caller attempts to quiesce and flush devices, waits
//! briefly for the vCPUs, and destroys the autodestruct VM. VMM-opened
//! non-stdio descriptors are close-on-exec, while stdin, stdout, and stderr
//! survive so `-l com1,stdio` remains attached. The control and framebuffer
//! listeners remove stale socket paths before binding, allowing the replacement
//! process to recreate them safely.

use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use slog::{error, info, Logger};
use vmm_machine::RunOutcome;

const RESET_WINDOW_MS: u64 = 60_000;
const RESET_MAX_PER_WINDOW: u32 = 10;
const ENV_RESET_COUNT: &str = "VMM_RESET_COUNT";
const ENV_RESET_WINDOW_START_MS: &str = "VMM_RESET_WINDOW_START_MS";

pub fn operator_stop_preempts_reboot(
    outcome: RunOutcome,
    sigterm_received: bool,
) -> bool {
    matches!(outcome, RunOutcome::Reboot) && sigterm_received
}

/// Apply the reset budget and return the state to carry across re-execs.
fn reset_budget(
    now_ms: u64,
    window_start_ms: Option<u64>,
    prev_count: u32,
) -> Option<(u64, u32)> {
    let (window_start_ms, prev_count) = match window_start_ms {
        Some(start) if now_ms.saturating_sub(start) <= RESET_WINDOW_MS => {
            (start, prev_count)
        }
        _ => (now_ms, 0),
    };

    let count = prev_count.saturating_add(1);
    (count <= RESET_MAX_PER_WINDOW).then_some((window_start_ms, count))
}

/// Build argv for the replacement process without one-shot migration flags.
///
/// A migration destination must reboot as a normal VM instead of waiting for
/// an inbound migration that will never arrive.
fn reboot_argv(argv: &[OsString]) -> Vec<OsString> {
    let mut result = Vec::with_capacity(argv.len());
    let Some((argv0, args)) = argv.split_first() else {
        return result;
    };
    result.push(argv0.clone());

    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg.as_os_str() == OsStr::new("--migrate-listen") {
            let _ = args.next();
        } else if arg.as_os_str().as_bytes().starts_with(b"--migrate-listen=") {
            continue;
        } else {
            result.push(arg.clone());
        }
    }

    result
}

pub fn reexec_for_reboot(
    exe: &Path,
    argv: &[OsString],
    sigterm_received: &AtomicBool,
    log: &Logger,
) -> ExitCode {
    if operator_stop_preempts_reboot(
        RunOutcome::Reboot,
        sigterm_received.load(Ordering::Relaxed),
    ) {
        info!(log, "operator stop pre-empted guest reset; not rebooting");
        return ExitCode::SUCCESS;
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let prev_start = env::var(ENV_RESET_WINDOW_START_MS)
        .ok()
        .and_then(|value| value.parse().ok());
    let prev_count = env::var(ENV_RESET_COUNT)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let Some((start, count)) = reset_budget(now_ms, prev_start, prev_count)
    else {
        error!(log, "reset rate limit exceeded, refusing to reboot";
            "limit" => RESET_MAX_PER_WINDOW,
            "window_ms" => RESET_WINDOW_MS);
        return ExitCode::from(4);
    };

    let args = reboot_argv(argv);
    let mut command = Command::new(exe);
    command
        .args(args.iter().skip(1))
        .env(ENV_RESET_COUNT, count.to_string())
        .env(ENV_RESET_WINDOW_START_MS, start.to_string());
    info!(log, "guest requested reset, restarting VMM";
        "reset_count" => count);
    // WHY: an explicit operator stop outranks a guest reset. Re-check at the
    // last userspace decision point so SIGTERM received during teardown or
    // reboot preparation cannot be discarded by the replacement process.
    if operator_stop_preempts_reboot(
        RunOutcome::Reboot,
        sigterm_received.load(Ordering::Relaxed),
    ) {
        info!(log, "operator stop pre-empted guest reset; not rebooting");
        return ExitCode::SUCCESS;
    }
    // Forking would leave the old VM process and its resources alive.
    let err = command.exec();
    error!(log, "re-exec failed"; "error" => %err);
    ExitCode::from(5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> Vec<OsString> {
        values.iter().map(|value| OsString::from(*value)).collect()
    }

    #[test]
    fn sigterm_preempts_guest_reboot() {
        assert!(operator_stop_preempts_reboot(RunOutcome::Reboot, true));
    }

    #[test]
    fn reboot_without_sigterm_is_preserved() {
        assert!(!operator_stop_preempts_reboot(RunOutcome::Reboot, false));
    }

    #[test]
    fn sigterm_does_not_reclassify_poweroff() {
        assert!(!operator_stop_preempts_reboot(RunOutcome::PowerOff, true));
    }

    #[test]
    fn sigterm_does_not_reclassify_guest_fault() {
        assert!(!operator_stop_preempts_reboot(RunOutcome::GuestFault, true));
    }

    #[test]
    fn budget_first_reset_starts_window() {
        assert_eq!(reset_budget(42_000, None, 0), Some((42_000, 1)));
    }

    #[test]
    fn budget_increments_within_window() {
        assert_eq!(reset_budget(11_000, Some(10_000), 3), Some((10_000, 4)));
    }

    #[test]
    fn budget_allows_reset_at_limit() {
        assert_eq!(reset_budget(11_000, Some(10_000), 9), Some((10_000, 10)));
    }

    #[test]
    fn budget_rejects_over_limit() {
        assert_eq!(reset_budget(11_000, Some(10_000), 10), None);
    }

    #[test]
    fn budget_keeps_exact_window_boundary() {
        assert_eq!(reset_budget(70_000, Some(10_000), 3), Some((10_000, 4)));
    }

    #[test]
    fn budget_resets_after_window() {
        assert_eq!(reset_budget(70_001, Some(10_000), 9), Some((70_001, 1)));
    }

    #[test]
    fn budget_survives_clock_going_backwards() {
        assert_eq!(reset_budget(9_000, Some(10_000), 3), Some((10_000, 4)));
    }

    #[test]
    fn strips_separate_migrate_listen() {
        assert_eq!(
            reboot_argv(&argv(&[
                "rshyve",
                "--migrate-listen",
                "0.0.0.0:4567",
                "vm",
            ])),
            argv(&["rshyve", "vm"]),
        );
    }

    #[test]
    fn strips_equals_migrate_listen() {
        assert_eq!(
            reboot_argv(&argv(&[
                "rshyve",
                "--migrate-listen=0.0.0.0:4567",
                "vm",
            ])),
            argv(&["rshyve", "vm"]),
        );
    }

    #[test]
    fn preserves_slot_and_lpc_flags() {
        let original = argv(&[
            "rshyve",
            "-s",
            "4,nvme,/d.img",
            "-l",
            "com1,stdio",
            "--vtpm",
            "vm",
        ]);
        assert_eq!(reboot_argv(&original), original);
    }

    #[test]
    fn preserves_argv0() {
        assert_eq!(
            reboot_argv(&argv(&["--migrate-listen", "vm"])),
            argv(&["--migrate-listen", "vm"]),
        );
    }
}
