// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What each link call sends the kernel.
//!
//! Two things are pinned for every method: the command it names, and
//! the argument it passes in each position. Neither is visible from
//! the errno, so a call that names another entry point, or that swaps
//! two fields of its payload, answers exactly like the right one. That
//! is what the recording channel is for.
//!
//! The other half of the boundary is the `/dev/null` handle. It
//! reaches the kernel and shows only the errno, which is enough to say
//! that a call still makes a real ioctl.

use std::fs::File;
use std::io::{ErrorKind, PipeReader, Write};
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::test::{
    answers_with, calls, cmds, not_a_link, recorder, recorder_on,
};
use crate::{Call, VIONA_PROMISC_ALL};

/// A wait this long means a thread is wedged, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// How long a wait that must not end is watched.
///
/// A correct wait parks in `poll(2)` with no deadline, so this only
/// bounds how long the test is willing to watch it park.
const PARKED: Duration = Duration::from_millis(200);

/// A recorder whose descriptor is one the test can make ready.
///
/// A pipe read end never reports `POLLRDBAND`, which is the band viona
/// signals interrupts on, so a wait on it ends only the way the test
/// arranges.
fn link_on(reader: PipeReader) -> VionaFd {
    recorder_on(File::from(OwnedFd::from(reader)))
}

/// One wait, running on its own thread.
struct Waiting {
    /// What the wait ended with, once it ends.
    end: mpsc::Receiver<Result<IntrWait>>,
    /// The thread it runs on, for a test that signals it.
    thread: Tid,
}

/// A thread id a test can carry across a channel.
///
/// `pthread_t` is a pointer on macOS, so it is not `Send` on its own.
#[derive(Clone, Copy)]
struct Tid(libc::pthread_t);

// Safety: the value only names a thread, and every test that holds
// one waits for that thread to end before it drops the id.
unsafe impl Send for Tid {}

/// Run one wait on its own thread.
///
/// A wait that never ends is a failure, not a hung run, so no test
/// below calls it on the test thread.
fn spawn_wait(fd: VionaFd, stop: PipeReader) -> Waiting {
    let (tx, end) = mpsc::channel();
    let (named, id) = mpsc::channel();
    std::thread::Builder::new()
        .name("wait-intr".into())
        .spawn(move || {
            // Safety: pthread_self names the calling thread.
            let me = Tid(unsafe { libc::pthread_self() });
            named.send(me).expect("the test reads this");
            tx.send(fd.wait_intr(&stop))
                .expect("the test waits for this");
        })
        .expect("the test can spawn a thread");
    let thread = id.recv_timeout(WEDGED).expect("the thread named itself");
    Waiting { end, thread }
}

/// Whether this platform tells viona's interrupt band apart from
/// ordinary readability.
///
/// viona signals a pending interrupt in a priority band. macOS maps
/// every read-side poll bit onto one select filter, so a readable pipe
/// reports `POLLRDBAND` there and no descriptor can tell the two
/// apart. illumos and Linux keep them apart, and those are the two
/// platforms this runs on for real, so a false answer there is a
/// failure and not a reason to skip.
fn tells_a_band_from_a_byte() -> bool {
    let (rx, mut tx) = std::io::pipe().expect("a pipe");
    tx.write_all(b"x").expect("the pipe takes a byte");
    let mut pfd = libc::pollfd {
        fd: rx.as_raw_fd(),
        events: libc::POLLRDBAND,
        revents: 0,
    };
    // Safety: one descriptor, open for this call, and the count
    // matches the array.
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    assert!(ret >= 0, "poll: {}", std::io::Error::last_os_error());

    let apart = pfd.revents & libc::POLLRDBAND == 0;
    assert!(
        apart || !cfg!(any(target_os = "illumos", target_os = "linux")),
        "this platform no longer keeps the poll bands apart, so the \
         wait cannot tell an interrupt from a byte",
    );
    apart
}

/// What one spawned wait ended with.
fn waited(waiting: &Waiting) -> IntrWait {
    waiting
        .end
        .recv_timeout(WEDGED)
        .expect("the wait ended")
        .expect("the poll answered")
}

