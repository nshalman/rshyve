// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use vmm_devices::audio::AudioSink;
use vmm_devices::lifecycle::{SoundMigrateState, VirtioDeviceMigrateState};
use zerocopy::{FromBytes, IntoBytes};

use super::*;
use crate::bits;
use crate::queue::VirtqDesc;
use crate::testlog::CountingDrain;

const MEM_GPA: u64 = 0x10_0000;
const MEM_LEN: usize = 0x20_0000;
const REQ_GPA: u64 = MEM_GPA;
const RESP_GPA: u64 = MEM_GPA + 0x1000;
const DESC_GPA: u64 = MEM_GPA + 0x2000;
const AVAIL_GPA: u64 = MEM_GPA + 0x3000;
const USED_GPA: u64 = MEM_GPA + 0x4000;
/// Message buffers: one PCM payload and one status per slot.
const DATA_GPA: u64 = MEM_GPA + 0x10_0000;
const SLOT: u64 = 0x1_0000;
const RING: u16 = 16;
/// An address past the end of guest memory.
const OUTSIDE: u64 = MEM_GPA + MEM_LEN as u64 + 0x1000;

/// 100 ms of 48 kHz stereo 16-bit.
const TENTH: u32 = 19_200;

#[derive(Default)]
struct Recorder {
    heard: Mutex<Vec<u8>>,
    /// When each piece of it came.
    times: Mutex<Vec<Instant>>,
}

impl AudioSink for Recorder {
    fn play(&self, pcm: &[u8]) {
        self.times.lock().unwrap().push(Instant::now());
        self.heard.lock().unwrap().extend_from_slice(pcm);
    }
}

/// Where message `slot`'s status goes.
fn status_gpa(slot: u64) -> u64 {
    DATA_GPA + slot * SLOT + SLOT - 16
}

struct Guest {
    physmap: Arc<PhysMap>,
    dev: VirtioSound,
    sink: Arc<Recorder>,
    queues: Vec<VirtQueue>,
    posted: u16,
}

impl Guest {
    fn new() -> Self {
        Self::logged(slog::Logger::root(slog::Discard, slog::o!()))
    }

    /// A guest whose device logs to `log`.
    fn logged(log: slog::Logger) -> Self {
        let physmap = Arc::new(
            PhysMap::new_anon(MEM_GPA, MEM_LEN).expect("guest memory"),
        );
        let audio = Arc::new(AudioBroker::default());
        let sink = Arc::new(Recorder::default());
        audio.set_sink(sink.clone());
        let dev =
            VirtioSound::new(audio, Arc::clone(&physmap), log).expect("device");
        let mut tx = VirtQueue::new(RING);
        tx.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
        let queues = vec![
            VirtQueue::new(RING),
            VirtQueue::new(RING),
            tx,
            VirtQueue::new(RING),
        ];
        Self {
            physmap,
            dev,
            sink,
            queues,
            posted: 0,
        }
    }

    fn write(&self, gpa: u64, bytes: &[u8]) {
        self.physmap
            .lookup(gpa, bytes.len())
            .unwrap()
            .write_bytes(bytes)
            .unwrap();
    }

    fn read(&self, gpa: u64, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        self.physmap
            .lookup(gpa, len)
            .unwrap()
            .read_bytes(&mut b)
            .unwrap();
        b
    }

    /// One control request; the response bytes the device wrote.
    fn ctrl(&self, req: &[u8], resp_len: u32) -> Vec<u8> {
        self.write(REQ_GPA, req);
        let bufs = [
            ChainBuf::Readable {
                addr: REQ_GPA,
                len: req.len() as u32,
            },
            ChainBuf::Writable {
                addr: RESP_GPA,
                len: resp_len,
            },
        ];
        let n = self.dev.handle_ctrl(&bufs, &self.physmap);
        self.read(RESP_GPA, n as usize)
    }

    fn status(&self, req: &[u8]) -> u32 {
        let resp = self.ctrl(req, 4);
        u32::from_le_bytes(resp[..4].try_into().expect("a status"))
    }

    fn pcm(&self, code: u32) -> u32 {
        let mut req = Hdr { code }.as_bytes().to_vec();
        req.extend_from_slice(PcmHdr { stream_id: 0 }.as_bytes());
        self.status(&req)
    }

    fn set_params(
        &self,
        stream_id: u32,
        channels: u8,
        format: u8,
        rate: u8,
    ) -> u32 {
        self.set_params_sized(
            stream_id,
            channels,
            format,
            rate,
            TENTH * 4,
            TENTH,
        )
    }

