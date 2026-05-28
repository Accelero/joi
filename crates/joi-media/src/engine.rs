//! [`MediaEngine`]: the single interface the composition root (`joi-app`) uses for all native
//! media. It owns the capture/playback/screen lifecycle and the pumps that move frames between the
//! cpal/xcap threads and the session, so `JoiApp` stays thin. The
//! [`SessionManagerHandle`] it binds to is the only seam it touches — the engine drives the mic,
//! speaker, and screen on the host machine.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use joi_core::config::NoiseSuppressionMode;
use joi_core::manager::SessionManagerHandle;
use joi_core::media::{AudioFormat, VideoFrame};
use tokio::sync::broadcast::error::RecvError;

use crate::capture::{spawn_capture, ApmConfig, CaptureHandle, RenderRef};
use crate::playback::{spawn_playback, PlaybackCmd};
use crate::screen::{spawn_screen_capture, ScreenHandle};

/// Bounded, stable sink for the echo-cancellation render reference: the provider audio Joi is
/// actually emitting through playback. Playback writes from its realtime callback with `try_send`
/// only while this sink is atomically enabled; capture drains it on its processing thread before
/// feeding AEC. Keeping this stable avoids a session-lifecycle mutex in the playback callback and
/// makes stale-reference latency bounded.
#[derive(Clone)]
pub(crate) struct RenderSink {
    tx: std::sync::mpsc::SyncSender<Vec<i16>>,
    enabled: Arc<AtomicBool>,
}

