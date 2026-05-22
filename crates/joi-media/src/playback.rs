//! Native audio playback: a `cpal` output stream fed by a [`JitterBuffer`], driven over a channel.
//!
//! Provider audio is 24 kHz mono PCM16 ([`AudioFormat::OUTPUT`]); we resample to the device rate on
//! enqueue and let the realtime output callback pull fixed blocks (silence on underrun).

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use joi_core::media::{resample_linear, AudioFormat, JitterBuffer};

use crate::MediaError;

/// Control messages for the playback engine.
pub enum PlaybackCmd {
    /// A frame of 24 kHz mono PCM16 from the provider.
    Pcm(Vec<i16>),
    /// Drop all buffered audio immediately (barge-in / interrupt, FR-2).
    Flush,
}

/// Spawn the playback engine on its own thread (it owns the `!Send` `cpal` stream) and return a
/// sender for feeding it. If the audio device can't be opened the thread logs and exits; sends then
/// no-op, so playback failure never takes down the app.
#[must_use]
pub fn spawn_playback() -> Sender<PlaybackCmd> {
    let (tx, rx) = std::sync::mpsc::channel::<PlaybackCmd>();
    let spawned = std::thread::Builder::new()
        .name("joi-playback".to_string())
        .spawn(move || run(&rx));
    if let Err(e) = spawned {
        tracing::error!("failed to spawn playback thread: {e}");
    }
    tx
}

fn run(rx: &Receiver<PlaybackCmd>) {
    let engine = match Playback::start() {
        Ok(engine) => engine,
        Err(e) => {
            tracing::error!("native playback unavailable: {e}");
            return;
        }
    };
    tracing::info!(device_rate = engine.device_rate, "native playback started");
    // Drains until all senders drop. Each message touches the jitter buffer shared with the
    // realtime callback.
    while let Ok(cmd) = rx.recv() {
        match cmd {
            PlaybackCmd::Pcm(pcm) => engine.enqueue(&pcm),
            PlaybackCmd::Flush => engine.flush(),
        }
    }
}

/// Owns the output stream and the buffer shared with its realtime callback.
struct Playback {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<JitterBuffer>>,
    device_rate: u32,
}

impl Playback {
    fn start() -> Result<Self, MediaError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(MediaError::NoOutputDevice)?;
        let supported = device
            .default_output_config()
            .map_err(|e| MediaError::Backend(e.to_string()))?;

        let device_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let sample_format = supported.sample_format();
        let config = supported.config();

        let buffer = Arc::new(Mutex::new(JitterBuffer::new()));
        let cb_buffer = Arc::clone(&buffer);
        let err_fn = |e| tracing::error!("playback stream error: {e}");

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |out: &mut [f32], _| {
                    for (i, s) in pull(&cb_buffer, out.len() / channels.max(1))
                        .iter()
                        .enumerate()
                    {
                        let v = f32::from(*s) / 32768.0;
                        for c in 0..channels {
                            out[i * channels + c] = v;
                        }
                    }
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config,
                move |out: &mut [i16], _| {
                    for (i, s) in pull(&cb_buffer, out.len() / channels.max(1))
                        .iter()
                        .enumerate()
                    {
                        for c in 0..channels {
                            out[i * channels + c] = *s;
                        }
                    }
                },
                err_fn,
                None,
            ),
            other => return Err(MediaError::UnsupportedFormat(format!("{other:?}"))),
        }
        .map_err(|e| MediaError::Backend(e.to_string()))?;

        stream
            .play()
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        Ok(Self {
            _stream: stream,
            buffer,
            device_rate,
        })
    }

    fn enqueue(&self, pcm: &[i16]) {
        let resampled = resample_linear(pcm, AudioFormat::OUTPUT.sample_rate, self.device_rate);
        if let Ok(mut jb) = self.buffer.lock() {
            jb.enqueue(&resampled);
        }
    }

    fn flush(&self) {
        if let Ok(mut jb) = self.buffer.lock() {
            jb.flush();
        }
    }
}

/// Pull `frames` mono samples from the shared buffer, returning silence on lock poisoning.
fn pull(buffer: &Arc<Mutex<JitterBuffer>>, frames: usize) -> Vec<i16> {
    buffer
        .lock()
        .map_or_else(|_| vec![0; frames], |mut jb| jb.pull(frames))
}