    /// SET_PARAMS with a buffer and period of the given sizes.
    fn set_params_sized(
        &self,
        stream_id: u32,
        channels: u8,
        format: u8,
        rate: u8,
        buffer_bytes: u32,
        period_bytes: u32,
    ) -> u32 {
        let mut req = Hdr {
            code: R_PCM_SET_PARAMS,
        }
        .as_bytes()
        .to_vec();
        req.extend_from_slice(PcmHdr { stream_id }.as_bytes());
        req.extend_from_slice(
            PcmSetParams {
                buffer_bytes,
                period_bytes,
                features: 0,
                channels,
                format,
                rate,
                padding: 0,
            }
            .as_bytes(),
        );
        self.status(&req)
    }

    /// SET_PARAMS, PREPARE and START, as the Linux driver does.
    fn started(&self) {
        assert_eq!(self.set_params(0, 2, PCM_FMT_S16, PCM_RATE_48000), S_OK);
        assert_eq!(self.pcm(R_PCM_PREPARE), S_OK);
        assert_eq!(self.pcm(R_PCM_START), S_OK);
    }

    /// Post one I/O message of `bytes` PCM bytes, each the value `fill`,
    /// and notify the transmit queue.
    fn play(&mut self, bytes: u32, fill: u8) {
        let status = status_gpa(u64::from(self.posted));
        self.write(status, &[0xAA; 8]);
        self.post(0, bytes, fill, Some(status));
    }

    /// Post one I/O message for `stream_id`, with its status at `status`
    /// or with no writable buffer at all.
    fn post(
        &mut self,
        stream_id: u32,
        bytes: u32,
        fill: u8,
        status: Option<u64>,
    ) {
        let slot = u64::from(self.posted);
        let data = DATA_GPA + slot * SLOT;
        let mut msg = PcmXfer { stream_id }.as_bytes().to_vec();
        msg.extend(std::iter::repeat_n(fill, bytes as usize));
        self.write(data, &msg);
        let d0 = self.posted * 2;
        let mut descs = vec![VirtqDesc {
            addr: data,
            len: msg.len() as u32,
            flags: 0,
            next: 0,
        }];
        if let Some(status) = status {
            descs[0].flags = bits::VRING_DESC_F_NEXT;
            descs[0].next = d0 + 1;
            descs.push(VirtqDesc {
                addr: status,
                len: 8,
                flags: bits::VRING_DESC_F_WRITE,
                next: 0,
            });
        }
        for (i, desc) in descs.iter().enumerate() {
            self.physmap
                .lookup(DESC_GPA + (u64::from(d0) + i as u64) * 16, 16)
                .unwrap()
                .write(desc)
                .unwrap();
        }
        self.write(
            AVAIL_GPA + 4 + u64::from(self.posted) * 2,
            &d0.to_le_bytes(),
        );
        self.posted += 1;
        self.write(AVAIL_GPA + 2, &self.posted.to_le_bytes());
        self.dev
            .notify_queue(VQ_TX, &mut self.queues, &self.physmap);
    }

    /// The length the device put in used ring entry `n`.
    fn used_len(&self, n: u64) -> u32 {
        let entry = USED_GPA + 4 + n * 8;
        u32::from_le_bytes(self.read(entry + 4, 4).try_into().unwrap())
    }

    /// Messages the device has handed back.
    fn completed(&self) -> u16 {
        u16::from_le_bytes(self.read(USED_GPA + 2, 2).try_into().unwrap())
    }

    /// The status the device wrote into message `slot`.
    fn message_status(&self, slot: u64) -> PcmStatus {
        let status = status_gpa(slot);
        PcmStatus::read_from_bytes(&self.read(status, 8)).unwrap()
    }

    /// Wait up to `limit` for the sink to have `n` bytes; how long it
    /// took.
    fn wait_heard(&self, n: usize, limit: Duration) -> Option<Duration> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if self.sink.heard.lock().unwrap().len() >= n {
                return Some(start.elapsed());
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }

