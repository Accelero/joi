//! Native mic capture with conditioning. The realtime cpal callback only downmixes to mono and
//! forwards raw samples; the capture thread resamples to 16 kHz and runs a WebRTC-style audio
//! processing module (Sonora: echo cancellation + noise suppression + AGC) before framing to 20 ms
//! PCM16 for the session. This restores the echo cancellation / `noiseSuppression` /
//! `autoGainControl` the webview's `getUserMedia` used to provide; Gemini only does VAD, so a
//! leveled, denoised, echo-free mic improves detection and stops the model interrupting itself when
//! playing through speakers.
//!
//! **AEC:** the playback engine's provider audio is forwarded here as the render (far-end)
//! reference via `render_rx`; the APM subtracts it from the mic so Joi's own voice picked up by the
//! speakers doesn't read as user speech (which otherwise triggers a barge-in feedback loop).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use joi_core::media::{pcm16_from_f32, resample_linear, AudioFormat, FrameAccumulator};
use sonora::config::{EchoCanceller, GainController2, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig as ApmStreamConfig};

use crate::MediaError;

/// APM runs at 16 kHz (a WebRTC-supported rate, and our provider rate); device audio is resampled
/// to it first. 10 ms frames = 160 samples.
const APM_RATE: u32 = AudioFormat::INPUT.sample_rate;
const APM_FRAME: usize = (APM_RATE / 100) as usize;
/// Cap on buffered far-end (render) audio (~200 ms at 16 kHz). If the provider ever streams faster
/// than real time, drop the oldest so the AEC reference lead stays within AEC3's delay-tracking
/// range instead of growing unboundedly.
const MAX_RENDER_BACKLOG: usize = APM_FRAME * 20;

/// Stops capture when dropped — the input stream + processing loop live on a dedicated thread that
/// exits once this handle's sender is dropped.
pub struct CaptureHandle {
    _stop: Sender<()>,
}

/// Spawn mic capture on its own thread (owns the `!Send` input stream and the APM). 16 kHz mono
/// PCM16 frames of `frame_samples` are pushed to `frames`, dropped on overflow. While `muted` is
/// set, no audio is captured. Capture stops when the returned [`CaptureHandle`] is dropped.
/// `render_rx` carries the provider audio Joi is playing — the AEC far-end reference (24 kHz PCM16,
/// [`AudioFormat::OUTPUT`]). It ends when the engine clears the render sink on stop.
#[must_use]
pub fn spawn_capture(
    frames: tokio::sync::mpsc::Sender<Vec<i16>>,
    frame_samples: usize,
    muted: Arc<AtomicBool>,
    render_rx: Receiver<Vec<i16>>,
    echo_cancellation: bool,
) -> CaptureHandle {
    let (stop_tx, stop_rx) = channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("joi-capture".to_string())
        .spawn(move || {
            if let Err(e) = run_capture(
                &frames,
                frame_samples,
                &muted,
                &stop_rx,
                &render_rx,
                echo_cancellation,
            ) {
                tracing::error!("native capture unavailable: {e}");
            }
        });
    if let Err(e) = spawned {
        tracing::error!("failed to spawn capture thread: {e}");
    }
    CaptureHandle { _stop: stop_tx }
}