/// Make `SIGUSR1` interrupt a system call on this process.
///
/// `SA_RESTART` is left out on purpose: with it the C library
/// restarts the call, and the retry under test never runs. The
/// handler does nothing, so the only effect is the `EINTR`.
fn sigusr1_interrupts_a_call() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    extern "C" fn note(_sig: libc::c_int) {}

    INSTALLED.call_once(|| {
        // Safety: the struct is initialized before the call, and the
        // handler is a plain function that touches nothing.
        let rc = unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction =
                note as extern "C" fn(libc::c_int) as libc::sighandler_t;
            act.sa_flags = 0;
            libc::sigemptyset(&mut act.sa_mask);
            libc::sigaction(libc::SIGUSR1, &act, std::ptr::null_mut())
        };
        assert_eq!(rc, 0, "sigaction: {}", std::io::Error::last_os_error());
    });
}

#[test]
fn every_link_call_sends_the_command_its_name_says() {
    // A call that names another command reaches another viona entry
    // point and reports success. `delete` is the one that matters
    // most: any other command there leaves viona's vmm_drv hold in
    // place, and VM_DESTROY_SELF then waits for it in an untimed
    // cv_wait.
    let fd = recorder();

    fd.ring_reset(1).expect("a recorder answers");
    fd.ring_kick(1).expect("a recorder answers");
    fd.ring_pause(1).expect("a recorder answers");
    fd.ring_intr_clr(1).expect("a recorder answers");
    fd.set_notify_iop(&Quiesced::for_recorder(&fd), 0x2000)
        .expect("a recorder answers");
    fd.set_promisc(PromiscMode::All)
        .expect("a recorder answers");
    fd.delete().expect("a recorder answers");

    assert_eq!(
        calls(&fd),
        [
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
                cmd: VNA_IOC_DELETE,
                arg: 0
            },
        ],
    );
}

#[test]
fn every_link_call_that_takes_a_struct_sends_the_command_its_name_says() {
    // The commands that copy a payload. Which entry point each one
    // reached, in order; what it put in the payload is pinned one
    // test per command below.
    let fd = recorder();

    fd.ring_init(1, 256, 0x1000, 0x2000, 0x3000)
        .expect("a recorder answers");
    fd.ring_set_state(&vioc_ring_state::default())
        .expect("a recorder answers");
    fd.ring_get_state(1).expect("a recorder answers");
    fd.ring_set_msi(1, 0xFEE0_0000, 0x21)
        .expect("a recorder answers");
    fd.intr_status().expect("a recorder answers");
    fd.set_features(1).expect("a recorder answers");
    fd.set_notify_mmio(&Quiesced::for_recorder(&fd), 0xC000_0000, 0x1000)
        .expect("a recorder answers");

    assert_eq!(
        cmds(&fd),
        [
            VNA_IOC_RING_INIT_MODERN,
            VNA_IOC_RING_SET_STATE,
            VNA_IOC_RING_GET_STATE,
            VNA_IOC_RING_SET_MSI,
            VNA_IOC_INTR_POLL,
            VNA_IOC_SET_FEATURES,
            VNA_IOC_SET_NOTIFY_MMIO,
        ],
    );
}

#[test]
fn every_ring_call_sends_the_ring_it_was_given() {
    // A call that drops its argument works on ring 0 whatever the
    // caller asks for. The halt would then leave every other ring
    // running, and the delete would wait for each of them.
    for ring in [0u16, 1, 7, u16::MAX] {
        let fd = recorder();

        fd.ring_reset(ring).expect("a recorder answers");
        fd.ring_kick(ring).expect("a recorder answers");
        fd.ring_pause(ring).expect("a recorder answers");
        fd.ring_intr_clr(ring).expect("a recorder answers");

        let args: Vec<usize> = calls(&fd).iter().map(Call::value).collect();
        assert_eq!(args, [ring as usize; 4], "a call dropped ring {ring}");
    }
}