    /// Wait up to `limit` for `n` completions; how long it took.
    fn wait_completed(&self, n: u16, limit: Duration) -> Option<Duration> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if self.completed() >= n {
                return Some(start.elapsed());
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Configuration and control
// ---------------------------------------------------------------------------

#[test]
fn config_offers_one_stream_and_one_channel_map() {
    let g = Guest::new();
    assert_eq!(g.dev.cfg_read(0, 4), 0, "jacks");
    assert_eq!(g.dev.cfg_read(4, 4), 1, "streams");
    assert_eq!(g.dev.cfg_read(8, 4), 1, "chmaps");
}

#[test]
fn the_stream_is_16_bit_stereo_output_at_48_khz() {
    let g = Guest::new();
    let mut req = Hdr { code: R_PCM_INFO }.as_bytes().to_vec();
    req.extend_from_slice(
        QueryInfo {
            start_id: 0,
            count: 1,
            size: 32,
        }
        .as_bytes(),
    );
    let resp = g.ctrl(&req, 4 + 32);
    assert_eq!(u32::from_le_bytes(resp[..4].try_into().unwrap()), S_OK);
    let info = PcmInfo::read_from_bytes(&resp[4..36]).unwrap();
    assert_eq!(info.formats, 1 << PCM_FMT_S16);
    assert_eq!(info.rates, 1 << PCM_RATE_48000);
    assert_eq!(info.direction, D_OUTPUT);
    assert_eq!((info.channels_min, info.channels_max), (2, 2));
}

#[test]
fn a_query_past_the_one_stream_is_a_bad_message() {
    let g = Guest::new();
    let mut req = Hdr { code: R_PCM_INFO }.as_bytes().to_vec();
    req.extend_from_slice(
        QueryInfo {
            start_id: 1,
            count: 1,
            size: 32,
        }
        .as_bytes(),
    );
    assert_eq!(g.status(&req), S_BAD_MSG);
}

/// The reply to a query is sized by the guest (item size times count).
/// One that cannot fit the guest's reply buffer is refused before it is
/// built: a 4 GiB item size used to be allocated, then refused.
#[test]
fn a_query_bigger_than_its_reply_buffer_is_refused_before_it_is_built() {
    let g = Guest::new();
    let q = QueryInfo {
        start_id: 0,
        count: 1,
        size: 64 << 20,
    };
    g.write(REQ_GPA, q.as_bytes());
    let bufs = [ChainBuf::Readable {
        addr: REQ_GPA,
        len: size_of::<QueryInfo>() as u32,
    }];
    let mut reader = ChainReader::new(&bufs, &g.physmap);
    let reply = g.dev.query(&mut reader, 1, pcm_info().as_bytes(), 4096);
    assert_eq!(reply.status, S_BAD_MSG);
    assert!(
        reply.payload.is_empty(),
        "{} bytes built",
        reply.payload.len()
    );
}

#[test]
fn the_channel_map_is_front_left_front_right() {
    let g = Guest::new();
    let mut req = Hdr { code: R_CHMAP_INFO }.as_bytes().to_vec();
    req.extend_from_slice(
        QueryInfo {
            start_id: 0,
            count: 1,
            size: 24,
        }
        .as_bytes(),
    );
    let resp = g.ctrl(&req, 4 + 24);
    assert_eq!(u32::from_le_bytes(resp[..4].try_into().unwrap()), S_OK);
    let map = ChmapInfo::read_from_bytes(&resp[4..28]).unwrap();
    assert_eq!(map.direction, D_OUTPUT);
    assert_eq!(map.channels, 2);
    assert_eq!(map.positions[..2], [CHMAP_FL, CHMAP_FR]);
}

/// A refused request reaches the log, so silence has a cause there; and
/// a guest repeating it cannot fill the log.
#[test]
fn a_refused_request_is_logged_but_not_without_limit() {
    let records = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let g = Guest::logged(slog::Logger::root(
        CountingDrain(Arc::clone(&records)),
        slog::o!(),
    ));
    assert_eq!(g.set_params(0, 1, PCM_FMT_S16, PCM_RATE_48000), S_NOT_SUPP);
    assert_eq!(records.load(std::sync::atomic::Ordering::Relaxed), 1);
    for _ in 0..100 {
        g.set_params(0, 1, PCM_FMT_S16, PCM_RATE_48000);
    }
    let logged = records.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        (1..=REFUSAL_LOG_BURST as usize).contains(&logged),
        "{logged} records"
    );
}

#[test]
fn set_params_takes_only_the_one_format() {
    let g = Guest::new();
    assert_eq!(g.set_params(0, 2, PCM_FMT_S16, PCM_RATE_48000), S_OK);
    assert_eq!(g.set_params(0, 1, PCM_FMT_S16, PCM_RATE_48000), S_NOT_SUPP);
    assert_eq!(g.set_params(0, 2, PCM_FMT_S16, 6), S_NOT_SUPP, "44.1 kHz");
    assert_eq!(g.set_params(0, 2, 17, PCM_RATE_48000), S_NOT_SUPP, "S32");
    assert_eq!(g.set_params(1, 2, PCM_FMT_S16, PCM_RATE_48000), S_BAD_MSG);
}

/// The driver's buffer and period sizes: a period, a buffer of at least
/// one period, and no more than the device holds for it.
#[test]
fn set_params_takes_only_sizes_that_make_sense() {
    let g = Guest::new();
    let sized = |buffer, period| {
        g.set_params_sized(0, 2, PCM_FMT_S16, PCM_RATE_48000, buffer, period)
    };
    assert_eq!(sized(TENTH, 0), S_BAD_MSG, "no period");
    assert_eq!(
        sized(TENTH - 4, TENTH),
        S_BAD_MSG,
        "a buffer under a period"
    );
    assert_eq!(sized(MAX_BUFFER_BYTES + 4, TENTH), S_BAD_MSG, "over 1 MiB");
    assert_eq!(sized(MAX_BUFFER_BYTES, TENTH), S_OK);
    assert_eq!(sized(TENTH, TENTH), S_OK);
}

/// A message for a stream the device does not have goes back as an I/O
/// error, and plays nothing.
#[test]
fn a_message_for_another_stream_is_refused() {
    let mut g = Guest::new();
    g.started();
    let status = status_gpa(0);
    g.post(1, TENTH, 7, Some(status));
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.message_status(0).status, S_IO_ERR);
    assert_eq!(g.used_len(0), 8);
    assert!(g.sink.heard.lock().unwrap().is_empty());
}

