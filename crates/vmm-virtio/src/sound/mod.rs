// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO sound device (type 25), one playback stream.
//!
//! The device offers one output stream, stereo signed 16-bit at 48 kHz,
//! with a front-left/front-right channel map, and no jacks, capture or
//! controls. Playback goes to an [`AudioBroker`], which a remote-display
//! server can connect a sink to.
//!
//! The guest paces itself on completions: the Linux driver advances the
//! stream's position when the device hands a message back. So a message
//! is handed back when its audio has played, not when it arrives. A
//! pacing thread keeps a playback clock: it sends each message's PCM to
//! the broker as the clock reaches it, and completes the message when
//! the clock has run through it. Sending on arrival would put the audio
//! ahead of the guest's own clock, which its video is timed against, by
//! however much the guest had queued.
//! The completions reach guest memory through a [`VirtioCompletion`],
//! inside a [`GuestAccess`] session, so a reset can take the rings back
//! and know that nothing queued will write to them afterwards.

pub mod protocol;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmm_core::mem::PhysMap;
use vmm_core::ratelimit::TokenBucket;
use vmm_devices::audio::{pcm_bytes_per_second, AudioBroker};
use vmm_devices::lifecycle::{
    DeviceStateError, SoundMigrateState, VirtioDeviceMigrateState,
};
use vmm_devices::Lifecycle;
use zerocopy::{FromBytes, IntoBytes};

use super::access::{GuestAccess, RingTag};
use super::chain_io::{ChainReader, ChainWriter};
use super::pci::intr::{BackendIntr, IntrSession, IntrSlot};
use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::VirtioDevice;
use protocol::*;

/// DTrace probes for the playback path; `tools/audio-rate.d` reads
/// them with the RDP server's.
#[usdt::provider(provider = "vmm")]
mod probes {
    /// The guest's message was queued to play. `ahead_us` is the audio
    /// already queued before it; `gap_us` is the silence before it when
    /// the clock had run out, an underrun.
    fn snd_tx(bytes: u64, ahead_us: u64, gap_us: u64) {}
    /// The pacer sent a message's audio to the sink, `late_us` after it
    /// was to start.
    fn snd_play(bytes: u64, late_us: u64) {}
    /// The pacer handed a message back, `late_us` after its audio had
    /// played.
    fn snd_done(late_us: u64) {}
}

pub const CONFIG_SIZE: u16 = 16;

/// Largest hardware buffer a driver may ask for: over five seconds of
/// the one format. The buffer bounds how much PCM one message may carry,
/// and so how much the device copies out of guest memory for it.
const MAX_BUFFER_BYTES: u32 = 1 << 20;

/// VirtIO sound device, one playback stream.
pub struct VirtioSound {
    shared: Arc<Shared>,
    pacer: Mutex<Option<thread::JoinHandle<()>>>,
    log: slog::Logger,
    /// Caps the refusals logged: the guest chooses how often it makes
    /// them.
    refusals: TokenBucket,
}

/// Refusals the device logs in a burst, and how long the bucket takes to
/// refill: enough to see what the guest asked for, not enough for it to
/// fill the log.
const REFUSAL_LOG_BURST: u32 = 4;
const REFUSAL_LOG_PERIOD: Duration = Duration::from_secs(15);

struct Shared {
    physmap: Arc<PhysMap>,
    audio: Arc<AudioBroker>,
    access: GuestAccess,
    interrupt: Arc<IntrSlot>,
    /// The transmit ring's completion handler, for the ring the driver
    /// programmed; dropped when it retires the ring.
    tx_completion: Mutex<Option<Arc<VirtioCompletion>>>,
    stream: Mutex<Stream>,
    playing: Mutex<Playing>,
    wake: Condvar,
}

/// The stream as the driver set it up.
#[derive(Default)]
struct Stream {
    /// Hardware buffer size, once SET_PARAMS accepted one.
    buffer_bytes: Option<u32>,
    /// Between PREPARE and RELEASE: the driver may send messages.
    prepared: bool,
    running: bool,
}