#[test]
fn a_ring_init_sends_the_addresses_the_guest_programmed() {
    // The three addresses are what the in-kernel worker reads. A pair
    // that swaps points the worker at the wrong half of the guest's
    // ring: it would take the used ring for the available ring and
    // write completions over entries the driver has not read.
    let fd = recorder();

    fd.ring_init(1, 256, 0x1_0000, 0x2_0000, 0x3_0000)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_ring_init_modern>(),
        vioc_ring_init_modern {
            rim_index: 1,
            rim_qsize: 256,
            rim_qaddr_desc: 0x1_0000,
            rim_qaddr_avail: 0x2_0000,
            rim_qaddr_used: 0x3_0000,
            _pad: [0; 2],
        },
    );
}

#[test]
fn a_ring_state_write_sends_the_state_it_was_given() {
    // A migration destination resumes from this. Every field is
    // distinct, so a payload that reorders any two of them is visible
    // here rather than in a guest that replays descriptors.
    let fd = recorder();
    let state = vioc_ring_state {
        vrs_index: 1,
        vrs_avail_idx: 7,
        vrs_used_idx: 5,
        vrs_qsize: 256,
        vrs_qaddr_desc: 0x1_0000,
        vrs_qaddr_avail: 0x2_0000,
        vrs_qaddr_used: 0x3_0000,
    };

    fd.ring_set_state(&state).expect("a recorder answers");

    assert_eq!(calls(&fd)[0].payload::<vioc_ring_state>(), state);
}

#[test]
fn a_ring_state_read_asks_for_the_ring_it_was_given() {
    // The index is the only field this sends. A call that drops it
    // reads ring 0 whatever the caller asks for, and a migration
    // source then writes ring 0's indices into every ring's payload.
    let fd = recorder();
    let filled = vioc_ring_state {
        vrs_index: 1,
        vrs_avail_idx: 7,
        vrs_used_idx: 5,
        vrs_qsize: 256,
        vrs_qaddr_desc: 0x1_0000,
        vrs_qaddr_avail: 0x2_0000,
        vrs_qaddr_used: 0x3_0000,
    };
    answers_with(&fd, VNA_IOC_RING_GET_STATE, filled);

    let state = fd.ring_get_state(1).expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_ring_state>(),
        vioc_ring_state {
            vrs_index: 1,
            ..Default::default()
        },
    );
    // The indices are the kernel's to fill, and they are what the
    // migration payload carries. A read that answered with the struct
    // it sent would report every ring at 0, and the destination would
    // replay descriptors the guest has already reclaimed.
    assert_eq!(state, filled);
}

#[test]
fn a_ring_msi_sends_the_message_where_the_driver_asked() {
    // The kernel writes this message itself. An address and a data
    // word that swap send the interrupt to the data word as an
    // address, which reaches no vector the driver armed.
    let fd = recorder();

    fd.ring_set_msi(1, 0xFEE0_0000, 0x21)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_ring_msi>(),
        vioc_ring_msi {
            rm_index: 1,
            rm_addr: 0xFEE0_0000,
            rm_msg: 0x21,
            _pad: [0; 3],
        },
    );
}

#[test]
fn a_feature_write_sends_the_features_the_guest_accepted() {
    // VERSION_1 is the bit that puts the kernel rings in the modern
    // layout. A call that sends 0 leaves the kernel reading a legacy
    // ring at addresses the guest programmed for a modern one.
    let fd = recorder();
    let features = (1u64 << 32) | 0x30;

    fd.set_features(features).expect("a recorder answers");

    assert_eq!(calls(&fd)[0].payload::<u64>(), features);
}

#[test]
fn a_notify_window_sends_its_address_and_its_size() {
    // Both halves matter: the kernel takes the guest's kick only for
    // writes inside this window, so a size of 0 sends every kick back
    // out to userspace.
    let fd = recorder();

    fd.set_notify_mmio(&Quiesced::for_recorder(&fd), 0xC000_0000, 0x1000)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_notify_mmio>(),
        vioc_notify_mmio {
            vim_address: 0xC000_0000,
            vim_size: 0x1000,
            _pad: 0,
        },
    );
}