/// The pacing thread raises the interrupt outside its section, as the
/// completion contract asks: an injection can be slow, and a vCPU
/// reprogramming the ring would wait out the section behind it.
#[test]
fn the_pacer_raises_outside_its_section() {
    let mut g = Guest::new();
    let raised_inside = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::downgrade(&g.dev.shared);
    let record = Arc::clone(&raised_inside);
    g.dev.install_interrupt(BackendIntr::detached(move |_, _| {
        if let Some(shared) = shared.upgrade() {
            record.lock().unwrap().push(shared.access.in_section());
        }
    }));
    g.started();
    g.play(TENTH / 10, 7);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    let raised = raised_inside.lock().unwrap().clone();
    assert!(!raised.is_empty(), "the completion raised nothing");
    assert!(raised.iter().all(|inside| !inside), "{raised:?}");
}

/// A message with nowhere to put its status goes back at once, empty,
/// and plays nothing.
#[test]
fn a_message_with_no_status_buffer_goes_back_empty() {
    let mut g = Guest::new();
    g.started();
    g.post(0, TENTH, 7, None);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.used_len(0), 0);
    thread::sleep(Duration::from_millis(150));
    assert!(g.sink.heard.lock().unwrap().is_empty());
}

/// A refused message says why: a guest's audio that stops has a cause
/// in the log, and the buffer budget is told apart from a bad message.
#[test]
fn a_refused_message_says_why() {
    let lines = crate::testlog::RecordingDrain::default();
    let mut g = Guest::logged(slog::Logger::root(lines.clone(), slog::o!()));
    g.play(TENTH, 7);
    assert_eq!(g.set_params(0, 2, PCM_FMT_S16, PCM_RATE_48000), S_OK);
    assert_eq!(g.pcm(R_PCM_PREPARE), S_OK);
    g.post(1, TENTH, 7, Some(status_gpa(1)));
    for fill in 0..5 {
        g.play(TENTH, fill);
    }
    let logged = lines.0.lock().unwrap().join("\n");
    for why in [
        "a stream not prepared",
        "a stream the device does not have",
        "audio past the buffer the driver declared",
    ] {
        assert!(logged.contains(why), "{why} not in:\n{logged}");
    }
}

