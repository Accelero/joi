//! Native mic capture with conditioning. The realtime cpal callback only downmixes to mono and
//! forwards raw samples; the capture thread resamples to 16 kHz and runs a WebRTC-style audio
//! processing module (Sonora: echo cancellation + noise suppression + AGC) before framing to 20 ms
//! PCM16 for the session. Gemini only does VAD, so a leveled, denoised, echo-free mic improves
//! detection and stops the model interrupting itself when playing through speakers.
//!
//! **AEC:** the playback engine forwards emitted device-rate samples into a bounded render
//! reference queue; capture drains that queue before APM and the APM subtracts it from the mic so
//! Joi's own voice picked up by the speakers doesn't read as user speech (which otherwise triggers
//! a barge-in feedback loop).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use joi_core::config::NoiseSuppressionMode;
use joi_core::media::{pcm16_from_f32, resample_linear, AudioFormat, FrameAccumulator};
use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression,
};
use sonora::{AudioProcessing, Config, StreamConfig as ApmStreamConfig};

#[cfg(debug_assertions)]
use crate::processed_mic_recorder::ProcessedMicRecorder;
use crate::MediaError;

/// APM runs at 16 kHz (a WebRTC-supported rate, and our provider rate); device audio is resampled
/// to it first. 10 ms frames = 160 samples.
const APM_RATE: u32 = AudioFormat::INPUT.sample_rate;
const APM_FRAME: usize = (APM_RATE / 100) as usize;
/// The neural denoiser processes 10 ms frames at 48 kHz.
const NEURAL_DENOISE_RATE: u32 = 48_000;
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
    pub rx: Arc<Mutex<Receiver<Vec<i16>>>>,
    /// Sample rate of those samples (the playback device rate).
    pub rate: u32,
}

/// Which APM (audio processing) stages to run on the mic. Each is independent so conditioning can be
/// moved to an OS/server APM (e.g. PipeWire's echo-cancel source) instead — turn the matching stage
/// off here to avoid double-processing.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct ApmConfig {
    /// Acoustic echo cancellation (needs the far-end render reference).
    pub echo_cancellation: bool,
    /// High-pass filter.
    pub high_pass_filter: bool,
    /// Noise suppression mode.
    pub noise_suppression: NoiseSuppressionMode,
    /// Fixed digital boost before the limiter.
    pub mic_boost_db: f32,
    /// AGC target headroom before clipping.
    pub agc_headroom_db: f32,
    /// Maximum adaptive digital gain.
    pub agc_max_gain_db: f32,
    /// Initial adaptive digital gain.
    pub agc_initial_gain_db: f32,
    /// Maximum AGC gain change rate.
    pub agc_gain_change_db_per_sec: f32,
    /// Automatic gain control (AGC2).
    pub auto_gain: bool,
    /// Final compressor/limiter before provider send.
    pub leveler_enabled: bool,
    /// Target RMS for the final leveler.
    pub leveler_target_rms_dbfs: f32,
    /// Maximum gain the final leveler may add.
    pub leveler_max_gain_db: f32,
    /// Maximum gain reduction the final leveler may apply.
    pub leveler_max_reduction_db: f32,
    /// Limiter ceiling for final samples.
    pub limiter_ceiling_dbfs: f32,
}

