// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The guest's playback, over the rdpsnd channel.
//!
//! The channel offers one format, the one a sound device plays
//! ([`vmm_devices::audio`]): uncompressed PCM, so nothing is encoded
//! here. Once a client takes it, the guest's playback goes to the
//! client as it arrives, gathered into 40 ms waves; before that, and
//! after the channel closes, playback is dropped.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender,
    SoundServerFactory,
};
use tokio::sync::mpsc::UnboundedSender;
use vmm_devices::audio::{
    pcm_bytes_per_second, AudioSink, PCM_BYTES_PER_FRAME, PCM_CHANNELS,
    PCM_RATE,
};

/// How much audio one wave carries: 40 ms.
///
/// The guest plays in buffers of about 10 ms, and IronRDP's server keeps
/// only the newest four waves of a batch that waited behind a slow frame,
/// so four 10 ms waves left a 43 ms stall audible. Four of these cover
/// 160 ms. A wave's length also has only 16 bits on the wire.
const WAVE_BYTES: usize = (pcm_bytes_per_second() / 25) as usize;

/// The one format offered.
fn pcm_format() -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: PCM_CHANNELS,
        n_samples_per_sec: PCM_RATE,
        n_avg_bytes_per_sec: pcm_bytes_per_second(),
        n_block_align: PCM_BYTES_PER_FRAME as u16,
        bits_per_sample: 16,
        data: None,
    }
}

#[derive(Default)]
struct State {
    sender: Option<UnboundedSender<ServerEvent>>,
    streaming: bool,
    /// Audio gathered toward the next wave.
    pending: Vec<u8>,
}

impl State {
    /// Start or stop the stream. Either way, audio gathered for the
    /// last one does not reach the next.
    fn set_streaming(&mut self, streaming: bool) {
        self.streaming = streaming;
        self.pending.clear();
    }
}

/// The audio sink the server binds, shared with the channel's handler.
pub(crate) struct Sound {
    state: Mutex<State>,
    epoch: Instant,
}

impl Sound {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            epoch: Instant::now(),
        })
    }

    /// A client that drops without closing its sound channel never
    /// stops the stream.
    pub(crate) fn end_session(&self) {
        self.lock().set_streaming(false);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("rdpsnd: state lock")
    }
}

impl AudioSink for Sound {
    /// Gather `pcm` and send each whole wave it completes. Part of a
    /// wave waits for the guest's next buffer; while a stream runs,
    /// PipeWire keeps them coming, silence included, for a few seconds
    /// after the last sound.
    fn play(&self, pcm: &[u8]) {
        let mut st = self.lock();
        if !st.streaming || st.sender.is_none() {
            return;
        }
        st.pending.extend_from_slice(pcm);
        // The wave's audio timestamp, in milliseconds (MS-RDPEA
        // 2.2.3.10); only differences between waves mean anything.
        let ms = self.epoch.elapsed().as_millis() as u32;
        while st.pending.len() >= WAVE_BYTES {
            let wave: Vec<u8> = st.pending.drain(..WAVE_BYTES).collect();
            if let Some(sender) = &st.sender {
                // A send fails only once the server has stopped.
                let _ = sender.send(ServerEvent::Rdpsnd(
                    RdpsndServerMessage::Wave(wave, ms),
                ));
            }
        }
    }
}

/// Builds the handler for each connection's rdpsnd channel.
pub(crate) struct Factory(pub(crate) Arc<Sound>);

/// Each connection's server owns one, so the stream ends with the
/// connection even when the client never closed its sound channel.
impl Drop for Factory {
    fn drop(&mut self) {
        self.0.end_session();
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.0.lock().sender = Some(sender);
    }
}

impl SoundServerFactory for Factory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(Handler {
            sound: Arc::clone(&self.0),
            formats: [pcm_format()],
        })
    }
}

struct Handler {
    sound: Arc<Sound>,
    formats: [AudioFormat; 1],
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler").finish_non_exhaustive()
    }
}