/// A chain the device cannot serve, a control request too short for its
/// header or a message with nowhere for its status, leaves a trace in
/// the log as a refused request does.
#[test]
fn a_malformed_request_is_logged() {
    let records = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut g = Guest::logged(slog::Logger::root(
        CountingDrain(Arc::clone(&records)),
        slog::o!(),
    ));
    assert!(g.ctrl(&[0; 2], 4).is_empty(), "a short header");
    assert_eq!(records.load(std::sync::atomic::Ordering::Relaxed), 1);
    g.started();
    g.post(0, TENTH, 7, None);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(records.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
fn an_unknown_request_is_not_supported() {
    let g = Guest::new();
    assert_eq!(g.status(Hdr { code: 0x0300 }.as_bytes()), S_NOT_SUPP);
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// A message on a stream that is not prepared goes back at once, as an
/// I/O error, and plays nothing.
#[test]
fn playback_before_prepare_is_refused() {
    let mut g = Guest::new();
    g.play(TENTH, 7);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.message_status(0).status, S_IO_ERR);
    assert!(g.sink.heard.lock().unwrap().is_empty());
}

/// A status the device cannot write is not counted as written.
#[test]
fn a_refused_message_with_no_status_goes_back_empty() {
    let mut g = Guest::new();
    g.post(0, TENTH, 7, Some(OUTSIDE));
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.used_len(0), 0);
}

#[test]
fn a_played_message_with_no_status_goes_back_empty() {
    let mut g = Guest::new();
    g.started();
    g.post(0, TENTH / 10, 7, Some(OUTSIDE));
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.used_len(0), 0);
}

/// Linux sends the prefill between PREPARE and START. It waits for
/// START: neither played nor handed back, since a message handed back
/// early moves the driver's hardware pointer past audio that never
/// played, and ALSA sees an underrun. START plays it and starts its
/// clock.
#[test]
fn the_prefill_waits_for_start() {
    let mut g = Guest::new();
    assert_eq!(g.set_params(0, 2, PCM_FMT_S16, PCM_RATE_48000), S_OK);
    assert_eq!(g.pcm(R_PCM_PREPARE), S_OK);
    g.play(TENTH, 7);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(g.completed(), 0, "handed back before START");
    assert!(
        g.sink.heard.lock().unwrap().is_empty(),
        "played before START"
    );

    let start = Instant::now();
    assert_eq!(g.pcm(R_PCM_START), S_OK);
    assert!(g
        .wait_heard(TENTH as usize, Duration::from_secs(5))
        .is_some());
    assert_eq!(*g.sink.heard.lock().unwrap(), vec![7u8; TENTH as usize]);
    assert!(g.wait_completed(1, Duration::from_secs(1)).is_some());
    assert!(
        start.elapsed() >= Duration::from_millis(80),
        "handed back {:?} after START",
        start.elapsed()
    );
    assert_eq!(g.message_status(0).status, S_OK);
}

/// Audio not yet played is held in host memory, so it is bounded by the
/// buffer the driver declared, which Linux never overruns: a message
/// past it goes back at once as an I/O error, and the rest wait.
#[test]
fn audio_past_the_declared_buffer_is_refused() {
    let mut g = Guest::new();
    // set_params declares a buffer of four tenths of a second.
    assert_eq!(g.set_params(0, 2, PCM_FMT_S16, PCM_RATE_48000), S_OK);
    assert_eq!(g.pcm(R_PCM_PREPARE), S_OK);
    for fill in 0..5 {
        g.play(TENTH, fill);
    }
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.message_status(4).status, S_IO_ERR);
    assert_eq!(g.completed(), 1, "the four that fit still wait for START");
}

/// Audio queued behind what is playing counts as well. The pacer is
/// held inside the sink with the first message, so none of the rest can
/// start however slowly they arrive: four more fill the buffer of four
/// tenths, and the sixth passes it.
#[test]
fn queued_audio_counts_against_the_declared_buffer_too() {
    let mut g = Guest::new();
    g.started();
    let (inside, release) = gate(&g);
    g.play(TENTH, 0);
    inside
        .recv_timeout(Duration::from_secs(5))
        .expect("the pacer plays the first message");
    for fill in 1..6 {
        g.play(TENTH, fill);
    }
    // A refused message is answered as it arrives.
    assert_eq!(g.message_status(5).status, S_IO_ERR);
    for slot in 1..5 {
        assert_ne!(g.message_status(slot).status, S_IO_ERR, "{slot} fit");
    }
    release.send(()).expect("let the pacer go");
}

/// A prefill the driver abandons goes back at STOP: Linux waits for
/// every message before RELEASE.
#[test]
fn stop_before_start_hands_back_the_prefill() {
    let mut g = Guest::new();
    assert_eq!(g.set_params(0, 2, PCM_FMT_S16, PCM_RATE_48000), S_OK);
    assert_eq!(g.pcm(R_PCM_PREPARE), S_OK);
    g.play(TENTH, 7);
    assert_eq!(g.pcm(R_PCM_STOP), S_OK);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert!(g.sink.heard.lock().unwrap().is_empty());
}