/// Spawn mic capture on its own thread (owns the `!Send` input stream and the APM). 16 kHz mono
/// PCM16 frames of `frame_samples` are pushed to `frames`, dropped on overflow. While `muted` is
/// set, no audio is captured (the manager separately tells the provider the stream paused).
/// Capture stops when the returned [`CaptureHandle`] is dropped.
/// `render` carries the provider audio Joi is actually emitting through the playback device — the
/// AEC far-end reference at the playback device rate. Capture drains the bounded queue and runs APM
/// before any frame reaches the rest of the program.
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
    // Muting drops frames here; the manager separately signals the provider that the
    // audio stream paused (`end_audio_stream`), so no silence needs to be streamed to keep healthy.
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
        high_pass_filter = apm.high_pass_filter,
        noise_suppression = ?apm.noise_suppression,
        mic_boost_db = apm.mic_boost_db,
        agc_headroom_db = apm.agc_headroom_db,
        agc_max_gain_db = apm.agc_max_gain_db,
        agc_initial_gain_db = apm.agc_initial_gain_db,
        agc_gain_change_db_per_sec = apm.agc_gain_change_db_per_sec,
        auto_gain = apm.auto_gain,
        leveler_enabled = apm.leveler_enabled,
        leveler_target_rms_dbfs = apm.leveler_target_rms_dbfs,
        leveler_max_gain_db = apm.leveler_max_gain_db,
        leveler_max_reduction_db = apm.leveler_max_reduction_db,
        limiter_ceiling_dbfs = apm.limiter_ceiling_dbfs,
        "native mic capture started"
    );

    let mut pipeline = CapturePipeline::new(device_rate, render.rate, frame_samples, apm);
    loop {
        // Check for stop on *every* iteration, not only on the recv timeout below. While the mic is
        // unmuted cpal delivers a buffer every frame, so `recv_timeout` keeps returning `Ok` and the
        // timeout branch almost never fires — if that were the only stop check, dropping the
        // `CaptureHandle` wouldn't end the loop and the thread (and its cpal stream) would leak on
        // every stop, with leaked threads stacking mic frames into the session on each reconnect.
        if matches!(stop_rx.try_recv(), Err(TryRecvError::Disconnected)) {
            break;
        }
        // Buffer the far-end reference (what we're playing); `process` consumes it 1:1 with capture
        // frames so the AEC sees both at the same cadence. The source is a bounded always-on queue
        // owned by the media engine, so stale render is capped and playback never blocks.
        pipeline.drain_render(render);
        match raw_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(mono) => {
                pipeline.diag.on_input(&mono, device_rate);
                pipeline.process(&mono, frames);
            }
            // Idle gap (e.g. muted): loop back and re-check the stop signal at the top.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
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
/// **Debug builds only.** Compiled behind `debug_assertions`, so a `cargo run` debug build gets it
/// but the release standalone binary gets the zero-cost no-op stubs below — none of the metering or
/// `tracing::debug!` calls are in the shipped binary. To see the meters, run a **debug** build with
/// `RUST_LOG=joi_media=debug cargo run -p joi-tui` — `RUST_LOG` only filters statements that exist,
/// so it cannot surface these in a release build where they are compiled out.
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
    render_backlog_peak: usize,
    render_backlog_dropped: u64,
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
            render_backlog_peak: 0,
            render_backlog_dropped: 0,
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
            let render_backlog_ms = self.render_backlog_ms();
            let render_backlog_dropped = self.drain_render_backlog_dropped();
            tracing::debug!(
                raw_dbfs,
                pre_apm_dbfs,
                post_apm_dbfs,
                render_dbfs,
                render_backlog_ms,
                render_backlog_dropped,
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

    fn on_render_backlog(&mut self, samples: usize) {
        self.render_backlog_peak = self.render_backlog_peak.max(samples);
    }

    fn on_render_backlog_drop(&mut self, samples: usize) {
        self.render_backlog_dropped = self.render_backlog_dropped.saturating_add(samples as u64);
    }

    fn render_backlog_ms(&mut self) -> u64 {
        let samples = self.render_backlog_peak;
        self.render_backlog_peak = 0;
        (samples as u64).saturating_mul(1000) / u64::from(APM_RATE)
    }

    fn drain_render_backlog_dropped(&mut self) -> u64 {
        let dropped = self.render_backlog_dropped;
        self.render_backlog_dropped = 0;
        dropped
    }
}

/// Release stub: zero-sized, all methods no-ops — the diagnostics aren't compiled into the binary.
#[cfg(not(debug_assertions))]
struct CaptureDiag;

#[cfg(not(debug_assertions))]
#[allow(clippy::unused_self)]
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
    #[inline]
    fn on_render_backlog(&mut self, _samples: usize) {}
    #[inline]
    fn on_render_backlog_drop(&mut self, _samples: usize) {}
}