impl RenderSink {
    pub(crate) fn new(tx: std::sync::mpsc::SyncSender<Vec<i16>>) -> Self {
        Self {
            tx,
            enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn try_send(&self, samples: &[i16]) {
        if self.enabled.load(Ordering::Relaxed) {
            let _ = self.tx.try_send(samples.to_vec());
        }
    }
}

pub(crate) type RenderSource = Arc<Mutex<std::sync::mpsc::Receiver<Vec<i16>>>>;

/// About 200 ms worth of typical 44.1/48 kHz callback chunks. Overflow drops render reference rather
/// than blocking playback; capture also caps the post-resample APM backlog.
const RENDER_QUEUE_CHUNKS: usize = 64;

/// Native-media settings, sourced from `Config` by the composition root.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct MediaConfig {
    /// Mic capture device name, or `"default"`. A specific name lets Joi bypass a flaky/virtual
    /// system-default source (e.g. a PipeWire `echo-cancel-source`) and own its own audio path.
    pub input_device: String,
    /// Playback device name, or `"default"`. Pair with `input_device` to keep the in-app AEC's
    /// far-end reference equal to what's actually played (don't route output through a separate APM).
    pub output_device: String,
    /// Samples per mic frame (e.g. 320 at 16 kHz / 20 ms).
    pub frame_samples: usize,
    /// Screen-capture frame rate.
    pub screen_fps: f32,
    /// Longest-edge cap for captured frames before encoding.
    pub screen_max_width: u32,
    /// JPEG encode quality, 1–100.
    pub screen_quality: u8,
    /// Acoustic echo cancellation on the mic (subtract Joi's own playback). Off → no far-end
    /// reference is wired and the canceller is disabled.
    pub echo_cancellation: bool,
    /// High-pass filter on the mic.
    pub high_pass_filter: bool,
    /// Noise suppression mode on the mic.
    pub noise_suppression: NoiseSuppressionMode,
    /// AGC target headroom before clipping.
    pub agc_headroom_db: f32,
    /// Maximum adaptive digital gain.
    pub agc_max_gain_db: f32,
    /// Initial adaptive digital gain.
    pub agc_initial_gain_db: f32,
    /// Maximum AGC gain change rate.
    pub agc_gain_change_db_per_sec: f32,
    /// Automatic gain control on the mic.
    pub auto_gain: bool,
    /// Final compressor/limiter before provider send.
    pub leveler_enabled: bool,
    /// Target RMS for the final leveler.
    pub leveler_target_rms_dbfs: f32,
    /// Maximum gain the final leveler may add.
    pub leveler_max_gain_db: f32,
    /// Maximum gain reduction the final leveler may apply.
    pub leveler_max_reduction_db: f32,
    /// Compressor gain-up/attack time in milliseconds.
    pub leveler_gain_up_ms: f32,
    /// Limiter ceiling for final samples.
    pub limiter_ceiling_dbfs: f32,
}

/// Owns all native media for one app instance. Construct once with [`MediaEngine::new`] (within a
/// Tokio runtime); drive it with the lifecycle methods from `JoiApp`. `Send + Sync`.
pub struct MediaEngine {
    config: MediaConfig,
    /// Mic frames are pushed here by the capture thread and drained into the session.
    cap_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    /// Active mic capture (present only while a session runs); dropping it stops the stream.
    capture: Mutex<Option<CaptureHandle>>,
    /// App-level mute: gates native capture at the source, independent of session state.
    mic_muted: Arc<AtomicBool>,
    /// Screen frames are pushed here by the capture thread and drained into the session.
    frame_tx: tokio::sync::mpsc::Sender<VideoFrame>,
    /// Active screen capture (present only while sharing); dropping it stops capture.
    screen: Mutex<Option<ScreenHandle>>,
    /// Echo-cancellation reference sink used by playback (see [`RenderSink`]).
    render_sink: RenderSink,
    /// Echo-cancellation reference source drained by active capture.
    render_source: RenderSource,
    /// Playback device sample rate, published by the playback engine once its stream opens. The
    /// AEC reference is tapped at the playback output at this rate; capture resamples it to the APM
    /// rate. `0` until playback has started.
    playback_rate: Arc<AtomicU32>,
}

impl MediaEngine {
    /// Build the engine bound to `handle` and spawn its always-on pumps: provider audio → native
    /// playback, captured mic frames → session, captured screen frames → session. Must be called
    /// within a Tokio runtime context (the pumps use [`tokio::spawn`]).
    #[must_use]
    pub fn new(handle: SessionManagerHandle, config: MediaConfig) -> Self {
        let (cap_tx, cap_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(64);
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<VideoFrame>(8);
        let mic_muted = Arc::new(AtomicBool::new(false));
        let (render_tx, render_rx) = std::sync::mpsc::sync_channel(RENDER_QUEUE_CHUNKS);
        let render_sink = RenderSink::new(render_tx);
        let render_source: RenderSource = Arc::new(Mutex::new(render_rx));
        let playback_rate = Arc::new(AtomicU32::new(0));

        spawn_playback_pump(
            &handle,
            config.output_device.clone(),
            render_sink.clone(),
            Arc::clone(&playback_rate),
        );
        spawn_audio_drain(handle.clone(), cap_rx);
        spawn_frame_drain(handle, frame_rx);

        Self {
            config,
            cap_tx,
            capture: Mutex::new(None),
            mic_muted,
            frame_tx,
            screen: Mutex::new(None),
            render_sink,
            render_source,
            playback_rate,
        }
    }

    /// Start native mic capture for a session. **Idempotent**: if capture is already running this
    /// is a no-op (the stream is not torn down and respawned). The manager and the mute gate both
    /// drop muted audio.
    pub fn start_capture(&self) {
        // Hold `capture` across the whole operation so concurrent calls can't both spawn. Keep the
        // lock order capture -> render_sink, matching `stop_capture`, to avoid a lifecycle deadlock.
        if let Ok(mut cap) = self.capture.lock() {
            if cap.is_some() {
                return; // already capturing
            }
            drain_render_source(&self.render_source);
            self.render_sink.set_enabled(self.config.echo_cancellation);
            // The AEC reference arrives at the playback device rate (tapped at the output callback);
            // capture resamples it to the APM rate. Fall back to the provider rate if playback hasn't
            // published its rate yet (no playback => no reference anyway).
            let render_rate = match self.playback_rate.load(Ordering::Relaxed) {
                0 => AudioFormat::OUTPUT.sample_rate,
                r => r,
            };
            *cap = Some(spawn_capture(
                self.config.input_device.clone(),
                self.cap_tx.clone(),
                self.config.frame_samples,
                Arc::clone(&self.mic_muted),
                RenderRef {
                    rx: Arc::clone(&self.render_source),
                    rate: render_rate,
                },
                ApmConfig {
                    echo_cancellation: self.config.echo_cancellation,
                    high_pass_filter: self.config.high_pass_filter,
                    noise_suppression: self.config.noise_suppression,
                    agc_headroom_db: self.config.agc_headroom_db,
                    agc_max_gain_db: self.config.agc_max_gain_db,
                    agc_initial_gain_db: self.config.agc_initial_gain_db,
                    agc_gain_change_db_per_sec: self.config.agc_gain_change_db_per_sec,
                    auto_gain: self.config.auto_gain,
                    leveler_enabled: self.config.leveler_enabled,
                    leveler_target_rms_dbfs: self.config.leveler_target_rms_dbfs,
                    leveler_max_gain_db: self.config.leveler_max_gain_db,
                    leveler_max_reduction_db: self.config.leveler_max_reduction_db,
                    leveler_gain_up_ms: self.config.leveler_gain_up_ms,
                    limiter_ceiling_dbfs: self.config.limiter_ceiling_dbfs,
                },
            ));
        }
    }

    /// Stop native mic capture. Idempotent (no-op if not capturing).
    pub fn stop_capture(&self) {
        // Same lock order as `start_capture` (capture → render_sink) to avoid deadlock.
        if let Ok(mut cap) = self.capture.lock() {
            *cap = None;
        }
        self.render_sink.set_enabled(false);
        drain_render_source(&self.render_source);
    }

    /// App-level mute: gates native capture at the source, regardless of session state.
    pub fn set_mic_muted(&self, muted: bool) {
        self.mic_muted.store(muted, Ordering::Relaxed);
    }

    /// Start native screen capture of the primary monitor; frames flow to the session.
    /// **Idempotent**: a no-op if already sharing (no duplicate capture thread / doubled frames).
    pub fn start_screenshare(&self) {
        if let Ok(mut screen) = self.screen.lock() {
            if screen.is_some() {
                return; // already sharing
            }
            *screen = Some(spawn_screen_capture(
                self.frame_tx.clone(),
                self.config.screen_fps,
                self.config.screen_max_width,
                self.config.screen_quality,
            ));
        }
    }

    /// Stop native screen capture (no-op if not sharing).
    pub fn stop_screenshare(&self) {
        if let Ok(mut screen) = self.screen.lock() {
            *screen = None;
        }
    }
}

/// Provider audio → native cpal playback. The manager's empty-frame is the barge-in sentinel →
/// flush the playback buffer immediately (FR-2/FR-7). The AEC reference is **not** fed here: provider
/// audio arrives in bursts, but the echo the mic hears is the jitter-buffered, real-time playback —
/// so the reference is tapped inside the playback engine at the output callback (what's actually
/// emitted), keeping it aligned with the near-end echo for AEC3.
fn spawn_playback_pump(
    handle: &SessionManagerHandle,
    output_device: String,
    render_sink: RenderSink,
    playback_rate: Arc<AtomicU32>,
) {
    let playback_tx = spawn_playback(output_device, render_sink, playback_rate);
    let mut audio_rx = handle.subscribe_audio();
    tokio::spawn(async move {
        loop {
            match audio_rx.recv().await {
                Ok(pcm) if pcm.is_empty() => {
                    if playback_tx.send(PlaybackCmd::Flush).is_err() {
                        break;
                    }
                }
                Ok(pcm) => {
                    if playback_tx.send(PlaybackCmd::Pcm(pcm)).is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
                Err(RecvError::Lagged(_)) => {}
            }
        }
    });
}

fn drain_render_source(render_source: &RenderSource) {
    if let Ok(rx) = render_source.lock() {
        while rx.try_recv().is_ok() {}
    }
}

/// Captured mic frames → session (the manager gates muted audio).
fn spawn_audio_drain(
    handle: SessionManagerHandle,
    mut cap_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
) {
    tokio::spawn(async move {
        while let Some(frame) = cap_rx.recv().await {
            if handle.send_audio(frame).await.is_err() {
                break;
            }
        }
    });
}

/// Captured screen frames → session.
fn spawn_frame_drain(
    handle: SessionManagerHandle,
    mut frame_rx: tokio::sync::mpsc::Receiver<VideoFrame>,
) {
    tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if handle.send_frame(frame).await.is_err() {
                break;
            }
        }
    });
}