/// The audio reaches the sink as it starts to play, but the message goes
/// back only once its time has played: the guest paces itself on
/// completions. The message is long, so a test thread the host is slow
/// to wake still sees the audio before it is done.
#[test]
fn a_message_goes_back_after_it_has_played() {
    let mut g = Guest::new();
    g.started();
    let posted = Instant::now();
    g.play(3 * TENTH, 7);
    assert!(g
        .wait_heard(3 * TENTH as usize, Duration::from_secs(5))
        .is_some());
    assert_eq!(*g.sink.heard.lock().unwrap(), vec![7u8; 3 * TENTH as usize]);
    assert_eq!(
        g.completed(),
        0,
        "300 ms of audio went back as it was heard"
    );
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert!(
        posted.elapsed() >= Duration::from_millis(300),
        "300 ms of audio went back after {:?}",
        posted.elapsed()
    );
    assert_eq!(g.message_status(0).status, S_OK);
}

/// Audio goes out as the guest's clock plays it, not as it arrives: the
/// guest times its video against that clock, and audio sent early
/// leads the picture by however much the guest has queued.
#[test]
fn audio_goes_out_as_it_plays_not_as_it_arrives() {
    let mut g = Guest::new();
    g.started();
    let start = Instant::now();
    g.play(TENTH, 1);
    g.play(TENTH, 2);
    assert!(g
        .wait_heard(2 * TENTH as usize, Duration::from_secs(5))
        .is_some());
    let mut both = vec![1u8; TENTH as usize];
    both.extend(vec![2u8; TENTH as usize]);
    assert!(*g.sink.heard.lock().unwrap() == both, "heard out of order");
    let second = g.sink.times.lock().unwrap()[1] - start;
    assert!(
        second >= Duration::from_millis(100),
        "the second went out {second:?} in, before the first had played"
    );
}

/// STOP drops the audio that had not started to play. The pacer is held
/// inside the sink with the first message, so the second cannot start
/// before the STOP however slowly the test runs.
#[test]
fn stop_drops_audio_that_has_not_started() {
    let mut g = Guest::new();
    g.started();
    let (inside, release) = gate(&g);
    g.play(TENTH, 1);
    g.play(TENTH, 2);
    inside
        .recv_timeout(Duration::from_secs(5))
        .expect("the pacer plays the first message");
    assert_eq!(g.pcm(R_PCM_STOP), S_OK);
    release.send(()).expect("let the pacer go");
    assert!(g.wait_completed(2, Duration::from_secs(5)).is_some());
    thread::sleep(Duration::from_millis(150));
    assert!(
        *g.sink.heard.lock().unwrap() == vec![1u8; TENTH as usize],
        "heard {} bytes",
        g.sink.heard.lock().unwrap().len()
    );
}

/// Messages queue up behind each other: the second is done 400 ms in,
/// 200 ms after the first.
#[test]
fn messages_play_back_to_back() {
    let mut g = Guest::new();
    g.started();
    let start = Instant::now();
    g.play(2 * TENTH, 1);
    g.play(2 * TENTH, 2);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.completed(), 1, "the second waits its turn");
    assert!(g.wait_completed(2, Duration::from_secs(5)).is_some());
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "{:?}",
        start.elapsed()
    );
}

/// As many messages as the test ring holds, 2.7 s of audio, in a running
/// stream: all of them back well before then were handed back, not
/// played. Returns how many.
fn a_long_queue(g: &mut Guest) -> u16 {
    let sized = g.set_params_sized(
        0,
        2,
        PCM_FMT_S16,
        PCM_RATE_48000,
        MAX_BUFFER_BYTES,
        TENTH,
    );
    assert_eq!(sized, S_OK);
    assert_eq!(g.pcm(R_PCM_PREPARE), S_OK);
    assert_eq!(g.pcm(R_PCM_START), S_OK);
    // Two descriptors each.
    let n = RING / 2;
    for fill in 0..n {
        g.play(64_000, fill as u8);
    }
    n
}

/// STOP hands back what was still playing, at once.
#[test]
fn stop_hands_back_the_queued_messages() {
    let mut g = Guest::new();
    let n = a_long_queue(&mut g);
    assert_eq!(g.pcm(R_PCM_STOP), S_OK);
    assert!(g.wait_completed(n, Duration::from_millis(1500)).is_some());
}

/// A reset takes the rings back: nothing queued may reach guest memory
/// afterwards.
#[test]
fn reset_forgets_the_queued_messages() {
    let mut g = Guest::new();
    g.started();
    g.play(TENTH, 1);
    VirtioDevice::reset(&g.dev);
    thread::sleep(Duration::from_millis(250));
    assert_eq!(g.completed(), 0);
    assert_eq!(g.message_status(0).status, u32::from_le_bytes([0xAA; 4]));
}

