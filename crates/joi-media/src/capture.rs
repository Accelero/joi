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
use sonora::config::{EchoCanceller, GainController2, HighPassFilter, NoiseSuppression};
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
/// Emit a mic-level diagnostic line once per this many APM frames (1 s = 100 × 10 ms frames).
const LEVEL_LOG_FRAMES: usize = APM_RATE as usize / APM_FRAME;

/// Stops capture when dropped — the input stream + processing loop live on a dedicated thread that
/// exits once this handle's sender is dropped.
pub struct CaptureHandle {
    _stop: Sender<()>,
}

/// The AEC far-end reference: the playback engine's just-emitted samples (`rx`) and the rate they
/// arrive at (`rate`, the playback device rate). Capture resamples them to the APM rate.
pub struct RenderRef {
    /// Stream of emitted playback samples, forwarded from the playback output callback.
    pub rx: Receiver<Vec<i16>>,
    /// Sample rate of those samples (the playback device rate).
    pub rate: u32,
}

/// Which APM (audio processing) stages to run on the mic. Each is independent so conditioning can be
/// moved to an OS/server APM (e.g. PipeWire's echo-cancel source) instead — turn the matching stage
/// off here to avoid double-processing.
#[derive(Debug, Clone, Copy)]
pub struct ApmConfig {
    /// Acoustic echo cancellation (needs the far-end render reference).
    pub echo_cancellation: bool,
    /// Noise suppression.
    pub noise_suppression: bool,
    /// Automatic gain control (AGC2).
    pub auto_gain: bool,
}