#[test]
fn an_interrupt_poll_reads_the_pending_rings() {
    // The payload is what the kernel copies out, so it goes in empty
    // and the answer comes back in it. A poll that sent a stale
    // status would report rings pending that the kernel never
    // signalled.
    //
    // The answer is the whole point of the call. A read that gave
    // back an empty status would leave the poll thread finding no
    // ring pending on every wakeup: nothing clears the kernel's
    // signal, the band stays asserted, the wait returns at once for
    // ever, and one core spins while the guest waits for an interrupt
    // that never comes.
    let fd = recorder();
    // The tx ring alone, so a status that answers by position is
    // told apart from one that answers at all.
    let pending = vioc_intr_poll { vip_status: [0, 1] };
    answers_with(&fd, VNA_IOC_INTR_POLL, pending);

    let status = fd.intr_status().expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_intr_poll>(),
        vioc_intr_poll::default(),
    );
    assert_eq!(status, pending.vip_status);
}

#[test]
fn the_promiscuous_mode_goes_by_value() {
    // viona_ioc_set_promisc casts the data argument straight to its
    // enum. A call that passed a pointer would set the mode to a
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
fn each_link_call_sends_exactly_one_ioctl() {
    // A call that also reset a ring, or that asked the kernel twice,
    // leaves a second entry here. The device counts on one call
    // reaching one viona entry point: the halt's own order depends on
    // it, and so does every step a test reads back.
    let fd = recorder();

    fd.ring_init(0, 8, 1, 2, 3).expect("a recorder answers");
    fd.ring_kick(0).expect("a recorder answers");
    fd.ring_pause(0).expect("a recorder answers");
    fd.ring_reset(0).expect("a recorder answers");
    fd.ring_intr_clr(0).expect("a recorder answers");
    fd.ring_set_state(&vioc_ring_state::default())
        .expect("a recorder answers");
    fd.ring_get_state(0).expect("a recorder answers");
    fd.ring_set_msi(0, 1, 2).expect("a recorder answers");
    fd.set_features(1).expect("a recorder answers");
    fd.intr_status().expect("a recorder answers");
    fd.set_notify_iop(&Quiesced::for_recorder(&fd), 1)
        .expect("a recorder answers");
    fd.set_notify_mmio(&Quiesced::for_recorder(&fd), 1, 2)
        .expect("a recorder answers");
    fd.set_promisc(PromiscMode::None)
        .expect("a recorder answers");
    fd.delete().expect("a recorder answers");

    assert_eq!(calls(&fd).len(), 14, "a link call sent more than one ioctl");
}

#[test]
fn every_link_call_reports_what_the_kernel_refused() {
    // /dev/null is not a link, so every one of these is refused at the
    // ioctl. A call that dropped the result would report success to a
    // device that then treats the ring as programmed.
    let link = not_a_link();
    let outcomes = [
        ("ring_init", link.ring_init(0, 8, 1, 2, 3).is_err()),
        ("ring_kick", link.ring_kick(0).is_err()),
        ("ring_reset", link.ring_reset(0).is_err()),
        ("ring_pause", link.ring_pause(0).is_err()),
        (
            "ring_set_state",
            link.ring_set_state(&vioc_ring_state::default()).is_err(),
        ),
        ("ring_get_state", link.ring_get_state(0).is_err()),
        ("ring_set_msi", link.ring_set_msi(0, 1, 2).is_err()),
        ("set_features", link.set_features(1).is_err()),
        ("intr_status", link.intr_status().is_err()),
        ("ring_intr_clr", link.ring_intr_clr(0).is_err()),
        (
            "set_notify_iop",
            link.set_notify_iop(&Quiesced::for_recorder(&link), 1)
                .is_err(),
        ),
        (
            "set_notify_mmio",
            link.set_notify_mmio(&Quiesced::for_recorder(&link), 1, 2)
                .is_err(),
        ),
        ("set_promisc", link.set_promisc(PromiscMode::All).is_err()),
        ("delete", link.delete().is_err()),
    ];

    for (name, failed) in outcomes {
        assert!(failed, "{name} swallowed what the kernel refused");
    }
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
    for ring in [0, 1, u16::MAX] {
        let err = fd.ring_reset(ring).expect_err("/dev/null holds no link");
        assert_ne!(
            err.kind(),
            ErrorKind::InvalidInput,
            "ring {ring} never reached the ioctl",
        );
    }
}

#[test]
fn one_wakeup_is_read_the_way_the_poll_thread_needs_it() {
    // The whole decision the wait makes, one row at a time.
    //
    // The stop descriptor comes first and on its own. The halt closes
    // the write end, so it is ready and stays ready: a wakeup read as
    // pending there would send the poll thread back to a link the
    // delete is about to destroy, and the halt would join a thread
    // that never leaves.
    //
    // POLLRDBAND is the band viona signals on. Ordinary readability is
    // not it. An end that is not a signal (the link hung up, the
    // descriptor is gone) stops the thread instead of spinning it:
    // none of those states clears on its own.
    //
    // The two rows that answer None are events the wait never asks
    // for, so it can only meet them through a mistake. The caller
    // reports them rather than waiting again on a descriptor that is
    // ready and stays ready.
    let stopped = Some(IntrWait::Stopped);
    let pending = Some(IntrWait::Pending);
    let table = [
        // link revents, stop revents, verdict
        (0, libc::POLLIN, stopped),
        (libc::POLLRDBAND, libc::POLLIN, stopped),
        (libc::POLLRDBAND, 0, pending),
        (libc::POLLRDBAND | libc::POLLHUP, 0, pending),
        (libc::POLLHUP, 0, stopped),
        (libc::POLLERR, 0, stopped),
        (libc::POLLNVAL, 0, stopped),
        (libc::POLLIN, 0, None),
        (0, 0, None),
    ];

    for (link, stop, want) in table {
        assert_eq!(
            poll_verdict(link, stop),
            want,
            "link {link:#x} and stop {stop:#x} were read wrong",
        );
    }
}

#[test]
fn the_wait_ends_when_the_halt_closes_the_stop_pipe() {
    // The halt closes the write end and joins the poll thread. A wait
    // that read that as pending would go back to the link, and the
    // halt would wait for a thread that never leaves.
    //
    // Where the platform allows it the link carries ordinary data, so
    // a wait that ends here has to end for the stop descriptor and not
    // for the link. macOS reports that byte in the interrupt band, and
    // would end the wait for it before the stop pipe ever closed, so
    // there the link is left idle and this reads only the wake.
    let (link_rx, mut link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, stop_tx) = std::io::pipe().expect("a pipe");
    if tells_a_band_from_a_byte() {
        link_tx.write_all(b"x").expect("the pipe takes a byte");
    }

    let waiting = spawn_wait(link_on(link_rx), stop_rx);
    drop(stop_tx);

    assert_eq!(waited(&waiting), IntrWait::Stopped);
}

#[test]
fn the_wait_ends_when_the_stop_pipe_carries_a_byte() {
    // The halt ends this wait by closing the write end, and a closed
    // end reports POLLHUP whatever the wait asked for. So the wait
    // must ask for readability on the stop descriptor as well: the
    // day anything signals the stop by writing it, a wait that asked
    // for nothing sleeps through the write, the poll thread stays
    // parked, and the halt joins it for ever.
    //
    // The link write end is held open, so nothing but the byte can
    // end this wait.
    let (link_rx, link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, mut stop_tx) = std::io::pipe().expect("a pipe");

    let waiting = spawn_wait(link_on(link_rx), stop_rx);
    stop_tx.write_all(b"x").expect("the pipe takes a byte");

    assert_eq!(waited(&waiting), IntrWait::Stopped);
    drop(link_tx);
}

#[test]
fn a_signal_does_not_end_the_wait() {
    // poll(2) gives up with EINTR when a signal lands on this thread,
    // and the kernel's interrupt is still pending after it. A wait
    // that reported that would end the poll thread on the first
    // signal the process takes: the link stays up, the halt still
    // joins a thread that already left, and the guest loses every
    // interrupt from then on.
    //
    // Both write ends stay open, so only the signal can reach the
    // wait until the test closes the stop pipe.
    let (link_rx, link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, stop_tx) = std::io::pipe().expect("a pipe");
    sigusr1_interrupts_a_call();

    let waiting = spawn_wait(link_on(link_rx), stop_rx);
    // The thread parks in poll(2) for the whole run below, so the
    // signals land there. One is enough to end a wait that gives up.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(10));
        // Safety: the thread named itself above, and the assertion
        // below reads the answer that says whether it is still there.
        let rc = unsafe { libc::pthread_kill(waiting.thread.0, libc::SIGUSR1) };
        // The thread sends its result before it leaves, so a thread
        // that is gone has already said why.
        assert_eq!(
            rc,
            0,
            "the wait thread left the run of signals, ending with {:?}",
            waiting.end.try_recv(),
        );
    }

    match waiting.end.recv_timeout(PARKED) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        other => panic!("a signal ended the wait with {other:?}"),
    }

    // Released either way, so the thread is never left parked.
    drop(stop_tx);
    assert_eq!(waited(&waiting), IntrWait::Stopped);
    drop(link_tx);
}