// ---------------------------------------------------------------------------
// Pause and migration
// ---------------------------------------------------------------------------

fn wait_quiesced(g: &Guest, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if Lifecycle::is_quiesced(&g.dev) {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

fn running_state() -> VirtioDeviceMigrateState {
    VirtioDeviceMigrateState::Sound(SoundMigrateState {
        buffer_bytes: Some(TENTH * 4),
        prepared: true,
        running: true,
    })
}

/// A migration exports rings with nothing in flight, so a pause hands
/// back every message, playing and held, and the device is quiesced
/// only once it has.
#[test]
fn pause_hands_back_every_message() {
    let mut g = Guest::new();
    let n = a_long_queue(&mut g);
    Lifecycle::pause(&g.dev);
    assert!(g.wait_completed(n, Duration::from_millis(1500)).is_some());
    assert!(wait_quiesced(&g, Duration::from_secs(5)));
    assert_eq!(g.message_status(u64::from(n) - 1).status, S_OK);
}

#[test]
fn pause_hands_back_the_prefill() {
    let mut g = Guest::new();
    assert_eq!(g.set_params(0, 2, PCM_FMT_S16, PCM_RATE_48000), S_OK);
    assert_eq!(g.pcm(R_PCM_PREPARE), S_OK);
    g.play(TENTH, 1);
    Lifecycle::pause(&g.dev);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert!(wait_quiesced(&g, Duration::from_secs(5)));
}

/// The vCPUs run on for a moment after the devices pause. A message
/// sent then goes back at once, unplayed, so none is in flight when
/// the rings are exported.
#[test]
fn a_paused_device_hands_messages_back_at_once() {
    let mut g = Guest::new();
    g.started();
    Lifecycle::pause(&g.dev);
    g.play(TENTH, 1);
    assert!(g.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(g.message_status(0).status, S_OK);
    assert!(wait_quiesced(&g, Duration::from_secs(5)));
    // Played, it would have been heard before it went back.
    thread::sleep(Duration::from_millis(150));
    assert!(g.sink.heard.lock().unwrap().is_empty());
}

/// A sink that holds the pacing thread inside its first `play` until
/// the test lets it go, so the test can act while the pacer cannot.
struct Gate {
    inner: Arc<Recorder>,
    entered: std::sync::mpsc::SyncSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    armed: std::sync::atomic::AtomicBool,
}

impl AudioSink for Gate {
    fn play(&self, pcm: &[u8]) {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let _ = self.entered.send(());
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        }
        self.inner.play(pcm);
    }
}

/// Install a gate in front of `g`'s recorder; returns the signal that
/// the pacer is inside it and the way to let it go.
fn gate(
    g: &Guest,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, inside) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::channel();
    g.dev.shared.audio.set_sink(Arc::new(Gate {
        inner: Arc::clone(&g.sink),
        entered,
        release: Mutex::new(wait),
        armed: std::sync::atomic::AtomicBool::new(true),
    }));
    (inside, release)
}

/// Whether the sink heard a byte of `fill` within `limit`.
fn heard_fill(g: &Guest, fill: u8, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if g.sink.heard.lock().unwrap().contains(&fill) {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

/// A pause hands back what was queued then, not what the guest sends
/// after the resume, however late the pacing thread gets to it.
#[test]
fn audio_sent_right_after_a_resume_is_played() {
    let mut g = Guest::new();
    g.started();
    let (inside, release) = gate(&g);
    g.play(TENTH, 1);
    inside
        .recv_timeout(Duration::from_secs(5))
        .expect("the pacer plays the first message");
    // The pacer is held inside the sink: none of this is seen by it
    // until it is let go.
    Lifecycle::pause(&g.dev);
    Lifecycle::resume(&g.dev);
    g.play(TENTH, 7);
    release.send(()).expect("let the pacer go");
    assert!(
        heard_fill(&g, 7, Duration::from_secs(2)),
        "the message after the resume was dropped"
    );
}

/// The same for a STOP and a START in quick succession.
#[test]
fn audio_sent_right_after_a_restart_is_played() {
    let mut g = Guest::new();
    g.started();
    let (inside, release) = gate(&g);
    g.play(TENTH, 1);
    inside
        .recv_timeout(Duration::from_secs(5))
        .expect("the pacer plays the first message");
    assert_eq!(g.pcm(R_PCM_STOP), S_OK);
    assert_eq!(g.pcm(R_PCM_START), S_OK);
    g.play(TENTH, 7);
    release.send(()).expect("let the pacer go");
    assert!(
        heard_fill(&g, 7, Duration::from_secs(2)),
        "the message after the restart was dropped"
    );
}

/// A resumed device plays, and paces again: a message goes back once
/// its audio has played, not at once as while paused. Both are bounded
/// from below only, so a slow machine cannot fail it.
#[test]
fn a_resumed_device_plays_again() {
    let mut g = Guest::new();
    g.started();
    Lifecycle::pause(&g.dev);
    Lifecycle::resume(&g.dev);
    let posted = Instant::now();
    g.play(TENTH, 7);
    assert!(g
        .wait_heard(TENTH as usize, Duration::from_secs(2))
        .is_some());
    assert!(g.wait_completed(1, Duration::from_secs(2)).is_some());
    assert!(
        posted.elapsed() >= Duration::from_millis(100),
        "100 ms of audio went back after {:?}",
        posted.elapsed()
    );
}

/// The destination's stream is the source's: a running stream plays
/// the next message with no new START.
#[test]
fn the_stream_survives_a_migration() {
    let g = Guest::new();
    g.started();
    Lifecycle::pause(&g.dev);
    let state = VirtioDevice::export_device_state(&g.dev)
        .expect("a quiesced device exports");
    assert_eq!(state, Some(running_state()));

    let mut dest = Guest::new();
    VirtioDevice::restore_device_state(
        &dest.dev,
        state.as_ref(),
        &dest.physmap,
    )
    .expect("its own export restores");
    dest.play(TENTH, 7);
    assert!(dest
        .wait_heard(TENTH as usize, Duration::from_secs(5))
        .is_some());
    assert!(dest.wait_completed(1, Duration::from_secs(5)).is_some());
    assert_eq!(dest.message_status(0).status, S_OK);
}

#[test]
fn an_idle_device_exports_an_idle_stream() {
    let g = Guest::new();
    assert_eq!(
        VirtioDevice::export_device_state(&g.dev).expect("export"),
        Some(VirtioDeviceMigrateState::Sound(SoundMigrateState::default()))
    );
}

/// A payload with no state for this device is one from a peer that
/// cannot carry it: refused, not taken as an idle device.
#[test]
fn a_payload_without_sound_state_is_refused() {
    let g = Guest::new();
    assert!(
        VirtioDevice::restore_device_state(&g.dev, None, &g.physmap).is_err()
    );
}

/// The peer is unauthenticated: a stream this device could not have
/// reached is refused, and so is another device's state.
#[test]
fn a_stream_the_device_could_not_reach_is_refused() {
    let bad = [
        SoundMigrateState {
            buffer_bytes: Some(0),
            prepared: false,
            running: false,
        },
        SoundMigrateState {
            buffer_bytes: Some(MAX_BUFFER_BYTES + 1),
            prepared: false,
            running: false,
        },
        SoundMigrateState {
            buffer_bytes: None,
            prepared: true,
            running: false,
        },
        SoundMigrateState {
            buffer_bytes: Some(TENTH),
            prepared: false,
            running: true,
        },
    ];
    for state in bad {
        let g = Guest::new();
        let payload = VirtioDeviceMigrateState::Sound(state.clone());
        assert!(
            VirtioDevice::restore_device_state(
                &g.dev,
                Some(&payload),
                &g.physmap
            )
            .is_err(),
            "{state:?}"
        );
    }
    let g = Guest::new();
    let gpu = VirtioDeviceMigrateState::Gpu(Default::default());
    assert!(
        VirtioDevice::restore_device_state(&g.dev, Some(&gpu), &g.physmap)
            .is_err()
    );
}

/// What `snd_tx` reports: the audio already queued ahead of a message,
/// or, when the clock had run out, the silence before it.
#[test]
fn a_message_sees_the_audio_ahead_of_it_or_the_gap_before_it() {
    let ms = Duration::from_millis;
    let now = Instant::now() + ms(1000);
    assert_eq!(lead(None, now), (Duration::ZERO, Duration::ZERO));
    assert_eq!(lead(Some(now + ms(30)), now), (ms(30), Duration::ZERO));
    assert_eq!(lead(Some(now), now), (Duration::ZERO, Duration::ZERO));
    assert_eq!(lead(Some(now - ms(12)), now), (Duration::ZERO, ms(12)));
}
