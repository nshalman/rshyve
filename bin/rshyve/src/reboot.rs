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

/// Build argv for the replacement process.
///
/// Two edits. The one-shot migration flags go, because a migration
/// destination must reboot as a normal VM and not wait for an inbound
/// migration that will never arrive. Every device an operator hot-added
/// gains a `-s` entry, or the disk they plugged in would vanish on the
/// guest's next reboot.
///
/// The `-s` entries already in argv are kept verbatim, in their argv
/// order. They carry the `bootindex` token, which the registry does not
/// record, so rebuilding the whole list from the registry would
/// silently change the boot order.
///
/// Devices only. A hot-added vCPU and hot-added memory are NOT carried
/// across a reset: `-c` and `-m` are copied verbatim, so the guest
/// comes back on its boot CPU count and boot memory size. Nothing here
/// can change that, because the caller passes device specs alone and
/// the added CPU and memory totals never leave the run loop.
fn reboot_argv(argv: &[OsString], hotplug: &[String]) -> Vec<OsString> {
    let mut result = Vec::with_capacity(argv.len() + 2 * hotplug.len());
    let Some((argv0, args)) = argv.split_first() else {
        return result;
    };
    result.push(argv0.clone());

    // Ahead of the positional VM name, where an option cannot be read
    // as the name itself.
    for spec in hotplug {
        result.push(OsString::from("-s"));
        result.push(OsString::from(spec));
    }

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
    hotplug: &[String],
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

    let args = reboot_argv(argv, hotplug);
    let mut command = Command::new(exe);
    command
        .args(args.iter().skip(1))
        .env(ENV_RESET_COUNT, count.to_string())
        .env(ENV_RESET_WINDOW_START_MS, start.to_string());
    info!(log, "guest requested reset, restarting VMM";
        "reset_count" => count, "hotplugged" => hotplug.len());
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

    fn specs(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn strips_separate_migrate_listen() {
        assert_eq!(
            reboot_argv(
                &argv(&["rshyve", "--migrate-listen", "0.0.0.0:4567", "vm"]),
                &[],
            ),
            argv(&["rshyve", "vm"]),
        );
    }

    #[test]
    fn strips_equals_migrate_listen() {
        assert_eq!(
            reboot_argv(
                &argv(&["rshyve", "--migrate-listen=0.0.0.0:4567", "vm"]),
                &[],
            ),
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
        assert_eq!(reboot_argv(&original, &[]), original);
    }

    #[test]
    fn preserves_argv0() {
        assert_eq!(
            reboot_argv(&argv(&["--migrate-listen", "vm"]), &[]),
            argv(&["--migrate-listen", "vm"]),
        );
    }

    #[test]
    fn a_hotplugged_disk_survives_the_reboot() {
        // Without this the device an operator plugged in is gone the
        // first time the guest restarts.
        let original = argv(&["rshyve", "-s", "4,nvme,/boot.img", "vm"]);

        let rebooted =
            reboot_argv(&original, &specs(&["5,virtio-blk,/added.img"]));

        assert_eq!(
            rebooted,
            argv(&[
                "rshyve",
                "-s",
                "5,virtio-blk,/added.img",
                "-s",
                "4,nvme,/boot.img",
                "vm",
            ]),
        );
    }

    #[test]
    fn the_bootindex_of_an_argv_device_is_kept() {
        // The registry records the stripped spec, so a rebuild from the
        // registry would drop the token and change the boot order.
        let original =
            argv(&["rshyve", "-s", "4,nvme,/boot.img,bootindex=1", "vm"]);

        let rebooted = reboot_argv(&original, &specs(&["5,virtio-rnd"]));

        assert!(
            rebooted.contains(&OsString::from("4,nvme,/boot.img,bootindex=1")),
            "got {rebooted:?}",
        );
    }

    #[test]
    fn every_hotplugged_device_is_replayed() {
        let rebooted = reboot_argv(
            &argv(&["rshyve", "vm"]),
            &specs(&["5,virtio-blk,/a.img", "6,virtio-blk,/b.img"]),
        );

        assert_eq!(
            rebooted,
            argv(&[
                "rshyve",
                "-s",
                "5,virtio-blk,/a.img",
                "-s",
                "6,virtio-blk,/b.img",
                "vm",
            ]),
        );
    }

    #[test]
    fn an_ejected_device_does_not_come_back() {
        // The registry drops a slot the guest ejected, so its spec is
        // not in the list, and the rebuilt argv is the boot argv.
        let original = argv(&["rshyve", "-s", "4,nvme,/boot.img", "vm"]);

        assert_eq!(reboot_argv(&original, &specs(&[])), original);
    }

    #[test]
    fn a_slot_pending_removal_comes_back() {
        // A guest that never runs _EJ0 still owns the device, so the
        // registry still lists it and the reboot must recreate it.
        let rebooted = reboot_argv(
            &argv(&["rshyve", "vm"]),
            &specs(&["5,virtio-blk,/pending.img"]),
        );

        assert_eq!(
            rebooted,
            argv(&["rshyve", "-s", "5,virtio-blk,/pending.img", "vm"]),
        );
    }

    #[test]
    fn the_full_config_string_of_an_added_device_is_replayed() {
        // Every option after the device name is part of the backend, so
        // a spec that loses one boots a different disk.
        let spec = "5,virtio-blk,/added.img,nocache,ro,sectorsize=4096";
        let rebooted = reboot_argv(&argv(&["rshyve", "vm"]), &specs(&[spec]));

        assert_eq!(rebooted, argv(&["rshyve", "-s", spec, "vm"]));
    }

    #[test]
    fn argv_slot_order_is_kept() {
        // Firmware walks the slots in order, so a reshuffle can change
        // which disk the guest boots.
        let rebooted = reboot_argv(
            &argv(&[
                "rshyve",
                "-s",
                "4,nvme,/boot.img,bootindex=1",
                "-s",
                "6,virtio-net,vnic0",
                "-s",
                "8,nvme,/data.img,bootindex=2",
                "vm",
            ]),
            &specs(&["5,virtio-blk,/added.img"]),
        );

        assert_eq!(
            rebooted,
            argv(&[
                "rshyve",
                "-s",
                "5,virtio-blk,/added.img",
                "-s",
                "4,nvme,/boot.img,bootindex=1",
                "-s",
                "6,virtio-net,vnic0",
                "-s",
                "8,nvme,/data.img,bootindex=2",
                "vm",
            ]),
        );
    }

    #[test]
    fn hot_added_cpus_and_memory_do_not_survive_a_reset() {
        // Known gap, pinned here so it cannot change unnoticed. The
        // rebuilt argv carries the boot `-c` and `-m`, so a guest that
        // grew comes back at its boot size.
        let original = argv(&[
            "rshyve",
            "-c",
            "cpus=2,maxcpus=8",
            "-m",
            "2G",
            "-s",
            "4,nvme,/boot.img",
            "vm",
        ]);

        let rebooted =
            reboot_argv(&original, &specs(&["5,virtio-blk,/added.img"]));

        assert!(rebooted.contains(&OsString::from("cpus=2,maxcpus=8")));
        assert!(rebooted.contains(&OsString::from("2G")));
    }

    #[test]
    fn a_machine_with_no_hotplug_reboots_on_its_own_argv() {
        // The path every VM that runs today takes.
        let original =
            argv(&["rshyve", "-c", "2", "-s", "4,nvme,/d.img", "vm"]);
        assert_eq!(reboot_argv(&original, &[]), original);
    }
}