fn run_capture(
    frames: &tokio::sync::mpsc::Sender<Vec<i16>>,
    frame_samples: usize,
    muted: &Arc<AtomicBool>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    render_rx: &Receiver<Vec<i16>>,
    echo_cancellation: bool,
) -> Result<(), MediaError> {
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
    let err_fn = |e| tracing::error!("capture stream error: {e}");

    // Realtime callback: mute-gate, downmix to mono f32, forward. No heavy DSP on the audio thread.
    let (raw_tx, raw_rx) = channel::<Vec<f32>>();
    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let raw_tx = raw_tx.clone();
            let muted = Arc::clone(muted);
            device.build_input_stream(
                &config,
                move |input: &[f32], _| {
                    if !muted.load(Ordering::Relaxed) {
                        let _ = raw_tx.send(downmix_f32(input, channels));
                    }
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let raw_tx = raw_tx.clone();
            let muted = Arc::clone(muted);
            device.build_input_stream(
                &config,
                move |input: &[i16], _| {
                    if !muted.load(Ordering::Relaxed) {
                        let _ = raw_tx.send(downmix_i16(input, channels));
                    }
                },
                err_fn,
                None,
            )
        }
        other => return Err(MediaError::UnsupportedFormat(format!("{other:?}"))),
    }
    .map_err(|e| MediaError::Backend(e.to_string()))?;
    drop(raw_tx); // only the stream's callback keeps a sender alive now
    stream
        .play()
        .map_err(|e| MediaError::Backend(e.to_string()))?;
    tracing::info!(
        device_rate,
        channels,
        echo_cancellation,
        "native mic capture started (NS + AGC)"
    );

    let mut pipeline = CapturePipeline::new(device_rate, frame_samples, echo_cancellation);
    loop {
        // Buffer the far-end reference (what we're playing); `process` consumes it 1:1 with capture
        // frames so the AEC sees both at the same cadence. Drains all that's queued; ends quietly
        // once the render sink is dropped on stop.
        while let Ok(render) = render_rx.try_recv() {
            pipeline.buffer_render(&render);
        }
        match raw_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(mono) => pipeline.process(&mono, frames),
            // No samples for a while: stop if the handle was dropped, else keep waiting.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if matches!(stop_rx.try_recv(), Err(TryRecvError::Disconnected)) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Resample → APM (echo cancellation + noise suppression + AGC) → 20 ms PCM16 framing. Lives on the
/// capture thread, so the APM (which is `!Send`-agnostic here) never crosses to the realtime
/// callback. Both the near-end (mic) and far-end (playback) streams are fed at 16 kHz in 10 ms APM
/// frames; the echo canceller subtracts the far-end from the near-end.
struct CapturePipeline {
    apm: AudioProcessing,
    device_rate: u32,
    /// Whether the echo canceller is enabled (config `audio.echo_cancellation`). When off, the
    /// far-end reference isn't fed and no echo is subtracted.
    echo_cancellation: bool,
    /// Near-end (mic) samples awaiting a full 10 ms APM frame.
    apm_in: Vec<f32>,
    /// Far-end (render/playback) samples awaiting a full 10 ms APM frame.
    render_in: Vec<f32>,
    out: FrameAccumulator,
}

impl CapturePipeline {
    fn new(device_rate: u32, frame_samples: usize, echo_cancellation: bool) -> Self {
        let config = Config {
            // AEC3: remove Joi's own playback (picked up by the mic) so it doesn't read as speech.
            // Off when `audio.echo_cancellation = false` (e.g. when using headphones).
            echo_canceller: echo_cancellation.then(EchoCanceller::default),
            noise_suppression: Some(NoiseSuppression::default()),
            gain_controller2: Some(GainController2::default()),
            ..Default::default()
        };
        let sc = ApmStreamConfig::new(APM_RATE, 1);
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(sc)
            .render_config(sc)
            .build();
        Self {
            apm,
            device_rate,
            echo_cancellation,
            apm_in: Vec::new(),
            render_in: Vec::new(),
            out: FrameAccumulator::new(frame_samples),
        }
    }

    fn process(&mut self, mono_device: &[f32], frames: &tokio::sync::mpsc::Sender<Vec<i16>>) {
        // Down to 16 kHz, then accumulate 10 ms APM frames.
        let pcm = pcm16_from_f32(mono_device);
        let resampled = resample_linear(&pcm, self.device_rate, APM_RATE);
        self.apm_in
            .extend(resampled.iter().map(|&s| f32::from(s) / 32768.0));

        while self.apm_in.len() >= APM_FRAME {
            // AEC3 needs the far-end (render) fed **every** capture frame at the same cadence —
            // silence when nothing is playing. Feeding it only during agent speech (as we did)
            // drifts its render/capture alignment until it cancels the *near-end* (user) voice,
            // i.e. the mic stops getting through after a few turns. Pair one render frame (real, or
            // silence) with each capture frame.
            if self.echo_cancellation {
                let render_frame: Vec<f32> = if self.render_in.len() >= APM_FRAME {
                    self.render_in.drain(..APM_FRAME).collect()
                } else {
                    vec![0.0f32; APM_FRAME]
                };
                let mut render_out = vec![0.0f32; APM_FRAME];
                let _ = self
                    .apm
                    .process_render_f32(&[&render_frame], &mut [&mut render_out]);
            }

            let frame: Vec<f32> = self.apm_in.drain(..APM_FRAME).collect();
            let mut out = vec![0.0f32; APM_FRAME];
            if self
                .apm
                .process_capture_f32(&[&frame], &mut [&mut out])
                .is_ok()
            {
                for done in self.out.push(&pcm16_from_f32(&out)) {
                    let _ = frames.try_send(done);
                }
            }
        }
    }

    /// Buffer the far-end (playback) reference for the echo canceller — provider audio at
    /// [`AudioFormat::OUTPUT`] (24 kHz), resampled to the 16 kHz APM rate. It is consumed 1:1 with
    /// capture frames in [`process`](Self::process); here we only enqueue it. The backlog is capped
    /// so that if the provider streams faster than real time, the far-end lead stays within AEC3's
    /// delay-tracking range instead of growing unboundedly.
    fn buffer_render(&mut self, render: &[i16]) {
        if !self.echo_cancellation {
            return; // AEC off: no reference needed; drained frames are simply dropped.
        }
        let resampled = resample_linear(render, AudioFormat::OUTPUT.sample_rate, APM_RATE);
        self.render_in
            .extend(resampled.iter().map(|&s| f32::from(s) / 32768.0));
        if self.render_in.len() > MAX_RENDER_BACKLOG {
            let excess = self.render_in.len() - MAX_RENDER_BACKLOG;
            self.render_in.drain(..excess);
        }
    }
}

/// Take channel 0 as the mono signal (adequate for a mic; avoids summing-clip).
fn downmix_f32(input: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return input.to_vec();
    }
    input.chunks(channels).map(|f| f[0]).collect()
}

fn downmix_i16(input: &[i16], channels: usize) -> Vec<f32> {
    let step = channels.max(1);
    input
        .iter()
        .step_by(step)
        .map(|&s| f32::from(s) / 32768.0)
        .collect()
}