impl RdpsndServerHandler for Handler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        // Only one format is offered, so any common one is it.
        common.first()
    }

    fn start(
        &mut self,
        _format: &NegotiatedFormat,
    ) -> Result<(), Box<dyn RdpsndError>> {
        self.sound.lock().set_streaming(true);
        Ok(())
    }

    fn stop(&mut self) {
        self.sound.lock().set_streaming(false);
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::encode_vec;
    use ironrdp_rdpsnd::pdu::{
        AudioFormatFlags, ClientAudioFormatPdu, ClientAudioOutputPdu,
        QualityMode, QualityModePdu, TrainingConfirmPdu, Version,
    };
    use ironrdp_rdpsnd::server::RdpsndServer;
    use ironrdp_svc::SvcProcessor;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    use super::*;

    /// A sound sink with its factory's sender set, as the server does.
    fn sound() -> (Arc<Sound>, Factory, UnboundedReceiver<ServerEvent>) {
        let sound = Sound::new();
        let mut factory = Factory(Arc::clone(&sound));
        let (tx, rx) = unbounded_channel();
        factory.set_sender(tx);
        (sound, factory, rx)
    }

    fn send(channel: &mut RdpsndServer, pdu: ClientAudioOutputPdu) {
        channel
            .process(&encode_vec(&pdu).expect("encode"))
            .expect("process");
    }

    /// Run a client that accepts `formats` through the channel's
    /// negotiation, as MS-RDPEA 3.1.1 orders it.
    fn negotiate(factory: &Factory, formats: Vec<AudioFormat>) -> RdpsndServer {
        let mut channel = RdpsndServer::new(factory.build_backend());
        channel.start().expect("start");
        send(
            &mut channel,
            ClientAudioOutputPdu::AudioFormat(ClientAudioFormatPdu {
                version: Version::V8,
                flags: AudioFormatFlags::ALIVE,
                formats,
                volume_left: 0xFFFF,
                volume_right: 0xFFFF,
                pitch: 0x0001_0000,
                dgram_port: 0,
            }),
        );
        send(
            &mut channel,
            ClientAudioOutputPdu::QualityMode(QualityModePdu {
                quality_mode: QualityMode::High,
            }),
        );
        send(
            &mut channel,
            ClientAudioOutputPdu::TrainingConfirm(TrainingConfirmPdu {
                timestamp: 4231,
                pack_size: 0,
            }),
        );
        channel
    }

    fn waves(rx: &mut UnboundedReceiver<ServerEvent>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) => {
                    out.push(data);
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        out
    }

    #[test]
    fn the_channel_offers_the_format_the_guest_plays() {
        let (_, factory, _) = sound();
        let offered = factory.build_backend().get_formats().to_vec();
        assert_eq!(offered.len(), 1);
        let f = &offered[0];
        assert_eq!(f.format, WaveFormat::PCM);
        assert_eq!(f.n_channels, 2);
        assert_eq!(f.n_samples_per_sec, 48_000);
        assert_eq!(f.bits_per_sample, 16);
        assert_eq!(f.n_block_align, 4);
        assert_eq!(f.n_avg_bytes_per_sec, 192_000);
    }

    #[test]
    fn nothing_plays_before_a_client_takes_the_format() {
        let (sound, _factory, mut rx) = sound();
        sound.play(&vec![1; WAVE_BYTES]);
        assert!(waves(&mut rx).is_empty());
    }

    /// The guest plays in buffers of a few milliseconds, and the server
    /// keeps only the newest four waves of a batch that waited behind a
    /// slow frame, so small buffers are gathered into fewer, longer waves.
    #[test]
    fn playback_reaches_a_client_in_chunks() {
        let (sound, factory, mut rx) = sound();
        let _channel = negotiate(&factory, vec![pcm_format()]);
        let half = WAVE_BYTES / 2;
        sound.play(&vec![1; half]);
        assert!(waves(&mut rx).is_empty(), "half a chunk waits");
        sound.play(&vec![2; half]);
        let want: Vec<u8> = [vec![1; half], vec![2; half]].concat();
        assert_eq!(waves(&mut rx), [want]);
    }

    /// A buffer longer than a chunk goes out as several, none longer:
    /// a wave's length has 16 bits on the wire.
    #[test]
    fn a_long_buffer_goes_out_in_chunks() {
        let (sound, factory, mut rx) = sound();
        let _channel = negotiate(&factory, vec![pcm_format()]);
        sound.play(&vec![7; WAVE_BYTES * 5 / 2]);
        let got = waves(&mut rx);
        assert_eq!(got.len(), 2, "the last half chunk waits");
        assert!(got.iter().all(|w| w.len() == WAVE_BYTES));
    }

    /// Part of a chunk left when a session ends is not played to the next.
    #[test]
    fn a_partial_chunk_does_not_outlive_its_session() {
        let (sound, factory, mut rx) = sound();
        let channel = negotiate(&factory, vec![pcm_format()]);
        sound.play(&vec![1; WAVE_BYTES / 2]);
        drop(channel);
        let _channel = negotiate(&factory, vec![pcm_format()]);
        sound.play(&vec![2; WAVE_BYTES]);
        assert_eq!(waves(&mut rx), [vec![2; WAVE_BYTES]]);
    }

    #[test]
    fn a_client_without_the_format_gets_nothing() {
        let (sound, factory, mut rx) = sound();
        let mut cd = pcm_format();
        cd.n_samples_per_sec = 44_100;
        let _channel = negotiate(&factory, vec![cd]);
        sound.play(&vec![1; WAVE_BYTES]);
        assert!(waves(&mut rx).is_empty());
    }

    #[test]
    fn a_client_that_drops_without_closing_gets_nothing_more() {
        let (sound, factory, mut rx) = sound();
        let _channel = negotiate(&factory, vec![pcm_format()]);
        // The connection's server, and the factory with it, goes away.
        drop(factory);
        sound.play(&vec![0; WAVE_BYTES]);
        assert!(waves(&mut rx).is_empty());
    }

    #[test]
    fn playback_stops_when_the_channel_closes() {
        let (sound, factory, mut rx) = sound();
        drop(negotiate(&factory, vec![pcm_format()]));
        sound.play(&vec![1; WAVE_BYTES]);
        assert!(waves(&mut rx).is_empty());
    }
}
