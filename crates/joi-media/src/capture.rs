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
#[cfg(debug_assertions)]
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
/// set, **silence** is captured instead of the mic (no user audio leaves the device, but the frame
/// cadence is preserved — see the realtime callback). Capture stops when the returned
/// [`CaptureHandle`] is dropped.
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

    // Realtime callback: downmix to mono f32 (or emit equal-length silence when muted), forward. No
    // heavy DSP on the audio thread. Muting feeds silence rather than dropping the frame so the
    // provider's realtime stream and the AEC render cadence stay continuous — halting the upstream
    // audio entirely disrupts the session's VAD/turn detection (the model's output cuts out).
    let (raw_tx, raw_rx) = channel::<Vec<f32>>();
    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let raw_tx = raw_tx.clone();
            let muted = Arc::clone(muted);
            device.build_input_stream(
                &config,
                move |input: &[f32], _| {
                    let mono = if muted.load(Ordering::Relaxed) {
                        vec![0.0; input.len() / channels.max(1)]
                    } else {
                        downmix_f32(input, channels)
                    };
                    let _ = raw_tx.send(mono);
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
                    let mono = if muted.load(Ordering::Relaxed) {
                        vec![0.0; input.len() / channels.max(1)]
                    } else {
                        downmix_i16(input, channels)
                    };
                    let _ = raw_tx.send(mono);
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
    loop {
        // Buffer the far-end reference (what we're playing); `process` consumes it 1:1 with capture
        // frames so the AEC sees both at the same cadence. Drains all that's queued; ends quietly
        // once the render sink is dropped on stop.
        while let Ok(chunk) = render.rx.try_recv() {
            pipeline.buffer_render(&chunk);
        }
        match raw_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(mono) => {
                pipeline.diag.on_input(&mono, device_rate);
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
#[cfg(debug_assertions)]
#[derive(Default)]
struct LevelMeter {
    sum_sq: f64,
    samples: u64,
}

#[cfg(debug_assertions)]
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

/// Per-second capture diagnostics: mic RMS levels (raw / pre-APM / post-APM / far-end reference)
/// and the actual input sample rate vs the assumed one.
///
/// **Debug builds only.** Compiled behind `debug_assertions`, so `cargo run`/`tauri dev` get it but
/// the release standalone (`tauri build`) gets the zero-cost no-op stubs below — none of the
/// metering or `tracing::debug!` calls are in the shipped binary. View output with
/// `RUST_LOG=joi_media=debug`.
#[cfg(debug_assertions)]
struct CaptureDiag {
    echo_cancellation: bool,
    raw: LevelMeter,
    pre_apm: LevelMeter,
    post_apm: LevelMeter,
    render: LevelMeter,
    frames_since_log: usize,
    rx_samples: u64,
    rx_since: std::time::Instant,
}

#[cfg(debug_assertions)]
impl CaptureDiag {
    fn new(echo_cancellation: bool) -> Self {
        Self {
            echo_cancellation,
            raw: LevelMeter::default(),
            pre_apm: LevelMeter::default(),
            post_apm: LevelMeter::default(),
            render: LevelMeter::default(),
            frames_since_log: 0,
            rx_samples: 0,
            rx_since: std::time::Instant::now(),
        }
    }

    fn tap_raw(&mut self, mono: &[f32]) {
        self.raw.add(mono);
    }
    fn tap_render(&mut self, frame: &[f32]) {
        self.render.add(frame);
    }
    fn tap_pre(&mut self, frame: &[f32]) {
        self.pre_apm.add(frame);
    }
    fn tap_post(&mut self, out: &[f32]) {
        self.post_apm.add(out);
    }

    /// Count one processed APM frame; emit the levels line every [`LEVEL_LOG_FRAMES`] (~1 s). Low
    /// `raw`/`pre_apm` ⇒ mic too quiet for VAD; `post_apm` far below `pre_apm` ⇒ APM (likely AEC) is
    /// suppressing the user's voice; `render` rises when Joi plays (the AEC reference is live).
    fn on_apm_frame(&mut self) {
        self.frames_since_log += 1;
        if self.frames_since_log < LEVEL_LOG_FRAMES {
            return;
        }
        self.frames_since_log = 0;
        let raw_dbfs = self.raw.drain_dbfs();
        let pre_apm_dbfs = self.pre_apm.drain_dbfs();
        let post_apm_dbfs = self.post_apm.drain_dbfs();
        if self.echo_cancellation {
            let render_dbfs = self.render.drain_dbfs();
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

    /// Accumulate received mono samples; once per second log actual vs assumed input rate. A
    /// mismatch means the device feeds a different rate than we resample for → distorted audio.
    fn on_input(&mut self, mono: &[f32], assumed_rate: u32) {
        self.rx_samples += mono.len() as u64;
        let elapsed = self.rx_since.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        #[allow(clippy::cast_precision_loss)] // sample counts stay well within f64's exact range
        let actual_rate = self.rx_samples as f64 / elapsed.as_secs_f64();
        tracing::debug!(assumed_rate, actual_rate, "mic input rate (mono samples/s)");
        self.rx_samples = 0;
        self.rx_since = std::time::Instant::now();
    }
}

/// Release stub: zero-sized, all methods no-ops — the diagnostics aren't compiled into the binary.
#[cfg(not(debug_assertions))]
struct CaptureDiag;

#[cfg(not(debug_assertions))]
impl CaptureDiag {
    #[inline]
    fn new(_echo_cancellation: bool) -> Self {
        Self
    }
    #[inline]
    fn tap_raw(&mut self, _mono: &[f32]) {}
    #[inline]
    fn tap_render(&mut self, _frame: &[f32]) {}
    #[inline]
    fn tap_pre(&mut self, _frame: &[f32]) {}
    #[inline]
    fn tap_post(&mut self, _out: &[f32]) {}
    #[inline]
    fn on_apm_frame(&mut self) {}
    #[inline]
    fn on_input(&mut self, _mono: &[f32], _assumed_rate: u32) {}
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
    /// Per-second level/rate diagnostics (debug builds only; no-op in release).
    diag: CaptureDiag,
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
            diag: CaptureDiag::new(echo_cancellation),
        }
    }

    fn process(&mut self, mono_device: &[f32], frames: &tokio::sync::mpsc::Sender<Vec<i16>>) {
        // Down to 16 kHz, then accumulate 10 ms APM frames.
        self.diag.tap_raw(mono_device);
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
                self.diag.tap_render(&render_frame);
            }

            let frame: Vec<f32> = self.apm_in.drain(..APM_FRAME).collect();
            self.diag.tap_pre(&frame);
            let mut out = vec![0.0f32; APM_FRAME];
            if self
                .apm
                .process_capture_f32(&[&frame], &mut [&mut out])
                .is_ok()
            {
                self.diag.tap_post(&out);
                for done in self.out.push(&pcm16_from_f32(&out)) {
                    let _ = frames.try_send(done);
                }
            }

            self.diag.on_apm_frame();
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