/// Spawn mic capture on its own thread (owns the `!Send` input stream and the APM). 16 kHz mono
/// PCM16 frames of `frame_samples` are pushed to `frames`, dropped on overflow. While `muted` is
/// set, no audio is captured. Capture stops when the returned [`CaptureHandle`] is dropped.
/// `render_rx` carries the provider audio Joi is playing — the AEC far-end reference (24 kHz PCM16,
/// [`AudioFormat::OUTPUT`]). It ends when the engine clears the render sink on stop.
#[must_use]
pub fn spawn_capture(
    device_name: String,
    frames: tokio::sync::mpsc::Sender<Vec<i16>>,
    frame_samples: usize,
    muted: Arc<AtomicBool>,
    render: RenderRef,
    apm: ApmConfig,
) -> CaptureHandle {
    let (stop_tx, stop_rx) = channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("joi-capture".to_string())
        .spawn(move || {
            if let Err(e) = run_capture(
                &device_name,
                &frames,
                frame_samples,
                &muted,
                &stop_rx,
                &render,
                apm,
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
    device_name: &str,
    frames: &tokio::sync::mpsc::Sender<Vec<i16>>,
    frame_samples: usize,
    muted: &Arc<AtomicBool>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    render: &RenderRef,
    apm: ApmConfig,
) -> Result<(), MediaError> {
    let host = cpal::default_host();
    let device = pick_input_device(&host, device_name).ok_or(MediaError::NoInputDevice)?;
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
        echo_cancellation = apm.echo_cancellation,
        noise_suppression = apm.noise_suppression,
        auto_gain = apm.auto_gain,
        "native mic capture started"
    );

    let mut pipeline = CapturePipeline::new(device_rate, render.rate, frame_samples, apm);
    // DIAGNOSTIC: measure the *actual* mono input rate vs the rate we assume for resampling. If the
    // device delivers a different rate than `default_input_config` reported (e.g. a PipeWire virtual
    // source negotiated at 44.1 k but feeding 48 k), every resample is wrong and the audio sent to
    // the provider is pitch/speed-distorted — i.e. unintelligible. Logged once per ~second.
    let mut rx_samples: u64 = 0;
    let mut rx_since = std::time::Instant::now();
    loop {
        // Buffer the far-end reference (what we're playing); `process` consumes it 1:1 with capture
        // frames so the AEC sees both at the same cadence. Drains all that's queued; ends quietly
        // once the render sink is dropped on stop.
        while let Ok(chunk) = render.rx.try_recv() {
            pipeline.buffer_render(&chunk);
        }
        match raw_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(mono) => {
                rx_samples += mono.len() as u64;
                let elapsed = rx_since.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    #[allow(clippy::cast_precision_loss)]
                    let actual_rate = rx_samples as f64 / elapsed.as_secs_f64();
                    tracing::debug!(
                        assumed_rate = device_rate,
                        actual_rate,
                        "mic input rate (mono samples/s)"
                    );
                    rx_samples = 0;
                    rx_since = std::time::Instant::now();
                }
                pipeline.process(&mono, frames);
            }
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

/// Accumulates sum-of-squares to report a signal's mean RMS level in dBFS, for capture diagnostics.
/// Samples are full-scale floats (±1.0), so 0 dBFS is full scale; speech typically lands around
/// −30…−15 dBFS and a silent/dead signal floors at [`LevelMeter::FLOOR_DBFS`].
#[derive(Default)]
struct LevelMeter {
    sum_sq: f64,
    samples: u64,
}

impl LevelMeter {
    /// Reported level when no signal is present, so `log10` never goes to −∞.
    const FLOOR_DBFS: f32 = -120.0;

    /// Fold a chunk of full-scale float samples into the running RMS.
    fn add(&mut self, frame: &[f32]) {
        for &s in frame {
            self.sum_sq += f64::from(s) * f64::from(s);
        }
        self.samples += frame.len() as u64;
    }

    /// Mean RMS over the accumulated samples as dBFS, then reset. Empty/silent → [`Self::FLOOR_DBFS`].
    fn drain_dbfs(&mut self) -> f32 {
        let dbfs = if self.samples == 0 {
            Self::FLOOR_DBFS
        } else {
            #[allow(clippy::cast_precision_loss)]
            // sample counts stay well within f64's exact range
            let rms = (self.sum_sq / self.samples as f64).sqrt();
            if rms <= 1e-9 {
                Self::FLOOR_DBFS
            } else {
                20.0 * (rms as f32).log10()
            }
        };
        self.sum_sq = 0.0;
        self.samples = 0;
        dbfs
    }
}

/// Resample → APM (echo cancellation + noise suppression + AGC) → 20 ms PCM16 framing. Lives on the
/// capture thread, so the APM (which is `!Send`-agnostic here) never crosses to the realtime
/// callback. Both the near-end (mic) and far-end (playback) streams are fed at 16 kHz in 10 ms APM
/// frames; the echo canceller subtracts the far-end from the near-end.
struct CapturePipeline {
    apm: AudioProcessing,
    device_rate: u32,
    /// Sample rate of the far-end (render) reference as delivered by the playback engine — its
    /// device rate. Resampled to [`APM_RATE`] before feeding the echo canceller.
    render_rate: u32,
    /// Whether the echo canceller is enabled (config `audio.echo_cancellation`). When off, the
    /// far-end reference isn't fed and no echo is subtracted.
    echo_cancellation: bool,
    /// Near-end (mic) samples awaiting a full 10 ms APM frame.
    apm_in: Vec<f32>,
    /// Far-end (render/playback) samples awaiting a full 10 ms APM frame.
    render_in: Vec<f32>,
    out: FrameAccumulator,
    /// Diagnostic RMS taps: raw mic in, APM input (post-resample), APM output, far-end reference.
    /// Flushed to a `debug!` line every [`LEVEL_LOG_FRAMES`] APM frames (~1 s).
    lvl_raw: LevelMeter,
    lvl_pre_apm: LevelMeter,
    lvl_post_apm: LevelMeter,
    lvl_render: LevelMeter,
    /// APM frames processed since the last level log.
    frames_since_log: usize,
}

impl CapturePipeline {
    fn new(device_rate: u32, render_rate: u32, frame_samples: usize, apm: ApmConfig) -> Self {
        let echo_cancellation = apm.echo_cancellation;
        let config = Config {
            // AEC3: remove Joi's own playback (picked up by the mic) so it doesn't read as speech.
            // Off when `audio.echo_cancellation = false` (e.g. headphones, or an OS APM does it).
            echo_canceller: apm.echo_cancellation.then(EchoCanceller::default),
            // High-pass filter: removes the DC bias / sub-audible rumble a raw mic (notably a
            // PDM/DMIC) carries — without it the DC dominates the RMS and buries speech. Coupled with
            // NS, the conventional WebRTC pre-conditioning grouping.
            high_pass_filter: apm.noise_suppression.then(HighPassFilter::default),
            // NS/AGC are independently switchable: turn them off when an echo-aware OS APM does the
            // conditioning, so a co-located AGC isn't echo-blind here (which amplifies residual echo).
            noise_suppression: apm.noise_suppression.then(NoiseSuppression::default),
            gain_controller2: apm.auto_gain.then(GainController2::default),
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
            render_rate,
            echo_cancellation,
            apm_in: Vec::new(),
            render_in: Vec::new(),
            out: FrameAccumulator::new(frame_samples),
            lvl_raw: LevelMeter::default(),
            lvl_pre_apm: LevelMeter::default(),
            lvl_post_apm: LevelMeter::default(),
            lvl_render: LevelMeter::default(),
            frames_since_log: 0,
        }
    }

    fn process(&mut self, mono_device: &[f32], frames: &tokio::sync::mpsc::Sender<Vec<i16>>) {
        // Down to 16 kHz, then accumulate 10 ms APM frames.
        self.lvl_raw.add(mono_device);
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
                self.lvl_render.add(&render_frame);
            }

            let frame: Vec<f32> = self.apm_in.drain(..APM_FRAME).collect();
            self.lvl_pre_apm.add(&frame);
            let mut out = vec![0.0f32; APM_FRAME];
            if self
                .apm
                .process_capture_f32(&[&frame], &mut [&mut out])
                .is_ok()
            {
                self.lvl_post_apm.add(&out);
                for done in self.out.push(&pcm16_from_f32(&out)) {
                    let _ = frames.try_send(done);
                }
            }

            self.frames_since_log += 1;
            if self.frames_since_log >= LEVEL_LOG_FRAMES {
                self.log_levels();
            }
        }
    }

    /// Emit the accumulated RMS taps as one diagnostic line and reset the window. Lets us see at a
    /// glance whether the mic is too quiet for Gemini's VAD (low `raw`/`pre_apm`) or the APM — most
    /// likely AEC on speakers — is suppressing the user's voice (`post_apm` far below `pre_apm`).
    /// Enable with `RUST_LOG=joi_media=debug` (or `logging.level: debug`).
    fn log_levels(&mut self) {
        let raw_dbfs = self.lvl_raw.drain_dbfs();
        let pre_apm_dbfs = self.lvl_pre_apm.drain_dbfs();
        let post_apm_dbfs = self.lvl_post_apm.drain_dbfs();
        self.frames_since_log = 0;
        if self.echo_cancellation {
            let render_dbfs = self.lvl_render.drain_dbfs();
            tracing::debug!(
                raw_dbfs,
                pre_apm_dbfs,
                post_apm_dbfs,
                render_dbfs,
                "mic levels (dBFS)"
            );
        } else {
            tracing::debug!(
                raw_dbfs,
                pre_apm_dbfs,
                post_apm_dbfs,
                "mic levels (dBFS, AEC off)"
            );
        }
    }

    /// Buffer the far-end (playback) reference for the echo canceller — the samples the playback
    /// engine just emitted, at its device rate ([`Self::render_rate`]), resampled to the 16 kHz APM
    /// rate. It is consumed 1:1 with capture frames in [`process`](Self::process); here we only
    /// enqueue it. The backlog is capped so that a transient burst keeps the far-end lead within
    /// AEC3's delay-tracking range instead of growing unboundedly.
    fn buffer_render(&mut self, render: &[i16]) {
        if !self.echo_cancellation {
            return; // AEC off: no reference needed; drained frames are simply dropped.
        }
        let resampled = resample_linear(render, self.render_rate, APM_RATE);
        self.render_in
            .extend(resampled.iter().map(|&s| f32::from(s) / 32768.0));
        if self.render_in.len() > MAX_RENDER_BACKLOG {
            let excess = self.render_in.len() - MAX_RENDER_BACKLOG;
            self.render_in.drain(..excess);
        }
    }
}

/// Resolve the configured input-device name to a cpal device. `"default"` uses the host default;
/// any other name selects the first input device whose cpal name matches exactly, falling back to
/// the default (with a warning) if none matches — a stale name must never silently kill capture.
/// The available names are logged once at `info` so the exact strings to put in config are
/// discoverable: **cpal's device names differ from PipeWire/`pactl` node names.**
fn pick_input_device(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    let available: Vec<(String, cpal::Device)> = host
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok().map(|n| (n, d))).collect())
        .unwrap_or_default();
    let names: Vec<&str> = available.iter().map(|(n, _)| n.as_str()).collect();
    tracing::info!(requested = name, available = ?names, "available input devices");
    if name != "default" {
        if let Some((_, dev)) = available.into_iter().find(|(n, _)| n == name) {
            return Some(dev);
        }
        tracing::warn!(
            requested = name,
            "input_device not found; using host default"
        );
    }
    host.default_input_device()
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
