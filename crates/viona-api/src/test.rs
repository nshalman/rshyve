// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the wrappers ask of the kernel.
//!
//! Two kinds of test live here, and neither replaces the other. The
//! recorder reads the command and the argument a wrapper sends, but
//! reaches no kernel. The `/dev/null` handle reaches the kernel and
//! shows only the errno. Together they pin that a wrapper makes a real
//! call, and that it is the call the wrapper's name says.

use std::sync::Mutex;

use super::*;

/// An open fd that is not a viona link.
///
/// The ioctls below reach the kernel and are refused there, so the
/// wrappers are testable off illumos. /dev/null answers every
/// ioctl with ENXIO (`uts/common/io/mem.c`, `mmioctl`).
fn not_a_link() -> VionaFd {
    VionaFd(Chan::Dev(dev_null()))
}

/// A handle that keeps what a wrapper asked of the kernel.
fn recorder() -> VionaFd {
    VionaFd(Chan::Record(dev_null(), Mutex::new(Vec::new())))
}

/// What a recorder was asked for, in order.
fn calls(fd: &VionaFd) -> Vec<Call> {
    match &fd.0 {
        Chan::Record(_, calls) => calls.lock().expect("the calls").clone(),
        Chan::Dev(_) => panic!("only a recorder keeps its calls"),
    }
}

/// The value one recorded call sent the kernel.
fn value_of(call: &Call) -> usize {
    match call {
        Call::Usize { arg, .. } => *arg,
        Call::Typed { cmd } => panic!("cmd {cmd:#x} sent no value"),
    }
}

fn dev_null() -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("every unix has /dev/null")
}

#[test]
fn latest_api_version() {
    let cur = ApiVersion::current();
    assert_eq!(VIONA_CURRENT_INTERFACE_VERSION, cur as u32);
}

#[test]
fn u32_comparisons() {
    assert!(1u32 < ApiVersion::V2);
    assert!(2u32 == ApiVersion::V2);
    assert!(3u32 > ApiVersion::V2);
}

#[test]
fn the_halt_commands_match_the_kernel() {
    // uts/intel/sys/viona_io.h. A number that drifts sends the
    // halt to some other viona entry point, and the vmm_drv hold
    // it must give back stays held.
    assert_eq!(VNA_IOC_DELETE, 0x0056_4302);
    assert_eq!(VNA_IOC_RING_RESET, 0x0056_4311);
}

#[test]
fn every_wrapper_sends_the_command_its_name_says() {
    // A wrapper that names another command reaches another viona
    // entry point and reports success. `delete` is the one that
    // matters most: any other command there leaves viona's vmm_drv
    // hold in place, and VM_DESTROY_SELF then waits for it in an
    // untimed cv_wait.
    let fd = recorder();

    fd.delete().expect("a recorder answers");
    fd.ring_reset(1).expect("a recorder answers");
    fd.ring_kick(1).expect("a recorder answers");
    fd.ring_pause(1).expect("a recorder answers");
    fd.ring_intr_clr(1).expect("a recorder answers");
    fd.set_notify_iop(0x2000).expect("a recorder answers");
    fd.set_promisc(PromiscMode::All)
        .expect("a recorder answers");
    fd.api_version().expect("a recorder answers");

    assert_eq!(
        calls(&fd),
        [
            Call::Usize {
                cmd: VNA_IOC_DELETE,
                arg: 0
            },
            Call::Usize {
                cmd: VNA_IOC_RING_RESET,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_RING_KICK,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_RING_PAUSE,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_RING_INTR_CLR,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_SET_NOTIFY_IOP,
                arg: 0x2000
            },
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: VIONA_PROMISC_ALL as usize
            },
            Call::Usize {
                cmd: VNA_IOC_VERSION,
                arg: 0
            },
        ],
    );
}

#[test]
fn every_struct_wrapper_sends_the_command_its_name_says() {
    // The other half of the boundary. A recorder copies nothing back,
    // so these say which viona entry point each wrapper reached and
    // nothing about the struct it filled in. A wrapper that names
    // another command still reports success to its caller.
    let fd = recorder();

    fd.ring_init_modern(1, 256, 0x1000, 0x2000, 0x3000)
        .expect("a recorder answers");
    fd.ring_set_state(&vioc_ring_state::default())
        .expect("a recorder answers");
    fd.ring_get_state(1).expect("a recorder answers");
    fd.ring_set_msi(1, 0xFEE0_0000, 0x21)
        .expect("a recorder answers");
    fd.intr_status().expect("a recorder answers");
    fd.set_features(1).expect("a recorder answers");
    fd.get_features().expect("a recorder answers");
    fd.set_notify_mmio(0xC000_0000, 0x1000)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd),
        [
            Call::Typed {
                cmd: VNA_IOC_RING_INIT_MODERN
            },
            Call::Typed {
                cmd: VNA_IOC_RING_SET_STATE
            },
            Call::Typed {
                cmd: VNA_IOC_RING_GET_STATE
            },
            Call::Typed {
                cmd: VNA_IOC_RING_SET_MSI
            },
            Call::Typed {
                cmd: VNA_IOC_INTR_POLL
            },
            Call::Typed {
                cmd: VNA_IOC_SET_FEATURES
            },
            Call::Typed {
                cmd: VNA_IOC_GET_FEATURES
            },
            Call::Typed {
                cmd: VNA_IOC_SET_NOTIFY_MMIO
            },
        ],
    );
}