/// Messages whose audio is playing, and the clock they play on.
#[derive(Default)]
struct Playing {
    /// Messages sent before START, not yet played.
    held: Vec<Held>,
    queue: VecDeque<Pending>,
    /// When the audio already queued will have played.
    clock: Option<Instant>,
    /// The device is paused: a message goes back as it arrives, and
    /// nothing plays.
    paused: bool,
    /// The pacer is handing messages back outside the lock.
    busy: bool,
    shutdown: bool,
}

/// A message waiting for START.
struct Held {
    tag: RingTag,
    head: u16,
    status: u64,
    pcm: Vec<u8>,
}

impl Playing {
    /// The audio held for START or waiting to sound.
    fn unplayed_bytes(&self) -> usize {
        let held: usize = self.held.iter().map(|h| h.pcm.len()).sum();
        let queued: usize = self
            .queue
            .iter()
            .filter_map(|p| p.pcm.as_ref().map(Vec::len))
            .sum();
        held + queued
    }

    /// Have the pacer hand back every message now: those playing, and
    /// those held for START, unplayed.
    ///
    /// Each is marked here, rather than by a flag the pacer reads when
    /// it next runs: a message the guest sends after a quick resume or
    /// START would otherwise be handed back with them, unheard.
    fn hand_back_all(&mut self) {
        let now = Instant::now();
        for p in &mut self.queue {
            p.pcm = None;
            p.starts = now;
            p.due = now;
        }
        for h in std::mem::take(&mut self.held) {
            self.queue.push_back(Pending {
                tag: h.tag,
                head: h.head,
                status: h.status,
                starts: now,
                pcm: None,
                due: now,
            });
        }
        self.clock = None;
    }

    /// Queue a message to play once the audio before it has, and to go
    /// back once its own has.
    fn schedule(&mut self, tag: RingTag, head: u16, status: u64, pcm: Vec<u8>) {
        let now = Instant::now();
        probes::snd_tx!(|| {
            let (ahead, gap) = lead(self.clock, now);
            (pcm.len() as u64, micros(ahead), micros(gap))
        });
        let starts = self.clock.map_or(now, |c| c.max(now));
        let due = starts + play_time(pcm.len());
        self.clock = Some(due);
        self.queue.push_back(Pending {
            tag,
            head,
            status,
            starts,
            pcm: Some(pcm),
            due,
        });
    }
}

/// A message waiting for its audio to play.
struct Pending {
    tag: RingTag,
    head: u16,
    /// Where its `virtio_snd_pcm_status` goes.
    status: u64,
    /// When its audio starts to play, and goes to the sink.
    starts: Instant,
    /// Its audio, until it has gone to the sink or been dropped.
    pcm: Option<Vec<u8>>,
    /// When its audio has played, and it goes back.
    due: Instant,
}

