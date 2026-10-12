// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a sound device's playback goes.
//!
//! A sound device plays into an [`AudioBroker`]; a remote-display server
//! that can carry audio (RDP) connects an [`AudioSink`] to it. The two
//! come from independent `-s` flags, so the sink is bound late, as input
//! is through the `InputBroker`.

use std::sync::{Arc, Mutex};

/// Frames per second of the PCM the guest plays.
pub const PCM_RATE: u32 = 48_000;
/// Interleaved channels: left, right.
pub const PCM_CHANNELS: u16 = 2;
/// Signed 16-bit little-endian samples, two to a frame.
pub const PCM_BYTES_PER_FRAME: u32 = 4;

/// Bytes of PCM a second of playback takes.
pub const fn pcm_bytes_per_second() -> u32 {
    PCM_RATE * PCM_BYTES_PER_FRAME
}

/// Takes the guest's playback: PCM in the format the constants above
/// give, in the order and at the pace the guest plays it.
pub trait AudioSink: Send + Sync {
    fn play(&self, pcm: &[u8]);
}

/// Routes a sound device's playback to the sink bound to it, if any.
#[derive(Default)]
pub struct AudioBroker {
    sink: Mutex<Option<Arc<dyn AudioSink>>>,
}

impl AudioBroker {
    pub fn set_sink(&self, sink: Arc<dyn AudioSink>) {
        *self.sink.lock().expect("audio broker lock") = Some(sink);
    }

    /// Play `pcm` through the sink; with none bound, the audio is
    /// dropped.
    pub fn play(&self, pcm: &[u8]) {
        let sink = self.sink.lock().expect("audio broker lock").clone();
        if let Some(sink) = sink {
            sink.play(pcm);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<u8>>);

    impl AudioSink for Recorder {
        fn play(&self, pcm: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(pcm);
        }
    }

    /// A sound device can play before any remote-display server has
    /// connected a sink; that audio is dropped.
    #[test]
    fn playing_without_a_sink_drops_the_audio() {
        AudioBroker::default().play(&[1, 2, 3, 4]);
    }

    #[test]
    fn playback_reaches_the_sink() {
        let broker = AudioBroker::default();
        let sink = Arc::new(Recorder::default());
        broker.set_sink(sink.clone());
        broker.play(&[1, 2, 3, 4]);
        broker.play(&[5, 6]);
        assert_eq!(*sink.0.lock().unwrap(), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn the_format_is_16_bit_stereo_at_48_khz() {
        assert_eq!(PCM_RATE, 48_000);
        assert_eq!(PCM_CHANNELS, 2);
        assert_eq!(PCM_BYTES_PER_FRAME, 4);
        assert_eq!(pcm_bytes_per_second(), 192_000);
    }
}
