//! Native mic capture: a `cpal` input stream that downmixes to mono, resamples to 16 kHz, frames to
//! 20 ms, and pushes PCM16 frames to the session. Mute is enforced by the manager
//! (`set_mic_muted`), so capture just streams while a session is active.

use std::sync::mpsc::{channel, Sender};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use joi_core::media::{pcm16_from_f32, resample_linear, AudioFormat, FrameAccumulator};

use crate::MediaError;

/// Stops capture when dropped — the input stream lives on a dedicated thread that parks until this
/// handle's sender is dropped.
pub struct CaptureHandle {
    _stop: Sender<()>,
}

/// Spawn mic capture on its own thread (owns the `!Send` input stream). 16 kHz mono PCM16 frames of
/// `frame_samples` are pushed to `frames`, dropped on overflow so the realtime audio thread never
/// blocks. Capture stops when the returned [`CaptureHandle`] is dropped.
#[must_use]
pub fn spawn_capture(
    frames: tokio::sync::mpsc::Sender<Vec<i16>>,
    frame_samples: usize,
) -> CaptureHandle {
    let (stop_tx, stop_rx) = channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("joi-capture".to_string())
        .spawn(move || match Capture::start(&frames, frame_samples) {
            Ok(_stream) => {
                let _ = stop_rx.recv(); // park until the handle drops; the stream drops here
            }
            Err(e) => tracing::error!("native capture unavailable: {e}"),
        });
    if let Err(e) = spawned {
        tracing::error!("failed to spawn capture thread: {e}");
    }
    CaptureHandle { _stop: stop_tx }
}

struct Capture {
    _stream: cpal::Stream,
}

impl Capture {
    fn start(
        frames: &tokio::sync::mpsc::Sender<Vec<i16>>,
        frame_samples: usize,
    ) -> Result<Self, MediaError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(MediaError::NoInputDevice)?;
        let supported = device
            .default_input_config()
            .map_err(|e| MediaError::Backend(e.to_string()))?;

        let device_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let sample_format = supported.sample_format();
        let config = supported.config();
        let target = AudioFormat::INPUT.sample_rate;
        let err_fn = |e| tracing::error!("capture stream error: {e}");

        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let frames = frames.clone();
                let mut acc = FrameAccumulator::new(frame_samples);
                device.build_input_stream(
                    &config,
                    move |input: &[f32], _| {
                        let mono = pcm16_from_f32(&downmix_f32(input, channels));
                        let resampled = resample_linear(&mono, device_rate, target);
                        for frame in acc.push(&resampled) {
                            let _ = frames.try_send(frame);
                        }
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let frames = frames.clone();
                let mut acc = FrameAccumulator::new(frame_samples);
                device.build_input_stream(
                    &config,
                    move |input: &[i16], _| {
                        let mono = downmix_i16(input, channels);
                        let resampled = resample_linear(&mono, device_rate, target);
                        for frame in acc.push(&resampled) {
                            let _ = frames.try_send(frame);
                        }
                    },
                    err_fn,
                    None,
                )
            }
            other => return Err(MediaError::UnsupportedFormat(format!("{other:?}"))),
        }
        .map_err(|e| MediaError::Backend(e.to_string()))?;

        stream.play().map_err(|e| MediaError::Backend(e.to_string()))?;
        tracing::info!(device_rate, channels, "native mic capture started");
        Ok(Self { _stream: stream })
    }
}

/// Take channel 0 as the mono signal (adequate for a mic; avoids summing-clip).
fn downmix_f32(input: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return input.to_vec();
    }
    input.chunks(channels).map(|f| f[0]).collect()
}

fn downmix_i16(input: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return input.to_vec();
    }
    input.chunks(channels).map(|f| f[0]).collect()
}