impl VirtioSound {
    /// A device that plays into `audio`.
    ///
    /// # Errors
    ///
    /// Returns an error if the pacing thread cannot start.
    pub fn new(
        audio: Arc<AudioBroker>,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            physmap,
            audio,
            access: GuestAccess::new(NUM_QUEUES),
            interrupt: Arc::new(IntrSlot::new()),
            tx_completion: Mutex::new(None),
            stream: Mutex::default(),
            playing: Mutex::default(),
            wake: Condvar::new(),
        });
        let pacer_shared = Arc::clone(&shared);
        let pacer = thread::Builder::new()
            .name("virtio-snd-pacer".into())
            .spawn(move || pace(&pacer_shared))
            .map_err(|e| anyhow::anyhow!("spawn virtio-snd pacer: {e}"))?;
        Ok(Self {
            shared,
            pacer: Mutex::new(Some(pacer)),
            log,
            refusals: TokenBucket::new(REFUSAL_LOG_BURST, REFUSAL_LOG_PERIOD),
        })
    }

    /// Log a request the device could not serve at all, as a refusal is.
    fn malformed(&self, why: &'static str) {
        if self.refusals.take() {
            slog::warn!(self.log, "virtio-snd dropped a request"; "why" => why);
        }
    }

    /// Log a refused request, so audio that stops has a cause in the log,
    /// at most as often as the bucket lets the guest.
    fn refused(&self, request: &str, status: u32) {
        if self.refusals.take() {
            slog::warn!(self.log, "virtio-snd refused a request";
                "request" => request,
                "status" => format!("{status:#06x}"));
        }
    }

    /// Install the transport interrupt path once the transport exists.
    pub fn install_interrupt(&self, intr: Arc<BackendIntr>) {
        self.shared.interrupt.install(intr);
    }

    fn stream(&self) -> std::sync::MutexGuard<'_, Stream> {
        self.shared.stream.lock().expect("virtio-snd: stream lock")
    }

    fn playing(&self) -> std::sync::MutexGuard<'_, Playing> {
        self.shared
            .playing
            .lock()
            .expect("virtio-snd: playing lock")
    }

    /// Serve one control request and return the bytes written.
    pub fn handle_ctrl(&self, bufs: &[ChainBuf], physmap: &PhysMap) -> u32 {
        let mut reader = ChainReader::new(bufs, physmap);
        let mut writer = ChainWriter::new(bufs, physmap);
        let Ok(hdr) = read::<Hdr>(&mut reader) else {
            self.malformed("a header shorter than 4 bytes");
            return 0;
        };
        // What a reply's payload may take: the status header goes first.
        let room = writer.capacity().saturating_sub(size_of::<Hdr>());
        let reply = match hdr.code {
            R_PCM_INFO => {
                self.query(&mut reader, 1, pcm_info().as_bytes(), room)
            }
            R_CHMAP_INFO => {
                self.query(&mut reader, 1, chmap_info().as_bytes(), room)
            }
            R_JACK_INFO => self.query(&mut reader, 0, &[], room),
            R_PCM_SET_PARAMS => Reply::status(self.set_params(&mut reader)),
            R_PCM_PREPARE | R_PCM_START | R_PCM_STOP | R_PCM_RELEASE => {
                Reply::status(self.transition(hdr.code, &mut reader))
            }
            _ => Reply::status(S_NOT_SUPP),
        };
        if reply.status != S_OK {
            self.refused(&format!("{:#06x}", hdr.code), reply.status);
        }
        // A response that does not fit goes back as its status alone.
        let status = Hdr { code: reply.status };
        if writer.capacity() >= 4 + reply.payload.len() {
            let _ = writer.write_all(status.as_bytes());
            let _ = writer.write_all(&reply.payload);
        } else if writer.capacity() >= 4 {
            let _ = writer.write_all(Hdr { code: S_BAD_MSG }.as_bytes());
        } else {
            self.malformed("no room for the response");
        }
        writer.written()
    }

    /// Answer a query for `available` items of one kind, each `item`,
    /// padded to the size the driver gave. A reply larger than `room`,
    /// what the driver's reply buffer holds, is refused before it is
    /// built: the driver chooses the size, and could ask for 4 GiB.
    fn query(
        &self,
        reader: &mut ChainReader<'_>,
        available: u32,
        item: &[u8],
        room: usize,
    ) -> Reply {
        let Ok(q) = read::<QueryInfo>(reader) else {
            return Reply::status(S_BAD_MSG);
        };
        let size = q.size as usize;
        let in_range = q
            .start_id
            .checked_add(q.count)
            .is_some_and(|end| end <= available);
        let fits = size
            .checked_mul(q.count as usize)
            .is_some_and(|total| total <= room);
        if !in_range || size < item.len() || !fits {
            return Reply::status(S_BAD_MSG);
        }
        let mut payload = Vec::with_capacity(size * q.count as usize);
        for _ in 0..q.count {
            payload.extend_from_slice(item);
            payload.resize(payload.len() + size - item.len(), 0);
        }
        Reply {
            status: S_OK,
            payload,
        }
    }

    fn set_params(&self, reader: &mut ChainReader<'_>) -> u32 {
        let (Ok(hdr), Ok(p)) =
            (read::<PcmHdr>(reader), read::<PcmSetParams>(reader))
        else {
            return S_BAD_MSG;
        };
        if hdr.stream_id != 0 {
            return S_BAD_MSG;
        }
        if p.channels != 2
            || p.format != PCM_FMT_S16
            || p.rate != PCM_RATE_48000
            || p.features != 0
        {
            return S_NOT_SUPP;
        }
        if p.period_bytes == 0
            || p.buffer_bytes < p.period_bytes
            || p.buffer_bytes > MAX_BUFFER_BYTES
        {
            return S_BAD_MSG;
        }
        self.stream().buffer_bytes = Some(p.buffer_bytes);
        S_OK
    }

    fn transition(&self, code: u32, reader: &mut ChainReader<'_>) -> u32 {
        let Ok(hdr) = read::<PcmHdr>(reader) else {
            return S_BAD_MSG;
        };
        if hdr.stream_id != 0 {
            return S_BAD_MSG;
        }
        let mut stream = self.stream();
        match code {
            R_PCM_PREPARE | R_PCM_START if stream.buffer_bytes.is_none() => {
                S_BAD_MSG
            }
            R_PCM_PREPARE => {
                stream.prepared = true;
                S_OK
            }
            R_PCM_START => {
                stream.prepared = true;
                stream.running = true;
                // The prefill plays now, and its clock starts now.
                let mut playing = self.playing();
                for h in std::mem::take(&mut playing.held) {
                    playing.schedule(h.tag, h.head, h.status, h.pcm);
                }
                self.shared.wake.notify_all();
                S_OK
            }
            _ => {
                // STOP and RELEASE: hand back what is still playing, and
                // what never started, unplayed.
                stream.running = false;
                if code == R_PCM_RELEASE {
                    stream.prepared = false;
                }
                self.playing().hand_back_all();
                self.shared.wake.notify_all();
                S_OK
            }
        }
    }

    /// Get or create the transmit ring's completion handler, as
    /// virtio-vsock does for its receive ring.
    fn ensure_tx_completion(&self, queue: &VirtQueue) -> Arc<VirtioCompletion> {
        let mut slot = self
            .shared
            .tx_completion
            .lock()
            .expect("virtio-snd: completion lock");
        if let Some(completion) = slot.as_ref() {
            return Arc::clone(completion);
        }
        let intr = Arc::clone(&self.shared.interrupt);
        let session = self.shared.interrupt.next_session(IntrSession::INITIAL);
        let completion = VirtioCompletion::new(
            queue,
            Arc::clone(&self.shared.physmap),
            session,
            move |session| intr.raise(session, VQ_TX),
        );
        *slot = Some(Arc::clone(&completion));
        completion
    }

    /// Take the guest's I/O messages: queue each one to play after the
    /// audio before it, hold it for START when the
    /// stream is prepared but not running, or hand it back now as an
    /// I/O error when the stream is not prepared.
    fn transmit(&self, queue: &mut VirtQueue, physmap: &PhysMap) {
        let completion = self.ensure_tx_completion(queue);
        let Some(session) = self.shared.access.enter_current(VQ_TX) else {
            return;
        };
        let tag = session.tag();
        let max = usize::from(queue.size());
        queue.drain_avail(physmap, max, |q, head| {
            let Some(bufs) = q.collect_chain(physmap, head) else {
                self.malformed("a descriptor chain that cannot be walked");
                completion.complete(head, 0);
                return;
            };
            // The status is the chain's last writable buffer.
            let status = bufs.iter().rev().find_map(|b| match b {
                ChainBuf::Writable { addr, len } if *len >= 8 => Some(*addr),
                _ => None,
            });
            let Some(status) = status else {
                self.malformed("a message with nowhere for its status");
                completion.complete(head, 0);
                return;
            };
            // The stream lock is held to the end, so a START cannot
            // pass between this message's check and its being held.
            let stream = self.stream();
            let buffer_bytes = stream.buffer_bytes.unwrap_or(0) as usize;
            let mut reader = ChainReader::new(&bufs, physmap);
            let len = match read::<PcmXfer>(&mut reader) {
                Err(_) => Err("a header shorter than 4 bytes"),
                Ok(x) if x.stream_id != 0 => {
                    Err("a stream the device does not have")
                }
                Ok(_) if !stream.prepared => Err("a stream not prepared"),
                Ok(_) => Ok(reader.remaining().min(buffer_bytes)),
            };
            let mut playing = self.playing();
            if len.is_ok() && playing.paused {
                // Nothing may be in flight while the device is paused:
                // a migration exports the rings as they are.
                hand_back(&completion, physmap, head, status, S_OK);
                return;
            }
            // Audio not yet played is held in host memory, so it may not
            // pass the buffer the driver declared, which Linux keeps to.
            // Checked before the copy is made.
            let pcm = len.and_then(|len| {
                if playing.unplayed_bytes() + len > buffer_bytes {
                    return Err("audio past the buffer the driver declared");
                }
                let mut pcm = vec![0u8; len];
                reader
                    .read_exact(&mut pcm)
                    .map(|()| pcm)
                    .map_err(|_| "audio in memory the device cannot read")
            });
            let pcm = match pcm {
                Ok(pcm) => pcm,
                Err(why) => {
                    drop(playing);
                    self.refused(&format!("audio: {why}"), S_IO_ERR);
                    hand_back(&completion, physmap, head, status, S_IO_ERR);
                    return;
                }
            };
            if stream.running {
                playing.schedule(tag, head, status, pcm);
            } else {
                playing.held.push(Held {
                    tag,
                    head,
                    status,
                    pcm,
                });
            }
        });
        self.shared.wake.notify_all();
    }

    /// Take the rings back: close them to deferred work, wait out what
    /// is inside, and forget the handler and the queued messages. With a
    /// queue, that ring alone; without, every ring and the stream.
    fn retire(&self, queue: Option<u16>) {
        match queue {
            Some(idx) => {
                self.shared.access.close_queue(idx);
            }
            None => self.shared.access.close_all(),
        }
        self.shared.access.drain();
        *self
            .shared
            .tx_completion
            .lock()
            .expect("virtio-snd: completion lock") = None;
        {
            let mut playing = self.playing();
            playing.held.clear();
            playing.queue.clear();
            playing.clock = None;
        }
        if queue.is_none() {
            *self.stream() = Stream::default();
        }
        self.shared.access.reopen(
            self.shared
                .interrupt
                .next_session(self.shared.access.intr_session()),
        );
    }

    fn stop_pacer(&self) {
        self.playing().shutdown = true;
        self.shared.wake.notify_all();
        if let Some(pacer) =
            self.pacer.lock().expect("virtio-snd: pacer lock").take()
        {
            if pacer.join().is_err() {
                slog::warn!(self.log, "virtio-snd pacing thread panicked");
            }
        }
    }
}