/// Resample → APM (echo cancellation + noise suppression + AGC) → 20 ms PCM16 framing. Lives on the
/// capture thread, so the APM never crosses to the realtime callback. Both the near-end (mic) and
/// far-end (playback) streams are fed at 16 kHz in 10 ms APM frames; the echo canceller subtracts
/// the far-end from the near-end.
struct CapturePipeline {
    conditioning_apm: AudioProcessing,
    gain_apm: Option<AudioProcessing>,
    neural_denoiser: Option<NeuralDenoiser>,
    leveler: Option<OutputLeveler>,
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
    /// Optional diagnostic tap of post-APM audio, enabled by `JOI_PROCESSED_MIC_WAV`.
    #[cfg(debug_assertions)]
    recorder: Option<ProcessedMicRecorder>,
}

impl CapturePipeline {
    fn new(device_rate: u32, render_rate: u32, frame_samples: usize, apm: ApmConfig) -> Self {
        let echo_cancellation = apm.echo_cancellation;
        let conditioning_config = Config {
            // AEC3: remove Joi's own playback (picked up by the mic) so it doesn't read as speech.
            // Off when `audio.echo_cancellation = false` (e.g. headphones, or an OS APM does it).
            echo_canceller: apm.echo_cancellation.then(EchoCanceller::default),
            // High-pass filter: removes the DC bias / sub-audible rumble a raw mic (notably a
            // PDM/DMIC) carries — without it the DC dominates the RMS and buries speech.
            high_pass_filter: apm.high_pass_filter.then(HighPassFilter::default),
            // Classic noise suppression is mutually exclusive with neural cleanup by design; stacking
            // both tends to make speech sound watery.
            noise_suppression: matches!(apm.noise_suppression, NoiseSuppressionMode::Classic)
                .then(NoiseSuppression::default),
            ..Default::default()
        };
        let gain_config = Config {
            gain_controller2: (apm.auto_gain || apm.mic_boost_db > 0.0).then(|| agc2_config(&apm)),
            ..Default::default()
        };
        let sc = ApmStreamConfig::new(APM_RATE, 1);
        let conditioning_apm = AudioProcessing::builder()
            .config(conditioning_config)
            .capture_config(sc)
            .render_config(sc)
            .build();
        let gain_apm = (apm.auto_gain || apm.mic_boost_db > 0.0).then(|| {
            AudioProcessing::builder()
                .config(gain_config)
                .capture_config(sc)
                .build()
        });
        Self {
            conditioning_apm,
            gain_apm,
            neural_denoiser: matches!(apm.noise_suppression, NoiseSuppressionMode::Ai)
                .then(NeuralDenoiser::new),
            leveler: apm.leveler_enabled.then(|| OutputLeveler::new(&apm)),
            device_rate,
            render_rate,
            echo_cancellation,
            apm_in: Vec::new(),
            render_in: Vec::new(),
            out: FrameAccumulator::new(frame_samples),
            diag: CaptureDiag::new(echo_cancellation),
            #[cfg(debug_assertions)]
            recorder: ProcessedMicRecorder::from_env(),
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
            // silence when nothing is playing. Feeding it only during agent speech
            // drifts its render/capture alignment until it cancels the *near-end* (user) voice, i.e.
            // the mic stops getting through after a few turns. Pair one render frame (real, or
            // silence) with each capture frame.
            if self.echo_cancellation {
                let render_frame: Vec<f32> = if self.render_in.len() >= APM_FRAME {
                    self.render_in.drain(..APM_FRAME).collect()
                } else {
                    vec![0.0f32; APM_FRAME]
                };
                let mut render_out = vec![0.0f32; APM_FRAME];
                let _ = self
                    .conditioning_apm
                    .process_render_f32(&[&render_frame], &mut [&mut render_out]);
                self.diag.tap_render(&render_frame);
            }

            let frame: Vec<f32> = self.apm_in.drain(..APM_FRAME).collect();
            self.diag.tap_pre(&frame);
            let mut conditioned = vec![0.0f32; APM_FRAME];
            if self
                .conditioning_apm
                .process_capture_f32(&[&frame], &mut [&mut conditioned])
                .is_ok()
            {
                let denoised = if let Some(denoiser) = &mut self.neural_denoiser {
                    denoiser.process(&conditioned)
                } else {
                    conditioned
                };
                let gained = if let Some(gain_apm) = &mut self.gain_apm {
                    let mut gained = vec![0.0f32; APM_FRAME];
                    if gain_apm
                        .process_capture_f32(&[&denoised], &mut [&mut gained])
                        .is_ok()
                    {
                        gained
                    } else {
                        denoised
                    }
                } else {
                    denoised
                };
                let out = if let Some(leveler) = &mut self.leveler {
                    leveler.process(&gained)
                } else {
                    gained
                };
                self.diag.tap_post(&out);
                for done in self.out.push(&pcm16_from_f32(&out)) {
                    #[cfg(debug_assertions)]
                    if let Some(recorder) = &mut self.recorder {
                        recorder.write_samples(&done);
                    }
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
            self.diag.on_render_backlog_drop(excess);
        }
        self.diag.on_render_backlog(self.render_in.len());
    }

    fn drain_render(&mut self, render: &RenderRef) {
        let Ok(rx) = render.rx.lock() else {
            return;
        };
        while let Ok(chunk) = rx.try_recv() {
            self.buffer_render(&chunk);
        }
    }
}

struct NeuralDenoiser {
    state: Box<nnnoiseless::DenoiseState<'static>>,
    first_frame: bool,
}

impl NeuralDenoiser {
    fn new() -> Self {
        Self {
            state: nnnoiseless::DenoiseState::new(),
            first_frame: true,
        }
    }

    fn process(&mut self, frame_16k: &[f32]) -> Vec<f32> {
        let pcm_16k = pcm16_from_f32(frame_16k);
        let pcm_48k = resample_linear(&pcm_16k, APM_RATE, NEURAL_DENOISE_RATE);
        let mut input = [0.0f32; nnnoiseless::DenoiseState::FRAME_SIZE];
        for (dst, &sample) in input.iter_mut().zip(pcm_48k.iter()) {
            *dst = f32::from(sample);
        }

        let mut output = [0.0f32; nnnoiseless::DenoiseState::FRAME_SIZE];
        let _speech_probability = self.state.process_frame(&mut output, &input);
        if self.first_frame {
            self.first_frame = false;
            return vec![0.0; APM_FRAME];
        }

        let out_48k: Vec<i16> = output
            .iter()
            .map(|&s| s.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16)
            .collect();
        let out_16k = resample_linear(&out_48k, NEURAL_DENOISE_RATE, APM_RATE);
        let mut out: Vec<f32> = out_16k
            .into_iter()
            .map(|s| f32::from(s) / 32768.0)
            .collect();
        out.resize(APM_FRAME, 0.0);
        out.truncate(APM_FRAME);
        out
    }
}

struct OutputLeveler {
    target_rms_dbfs: f32,
    max_gain_db: f32,
    max_reduction_db: f32,
    limiter_ceiling: f32,
    gain_db: f32,
    initialized: bool,
    gain_up_coeff: f32,
    gain_down_coeff: f32,
}

impl OutputLeveler {
    const RMS_FLOOR_DBFS: f32 = -90.0;
    const INIT_GATE_DBFS: f32 = -75.0;
    const GAIN_UP_MS: f32 = 45.0;
    const GAIN_DOWN_MS: f32 = 10.0;

    fn new(apm: &ApmConfig) -> Self {
        Self {
            target_rms_dbfs: apm.leveler_target_rms_dbfs,
            max_gain_db: apm.leveler_max_gain_db,
            max_reduction_db: apm.leveler_max_reduction_db,
            limiter_ceiling: db_to_linear(apm.limiter_ceiling_dbfs),
            gain_db: 0.0,
            initialized: false,
            gain_up_coeff: smoothing_coeff(Self::GAIN_UP_MS),
            gain_down_coeff: smoothing_coeff(Self::GAIN_DOWN_MS),
        }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let rms_dbfs = rms_dbfs(input);
        let desired_db = if rms_dbfs <= Self::RMS_FLOOR_DBFS {
            0.0
        } else {
            (self.target_rms_dbfs - rms_dbfs).clamp(-self.max_reduction_db, self.max_gain_db)
        };

        if !self.initialized && rms_dbfs > Self::INIT_GATE_DBFS {
            self.gain_db = desired_db;
            self.initialized = true;
        } else {
            let coeff = if desired_db < self.gain_db {
                self.gain_down_coeff
            } else {
                self.gain_up_coeff
            };
            self.gain_db = desired_db + (self.gain_db - desired_db) * coeff;
        }

        let gain = db_to_linear(self.gain_db);
        input
            .iter()
            .map(|&s| (s * gain).clamp(-self.limiter_ceiling, self.limiter_ceiling))
            .collect()
    }
}

fn rms_dbfs(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return OutputLeveler::RMS_FLOOR_DBFS;
    }
    let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
    let rms = (sum_sq / frame.len() as f32).sqrt();
    if rms <= 1e-9 {
        OutputLeveler::RMS_FLOOR_DBFS
    } else {
        20.0 * rms.log10()
    }
}

fn smoothing_coeff(ms: f32) -> f32 {
    let frame_ms = 10.0;
    (-frame_ms / ms).exp()
}

fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn agc2_config(apm: &ApmConfig) -> GainController2 {
    GainController2 {
        adaptive_digital: apm.auto_gain.then(|| AdaptiveDigital {
            headroom_db: apm.agc_headroom_db,
            max_gain_db: apm.agc_max_gain_db,
            initial_gain_db: apm.agc_initial_gain_db,
            max_gain_change_db_per_second: apm.agc_gain_change_db_per_sec,
            ..Default::default()
        }),
        fixed_digital: sonora::config::FixedDigital {
            gain_db: apm.mic_boost_db,
        },
        ..Default::default()
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn test_apm(auto_gain: bool, mic_boost_db: f32) -> ApmConfig {
        ApmConfig {
            echo_cancellation: false,
            high_pass_filter: true,
            noise_suppression: NoiseSuppressionMode::Off,
            mic_boost_db,
            agc_headroom_db: 1.0,
            agc_max_gain_db: 60.0,
            agc_initial_gain_db: 30.0,
            agc_gain_change_db_per_sec: 24.0,
            auto_gain,
            leveler_enabled: true,
            leveler_target_rms_dbfs: -18.0,
            leveler_max_gain_db: 24.0,
            leveler_max_reduction_db: 36.0,
            limiter_ceiling_dbfs: -1.0,
        }
    }

    #[test]
    fn auto_gain_config_enables_adaptive_digital_gain() {
        let agc = agc2_config(&test_apm(true, 0.0));
        assert!(agc.adaptive_digital.is_some());
        if let Some(adaptive) = agc.adaptive_digital {
            assert!((adaptive.headroom_db - 1.0).abs() < f32::EPSILON);
            assert!((adaptive.max_gain_db - 60.0).abs() < f32::EPSILON);
            assert!((adaptive.initial_gain_db - 30.0).abs() < f32::EPSILON);
            assert!((adaptive.max_gain_change_db_per_second - 24.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn mic_boost_config_enables_fixed_digital_gain() {
        let agc = agc2_config(&test_apm(false, 12.0));
        assert!(agc.adaptive_digital.is_none());
        assert!((agc.fixed_digital.gain_db - 12.0).abs() < f32::EPSILON);
    }

    #[test]
    fn output_leveler_reduces_hot_audio_to_limiter_ceiling() {
        let mut apm = test_apm(false, 0.0);
        apm.limiter_ceiling_dbfs = -6.0;
        let mut leveler = OutputLeveler::new(&apm);
        let out = leveler.process(&[2.0; APM_FRAME]);
        let ceiling = db_to_linear(-6.0);
        assert!(out.iter().all(|s| s.abs() <= ceiling + f32::EPSILON));
    }

    #[test]
    fn output_leveler_bootstraps_quiet_speech_gain() {
        let mut apm = test_apm(false, 0.0);
        apm.leveler_target_rms_dbfs = -20.0;
        apm.leveler_max_gain_db = 18.0;
        let mut leveler = OutputLeveler::new(&apm);
        let sample = db_to_linear(-45.0);

        let out = leveler.process(&[sample; APM_FRAME]);

        assert!(out[0] > sample * db_to_linear(17.0));
    }
}
