// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fhrun-init`: the minimal PID 1 for a fhrun microVM.
//!
//! Responsibilities, in order:
//! 1. Mount `/proc`, `/sys`, and make `/dev` usable.
//! 2. Read `/firehyve-spec.json`.
//! 3. Bring up each NIC the spec describes, with its IPv4 address and,
//!    for the first NIC that names one, a default route.
//! 4. `execve()` the payload binary at `spec.bin` with `spec.args` and
//!    the given environment. argv[0] is the binary's basename.
//! 5. On any error, trigger ACPI poweroff through `/proc/sysrq-trigger`
//!    so the VMM reports a clean shutdown.
//!
//! Cross-compile target: `x86_64-unknown-linux-musl`. The init must be
//! statically linked because the guest rootfs has no dynamic loader.

mod net;
mod spec;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::spec::GuestSpec;

const SPEC_PATH: &str = "/firehyve-spec.json";

fn main() -> ! {
    // The earliest point userspace can signal the host from, so it
    // brackets the same interval Firecracker's spec measures ("API call
    // to start of /sbin/init"). devtmpfs must be mounted first because
    // the initramfs ships an empty /dev, so /dev/ttyS1 does not exist
    // yet. The kernel populates devtmpfs from already registered
    // devices, so the mount costs microseconds. The later
    // mount_essentials() call gets EBUSY here and treats it as success.
    let _ = do_mount("devtmpfs", "/dev", "devtmpfs", 0);
    ttys1_ping("fhrun-init: init-start\n");

    if let Err(e) = run() {
        klog(&format!("fhrun-init: fatal: {e}"));
    }
    // Never return from PID 1. Trigger ACPI poweroff so the VMM sees a
    // power-off suspend and exits with rc 0.
    poweroff();
}

fn run() -> Result<(), String> {
    ttys1_ping("fhrun-init: stage-run\n");
    klog("fhrun-init: hello");
    ttys1_ping("fhrun-init: stage-klog\n");

    mount_essentials()?;
    ttys1_ping("fhrun-init: stage-mounts\n");
    redirect_stdio()?;
    ttys1_ping("fhrun-init: stage-stdio\n");

    let spec = load_spec(Path::new(SPEC_PATH))?;
    ttys1_ping("fhrun-init: stage-spec\n");
    klog(&format!(
        "fhrun-init: spec loaded: bin={} argc={} env={}",
        spec.bin,
        spec.args.len(),
        spec.env.len()
    ));

    if let Err(e) = net::bring_up_lo() {
        klog(&format!("fhrun-init: warn: {e}"));
    }
    // Bring each NIC up in declaration order: spec.nics[0] is eth0,
    // spec.nics[1] is eth1, and so on. Only the first NIC with a
    // `gateway` installs a default route; later gateways are ignored,
    // because Linux honors a single default per table.
    let mut default_route_set = false;
    for (idx, nic) in spec.nics.iter().enumerate() {
        let name = format!("eth{idx}");
        let want_default = !default_route_set && nic.gateway.is_some();
        net::configure_iface(&name, nic, want_default)
            .map_err(|e| format!("net {name}: {e}"))?;
        if want_default {
            default_route_set = true;
        }
        klog(&format!(
            "fhrun-init: {name} up {} (role={})",
            nic.ip,
            nic.role.as_deref().unwrap_or("-")
        ));
    }

    // Consoles need no setup: devtmpfs creates the device nodes. They
    // are logged so an operator can match a guest node to the host
    // socket fhrun printed.
    for console in &spec.consoles {
        klog(&format!(
            "fhrun-init: console {} (role={})",
            console.guest_device,
            console.role.as_deref().unwrap_or("-")
        ));
    }

    ttys1_ping("fhrun-init: stage-net\n");
    chdir(&spec.workdir)?;

    // Ping ttyS1 (COM2) right before handing off to the workload. This
    // is a raw write to a tty device init opens itself, not something
    // routed through the kernel's console/printk path, so it stays
    // available for host-side timing even when the guest boots with
    // `console=` dropped from the cmdline for speed.
    signal_ready();

    exec_user(&spec)?;
    // Unreachable: exec_user returns only on failure.
    Err("execve returned without error?".to_string())
}

/// Best-effort readiness ping on /dev/ttyS1 (COM2). Boot never fails on
/// it: a missing device just skips the marker.
fn signal_ready() {
    ttys1_ping("fhrun-init: ready\n");
}

/// Raw, best-effort write of `msg` to /dev/ttyS1 (COM2), for host-side
/// timing markers.
fn ttys1_ping(msg: &str) {
    // The port is opened once and the fd is reused for every marker. A
    // second open() of the still-open port does not deliver, and the fd
    // can never be closed anyway: the 8250 release path drains the TX
    // FIFO, and the virtual UART never signals transmission-complete,
    // so close() would hang here forever. Leaking one fd is harmless
    // for a one-shot init that is about to execve.
    static FD: AtomicI32 = AtomicI32::new(-2);

    let mut fd = FD.load(Ordering::Relaxed);
    if fd == -2 {
        let path = CString::new("/dev/ttyS1").unwrap();
        // O_NONBLOCK matters here: a blocking open() on a serial device
        // waits for DCD before it returns, and the virtual UART never
        // asserts it. Without this flag fhrun-init hangs forever right
        // here, before it reaches exec_user().
        fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_WRONLY | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        FD.store(fd, Ordering::Relaxed);
    }
    if fd < 0 {
        return;
    }
    unsafe {
        libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
    }
}