impl Drop for VirtioSound {
    fn drop(&mut self) {
        self.stop_pacer();
    }
}

/// A control response: a status, and the items a query asked for.
struct Reply {
    status: u32,
    payload: Vec<u8>,
}

impl Reply {
    fn status(status: u32) -> Self {
        Self {
            status,
            payload: Vec::new(),
        }
    }
}

fn pcm_info() -> PcmInfo {
    PcmInfo {
        formats: 1 << PCM_FMT_S16,
        rates: 1 << PCM_RATE_48000,
        direction: D_OUTPUT,
        channels_min: 2,
        channels_max: 2,
        ..Default::default()
    }
}

fn chmap_info() -> ChmapInfo {
    let mut positions = [0; CHMAP_MAX_SIZE];
    positions[0] = CHMAP_FL;
    positions[1] = CHMAP_FR;
    ChmapInfo {
        hda_fn_nid: 0,
        direction: D_OUTPUT,
        channels: 2,
        positions,
    }
}

/// How long `bytes` of the one PCM format take to play.
/// The audio queued ahead of a message scheduled at `now` on `clock`,
/// and the silence before it when the clock has run out.
fn lead(clock: Option<Instant>, now: Instant) -> (Duration, Duration) {
    match clock {
        Some(c) if c >= now => (c - now, Duration::ZERO),
        Some(c) => (Duration::ZERO, now - c),
        None => (Duration::ZERO, Duration::ZERO),
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn play_time(bytes: usize) -> Duration {
    Duration::from_nanos(
        bytes as u64 * 1_000_000_000 / u64::from(pcm_bytes_per_second()),
    )
}

fn read<T: FromBytes>(reader: &mut ChainReader<'_>) -> std::io::Result<T> {
    let mut buf = vec![0u8; size_of::<T>()];
    reader.read_exact(&mut buf)?;
    T::read_from_bytes(&buf).map_err(|_| std::io::Error::other("short read"))
}

/// Write an I/O message's status at `gpa` and hand the message back,
/// with a used length of 0 when the status could not be written.
fn hand_back(
    completion: &VirtioCompletion,
    physmap: &PhysMap,
    head: u16,
    gpa: u64,
    status: u32,
) {
    if publish_back(completion, physmap, head, gpa, status) {
        completion.signal();
    }
}

/// As [`hand_back`], short of the interrupt: whether the guest wants
/// one. A thread off the vCPU raises it outside its section.
fn publish_back(
    completion: &VirtioCompletion,
    physmap: &PhysMap,
    head: u16,
    gpa: u64,
    status: u32,
) -> bool {
    let pcm_status = PcmStatus {
        status,
        latency_bytes: 0,
    };
    let written = physmap
        .lookup(gpa, 8)
        .is_some_and(|sub| sub.write_bytes(pcm_status.as_bytes()).is_ok());
    completion.publish_batch(&[(head, if written { 8 } else { 0 })])
}

/// The pacing thread: send each message's audio to the sink as it
/// starts to play, and hand the message back once it has played, or at
/// once after a STOP, dropping audio that had not started.
fn pace(shared: &Shared) {
    let mut playing = shared.playing.lock().expect("virtio-snd: playing lock");
    loop {
        if playing.shutdown {
            return;
        }
        let now = Instant::now();
        // A message handed back by a STOP or a pause has no audio left
        // and is due now, so it goes back unheard.
        let mut sounding = Vec::new();
        for p in &mut playing.queue {
            if p.starts > now {
                break;
            }
            if let Some(pcm) = p.pcm.take() {
                probes::snd_play!(|| (
                    pcm.len() as u64,
                    micros(now.saturating_duration_since(p.starts))
                ));
                sounding.push(pcm);
            }
        }
        let mut ready = Vec::new();
        while let Some(front) = playing.queue.front() {
            if front.due > now {
                break;
            }
            probes::snd_done!(|| micros(
                now.saturating_duration_since(front.due)
            ));
            ready.extend(playing.queue.pop_front());
        }
        if !sounding.is_empty() || !ready.is_empty() {
            playing.busy = true;
            drop(playing);
            for pcm in sounding {
                shared.audio.play(&pcm);
            }
            for p in ready {
                let published = {
                    // A ring retired since this message was queued is
                    // the guest's again: the session is refused.
                    let Some(_session) = shared.access.enter(p.tag) else {
                        continue;
                    };
                    let Some(completion) = shared
                        .tx_completion
                        .lock()
                        .expect("virtio-snd: completion lock")
                        .clone()
                    else {
                        continue;
                    };
                    let raise = publish_back(
                        &completion,
                        &shared.physmap,
                        p.head,
                        p.status,
                        S_OK,
                    );
                    (raise, completion)
                };
                // Delivery can block, so it leaves the session behind.
                if let (true, completion) = published {
                    shared.access.deliver(p.tag, || completion.signal());
                }
            }
            playing = shared.playing.lock().expect("virtio-snd: playing lock");
            playing.busy = false;
            continue;
        }
        // The next moment something starts or finishes playing.
        let next = playing
            .queue
            .iter()
            .map(|p| if p.pcm.is_some() { p.starts } else { p.due })
            .min();
        playing = match next {
            Some(next) => {
                let wait = next.saturating_duration_since(now);
                shared
                    .wake
                    .wait_timeout(playing, wait)
                    .expect("virtio-snd: playing lock")
                    .0
            }
            None => {
                shared.wake.wait(playing).expect("virtio-snd: playing lock")
            }
        };
    }
}

impl VirtioDevice for VirtioSound {
    fn device_features(&self) -> u64 {
        // Ring features: legacy transport strips these in pci.rs.
        super::bits::VIRTIO_F_RING_EVENT_IDX
            | super::bits::VIRTIO_F_RING_INDIRECT_DESC
    }

    fn set_features(&self, _features: u64) {}

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        let config = Config {
            jacks: 0,
            streams: 1,
            chmaps: 1,
            controls: 0,
        };
        super::cfg_read_bytes(config.as_bytes(), offset, len)
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        queue_idx: u16,
        queue: &mut VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32 {
        if queue_idx != VQ_CONTROL {
            return 0;
        }
        let Some(bufs) = queue.collect_chain(physmap, head) else {
            self.malformed("a descriptor chain that cannot be walked");
            return 0;
        };
        self.handle_ctrl(&bufs, physmap)
    }

    /// Control requests are answered inline. I/O messages go back from
    /// the pacing thread. The event and capture rings are left alone:
    /// this device sends no events and has no capture stream, and a
    /// posted buffer the device never uses is the driver's to keep.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        match queue_idx {
            VQ_CONTROL => {
                let Some(queue) = queues.get_mut(usize::from(VQ_CONTROL))
                else {
                    return false;
                };
                let old_used_idx = queue.read_used_idx(physmap);
                let max = usize::from(queue.size());
                let popped = queue.drain_avail(physmap, max, |q, head| {
                    let written =
                        self.process_queue(VQ_CONTROL, q, head, physmap);
                    q.push_used(physmap, head, written);
                });
                popped > 0 && queue.should_notify_guest(physmap, old_used_idx)
            }
            VQ_TX => {
                if let Some(queue) = queues.get_mut(usize::from(VQ_TX)) {
                    self.transmit(queue, physmap);
                }
                false
            }
            _ => false,
        }
    }

    /// The transmit ring's handler names the ring it was built on.
    fn queue_addr_set(&self, queue_idx: u16, _queue: &VirtQueue) {
        if queue_idx == VQ_TX {
            self.retire(Some(queue_idx));
        }
    }

    fn reset(&self) {
        self.retire(None);
    }

    /// The stream as the driver set it up. Nothing is in flight once
    /// the device is paused and quiesced; a message that still is would
    /// be one the destination never hands back, so it fails the export.
    fn export_device_state(
        &self,
    ) -> Result<Option<VirtioDeviceMigrateState>, DeviceStateError> {
        let stream = self.stream();
        let playing = self.playing();
        if !playing.queue.is_empty() || !playing.held.is_empty() {
            return Err(DeviceStateError::Export(
                "virtio-snd: messages are still in flight".into(),
            ));
        }
        Ok(Some(VirtioDeviceMigrateState::Sound(SoundMigrateState {
            buffer_bytes: stream.buffer_bytes,
            prepared: stream.prepared,
            running: stream.running,
        })))
    }

    /// Take the source's stream, if this device could have reached it:
    /// a buffer SET_PARAMS would accept, and no PREPARE or START without
    /// what they need.
    fn restore_device_state(
        &self,
        state: Option<&VirtioDeviceMigrateState>,
        _physmap: &PhysMap,
    ) -> Result<(), DeviceStateError> {
        let s = match state {
            Some(VirtioDeviceMigrateState::Sound(s)) => s,
            Some(other) => {
                return Err(DeviceStateError::WrongKind {
                    want: vmm_devices::lifecycle::SOUND_KIND,
                    got: other.kind(),
                })
            }
            None => {
                return Err(DeviceStateError::invalid(
                    "virtio-snd: payload carries no stream",
                ))
            }
        };
        if s.buffer_bytes
            .is_some_and(|b| b == 0 || b > MAX_BUFFER_BYTES)
        {
            return Err(DeviceStateError::invalid(format!(
                "virtio-snd: a buffer of {:?} bytes",
                s.buffer_bytes
            )));
        }
        if (s.prepared && s.buffer_bytes.is_none())
            || (s.running && !s.prepared)
        {
            return Err(DeviceStateError::invalid(format!(
                "virtio-snd: a stream no driver could reach: {s:?}"
            )));
        }
        *self.stream() = Stream {
            buffer_bytes: s.buffer_bytes,
            prepared: s.prepared,
            running: s.running,
        };
        Ok(())
    }
}

impl Lifecycle for VirtioSound {
    fn type_name(&self) -> &'static str {
        vmm_devices::lifecycle::SOUND_KIND
    }

    /// Hand back every message, and every one that arrives until the
    /// device resumes, so nothing is in flight: a migration exports the
    /// rings next. The guest loses what was queued, a fraction of a
    /// second of sound.
    fn pause(&self) {
        let mut playing = self.playing();
        playing.paused = true;
        playing.hand_back_all();
        self.shared.wake.notify_all();
    }

    fn is_quiesced(&self) -> bool {
        let playing = self.playing();
        playing.queue.is_empty() && playing.held.is_empty() && !playing.busy
    }

    fn resume(&self) {
        self.playing().paused = false;
    }

    fn halt(&self) {
        self.stop_pacer();
    }
}

#[cfg(test)]
mod tests;