#[test]
fn the_wait_ends_when_the_link_hangs_up() {
    // A link whose write end is gone signals POLLHUP and nothing
    // else, for ever. Reading that as anything but an end spins one
    // core until the halt runs.
    if !tells_a_band_from_a_byte() {
        return;
    }
    let (link_rx, link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, _stop_tx) = std::io::pipe().expect("a pipe");
    drop(link_tx);

    let waiting = spawn_wait(link_on(link_rx), stop_rx);

    assert_eq!(waited(&waiting), IntrWait::Stopped);
}

#[test]
fn a_readable_link_with_no_interrupt_does_not_end_the_wait() {
    // viona signals interrupts in a priority band, not with ordinary
    // readability. A wait that watched for POLLIN would return with
    // nothing pending, and the poll thread would ask the kernel for a
    // status on every byte instead of on every interrupt.
    if !tells_a_band_from_a_byte() {
        return;
    }
    let (link_rx, mut link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, stop_tx) = std::io::pipe().expect("a pipe");
    link_tx.write_all(b"x").expect("the pipe takes a byte");

    let waiting = spawn_wait(link_on(link_rx), stop_rx);

    match waiting.end.recv_timeout(PARKED) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        other => panic!("a readable link ended the wait with {other:?}"),
    }

    // Released either way, so the thread is never left parked.
    drop(stop_tx);
    assert_eq!(waited(&waiting), IntrWait::Stopped);
}