/// Wire up stdin/stdout/stderr to /dev/console before the payload is
/// execve'd, so its prints land on the serial port the host mirrors.
/// Without this the kernel opens stdio for PID 1 only, and the child
/// inherits raw fds that need not be a tty.
fn redirect_stdio() -> Result<(), String> {
    let path = CString::new("/dev/console").unwrap();
    let fd =
        unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        klog(&format!(
            "fhrun-init: open /dev/console: {e} (skipping redirect)"
        ));
        return Ok(());
    }
    unsafe {
        libc::dup2(fd, 0);
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
        if fd > 2 {
            libc::close(fd);
        }
    }
    Ok(())
}

fn mount_essentials() -> Result<(), String> {
    // /proc and /sys are required for sysrq and interface enumeration.
    // /dev needs devtmpfs so /dev/console and /dev/tty* exist. Init
    // mounts it rather than relying on CONFIG_DEVTMPFS_MOUNT.
    do_mount("proc", "/proc", "proc", 0)?;
    do_mount("sysfs", "/sys", "sysfs", 0)?;
    do_mount("devtmpfs", "/dev", "devtmpfs", 0)?;
    Ok(())
}

fn do_mount(
    src: &str,
    target: &str,
    fstype: &str,
    flags: u64,
) -> Result<(), String> {
    let src_c = CString::new(src).unwrap();
    let target_c = CString::new(target).unwrap();
    let fstype_c = CString::new(fstype).unwrap();
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            target_c.as_ptr(),
            fstype_c.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        // EBUSY on /proc or /sys is benign: the kernel may have mounted
        // it already. Treat it as success.
        if e.raw_os_error() == Some(libc::EBUSY) {
            return Ok(());
        }
        return Err(format!("mount {fstype} on {target}: {e}"));
    }
    Ok(())
}

fn load_spec(path: &Path) -> Result<GuestSpec, String> {
    let raw = std::fs::read(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let s: GuestSpec = serde_json::from_slice(&raw)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(s)
}

fn chdir(dir: &str) -> Result<(), String> {
    let c = CString::new(dir).unwrap();
    let rc = unsafe { libc::chdir(c.as_ptr()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        klog(&format!(
            "fhrun-init: chdir({dir}) failed: {e}; falling back to /"
        ));
        let root = CString::new("/").unwrap();
        unsafe { libc::chdir(root.as_ptr()) };
    }
    Ok(())
}

fn exec_user(spec: &GuestSpec) -> Result<(), String> {
    let bin = CString::new(spec.bin.as_str()).unwrap();
    let basename = Path::new(&spec.bin)
        .file_name()
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_else(|| spec.bin.as_bytes().to_vec());
    let argv0 = CString::new(basename).unwrap();

    // argv as Vec<CString> first, then a NULL-terminated array of
    // pointers into it.
    let mut argv_c: Vec<CString> = Vec::with_capacity(spec.args.len() + 1);
    argv_c.push(argv0);
    for a in &spec.args {
        argv_c.push(
            CString::new(a.as_str()).map_err(|_| "arg has NUL".to_string())?,
        );
    }
    let mut argv_p: Vec<*const libc::c_char> =
        argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());

    let envp_c = build_env(&spec.env)?;
    let mut envp_p: Vec<*const libc::c_char> =
        envp_c.iter().map(|c| c.as_ptr()).collect();
    envp_p.push(std::ptr::null());

    klog(&format!("fhrun-init: execve {}", spec.bin));
    let rc =
        unsafe { libc::execve(bin.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr()) };
    let e = std::io::Error::last_os_error();
    Err(format!("execve {}: rc={rc} err={e}", spec.bin))
}

fn build_env(env: &BTreeMap<String, String>) -> Result<Vec<CString>, String> {
    let mut out = Vec::with_capacity(env.len() + 4);
    for (k, v) in env {
        if k.contains('=') {
            return Err(format!("env key {k} contains '='"));
        }
        let entry = format!("{k}={v}");
        out.push(
            CString::new(entry).map_err(|_| "env contains NUL".to_string())?,
        );
    }
    // Supply a sane default PATH when the manifest gives none.
    if !env.contains_key("PATH") {
        out.push(CString::new("PATH=/usr/local/bin:/usr/bin:/bin").unwrap());
    }
    Ok(out)
}

fn klog(msg: &str) {
    // Best-effort write to /dev/kmsg so the message appears on the
    // serial console, which the VMM mirrors to its stdout.
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg")
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    } else {
        // Fall back to stderr so the message is still visible during
        // early boot, before /dev is wired up.
        eprintln!("{msg}");
    }
}

fn poweroff() -> ! {
    klog("fhrun-init: poweroff");
    // sysrq 'o' is poweroff. It needs CONFIG_MAGIC_SYSRQ and sysrq on.
    let _ = std::fs::write("/proc/sys/kernel/sysrq", "1");
    let _ = std::fs::write("/proc/sysrq-trigger", "o");
    // Fallback: the reboot syscall with LINUX_REBOOT_CMD_POWER_OFF.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // If control still reaches here, spin: the kernel panics when init
    // exits.
    loop {
        unsafe { libc::pause() };
    }
}
