// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `virtiofs-test`: PID 1 that mounts a virtio-fs share and reports on
//! it, then powers the VM off.
//!
//! The same binary is also staged inside the share as `exec-probe`. It
//! tells the two roles apart by PID: only the kernel starts init as PID
//! 1, so any other PID means this copy was executed off the share by
//! the `exec_from_share` check, and it exits with [`PROBE_STATUS`].
//!
//! Reads its settings from the kernel command line:
//!
//! | parameter | default | meaning |
//! |---|---|---|
//! | `virtiofs.tag=` | `testfs` | mount tag the device advertises |
//! | `virtiofs.mode=` | `rw` | `ro` asserts every mutation gets EROFS |
//! | `virtiofs.role=` | `checks` | `container` boots the share as a rootfs, `hotplug` checks hot-added resources |
//! | `virtiofs.entry=` | none | run this script instead of the image's entrypoint |
//!
//! The `hotplug` role reads its own `hotplug.*` keys, which
//! [`hotplug::HotplugSpec`] documents.
//!
//! Cross-compile target: `x86_64-unknown-linux-musl`. The binary must be
//! statically linked: the initramfs has no dynamic loader, and the copy
//! on the share has to run without one too.

mod cmdline;
mod hotplug;
mod report;
mod spec;

#[cfg(target_os = "linux")]
mod checks;
#[cfg(target_os = "linux")]
mod container;
#[cfg(target_os = "linux")]
mod hotplug_checks;
#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod mount;

#[cfg(target_os = "linux")]
fn main() -> ! {
    init::main()
}

/// The guest runs only on Linux, but the spec parsing is host
/// independent, so a stub entry point keeps `cargo test` useful on a
/// developer's own machine.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("virtiofs-test runs as PID 1 inside a Linux guest");
    std::process::exit(2);
}