#[test]
fn the_promiscuous_mode_goes_by_value() {
    // viona_ioc_set_promisc casts the data argument straight to its
    // enum. A wrapper that passed a pointer would set the mode to a
    // userspace address, and the kernel would refuse the whole call.
    // A guest allowed to spoof its MAC then gets no traffic.
    let fd = recorder();

    fd.set_promisc(PromiscMode::None)
        .expect("a recorder answers");
    fd.set_promisc(PromiscMode::Multi)
        .expect("a recorder answers");
    fd.set_promisc(PromiscMode::All)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd),
        [
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: 0
            },
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: 2
            },
        ],
    );
}

#[test]
fn every_ring_wrapper_sends_the_ring_it_was_given() {
    // A wrapper that drops its argument works on ring 0 whatever the
    // caller asks for. The halt would then leave every other ring
    // running, and the delete would wait for each of them.
    for ring in [0usize, 1, 7, usize::MAX] {
        let fd = recorder();

        fd.ring_reset(ring).expect("a recorder answers");
        fd.ring_kick(ring).expect("a recorder answers");
        fd.ring_pause(ring).expect("a recorder answers");
        fd.ring_intr_clr(ring).expect("a recorder answers");

        let args: Vec<usize> = calls(&fd).iter().map(value_of).collect();
        assert_eq!(args, [ring; 4], "a ring wrapper dropped ring {ring}");
    }
}

#[test]
fn the_halt_commands_may_take_a_usize() {
    // delete() and ring_reset() go through ioctl_usize. A cmd the
    // allowlist misses is refused before the kernel sees it, and
    // the halt would then leave viona's hold on the VM.
    assert!(VionaFd::ioctl_usize_safe(VNA_IOC_DELETE));
    assert!(VionaFd::ioctl_usize_safe(VNA_IOC_RING_RESET));
}

#[test]
fn no_command_that_copies_in_may_take_a_usize() {
    // ioctl_usize hands its argument to the kernel as the data
    // pointer. A cmd that copies in would read the usize as a
    // user address of the caller's choice.
    for cmd in [
        VNA_IOC_CREATE,
        VNA_IOC_DEFAULT_PARAMS,
        VNA_IOC_RING_INIT,
        VNA_IOC_RING_INIT_MODERN,
        VNA_IOC_RING_SET_MSI,
        VNA_IOC_RING_SET_STATE,
        VNA_IOC_RING_GET_STATE,
        VNA_IOC_INTR_POLL,
        VNA_IOC_INTR_POLL_MQ,
        VNA_IOC_SET_FEATURES,
        VNA_IOC_GET_FEATURES,
        VNA_IOC_SET_NOTIFY_MMIO,
        VNA_IOC_GET_PARAMS,
        VNA_IOC_SET_PARAMS,
    ] {
        assert!(
            !VionaFd::ioctl_usize_safe(cmd),
            "cmd {cmd:#x} copies in through its data argument",
        );
    }
}

#[test]
fn a_command_that_copies_in_is_refused_before_it_is_recorded() {
    // The allowlist has to hold at the call, not only in the const fn
    // that lists it, and it has to hold before anything else looks at
    // the command.
    let fd = recorder();

    let err = fd
        .ioctl_usize(VNA_IOC_RING_SET_STATE, 0x4141_4141)
        .expect_err("a copyin cmd may not take a usize");

    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert!(calls(&fd).is_empty(), "a refused cmd reached the channel");
}

#[test]
fn a_command_that_copies_in_is_refused_at_the_kernel_call() {
    let err = not_a_link()
        .ioctl_usize(VNA_IOC_RING_SET_STATE, 0x4141_4141)
        .expect_err("a copyin cmd may not take a usize");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn a_failed_delete_reports_it() {
    // The halt calls this and carries on to VM_DESTROY_SELF. A
    // failure it swallowed would hide the destroy wedge that
    // follows.
    let err = not_a_link().delete().expect_err("/dev/null holds no link");
    assert_ne!(
        err.kind(),
        ErrorKind::InvalidInput,
        "the delete never reached the ioctl",
    );
}

#[test]
fn a_failed_ring_reset_reports_it() {
    // The halt resets every ring before the delete and reports
    // what fails. An out of range ring is the kernel's to refuse.
    let fd = not_a_link();
    for ring in [0, 1, usize::MAX] {
        let err = fd.ring_reset(ring).expect_err("/dev/null holds no link");
        assert_ne!(
            err.kind(),
            ErrorKind::InvalidInput,
            "ring {ring} never reached the ioctl",
        );
    }
}