#[test]
fn the_public_ioctl_calls_refuse_the_hook_commands() {
    // A hook change takes the VM write lock, so only the LinkOps calls
    // that ask for stopped rings may send one.
    let fd = recorder();
    let mut vim = vioc_notify_mmio::default();

    let iop = fd.ioctl_usize(VNA_IOC_SET_NOTIFY_IOP, 0x2000);
    // Safety: the payload is one vioc_notify_mmio.
    let mmio = unsafe { fd.ioctl(VNA_IOC_SET_NOTIFY_MMIO, &mut vim) };

    assert_eq!(iop.unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(mmio.unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(calls(&fd), []);
}

#[test]
fn a_hook_change_refuses_a_proof_for_another_link() {
    let fd = recorder();
    let other = recorder();

    let rc = fd.set_notify_iop(&Quiesced::for_recorder(&other), 0x2000);

    assert_eq!(rc.unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(calls(&fd), []);
}

#[test]
fn removing_the_notify_window_sends_a_null_payload() {
    // The kernel removes the window only for a null pointer. A payload
    // with address 0 would hook a window at guest address 0.
    let fd = recorder();

    fd.set_notify_mmio(&Quiesced::for_recorder(&fd), 0, 0x1000)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd),
        [Call::Usize {
            cmd: VNA_IOC_SET_NOTIFY_MMIO,
            arg: 0
        }],
    );
}
