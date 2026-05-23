//! [`MediaEngine`]: the single interface the Tauri shell uses for all native media. It owns the
//! capture/playback/screen lifecycle and the pumps that move frames between the cpal/xcap threads
//! and the session, so the composition root stays thin (PLAN-NATIVE-MEDIA §4). The
//! [`SessionManagerHandle`] API it binds to is unchanged — the engine simply takes the role the
//! webview used to play.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use joi_core::manager::SessionManagerHandle;
use joi_core::media::VideoFrame;
use tokio::sync::broadcast::error::RecvError;

use crate::capture::{spawn_capture, ApmConfig, CaptureHandle};
use crate::playback::{spawn_playback, PlaybackCmd};
use crate::screen::{spawn_screen_capture, ScreenHandle};

/// Sink for the active capture's echo-cancellation reference: the provider audio Joi is playing.
/// `Some` only while capturing; the playback pump forwards each chunk so the capture APM can
/// subtract Joi's own voice from the mic (AEC). A `std::sync::mpsc` channel bridges the async pump
/// to the synchronous capture thread.
type RenderSink = Arc<Mutex<Option<std::sync::mpsc::Sender<Vec<i16>>>>>;

/// Native-media settings, sourced from `Config` by the composition root.
#[derive(Debug, Clone, Copy)]
pub struct MediaConfig {
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
    /// Noise suppression on the mic.
    pub noise_suppression: bool,
    /// Automatic gain control on the mic.
    pub auto_gain: bool,
}

/// Owns all native media for one app instance. Construct once with [`MediaEngine::new`] (within a
/// Tokio runtime); drive it with the lifecycle methods from IPC commands. `Send + Sync`, so it lives
/// in Tauri-managed state.
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
    /// Echo-cancellation reference sink for the active capture (see [`RenderSink`]).
    render_sink: RenderSink,
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
        let render_sink: RenderSink = Arc::new(Mutex::new(None));

        spawn_playback_pump(&handle, Arc::clone(&render_sink));
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
        }
    }

    /// Start native mic capture for a session. **Idempotent**: if capture is already running this
    /// is a no-op (the stream is not torn down and respawned). The manager and the mute gate both
    /// drop muted audio.
    pub fn start_capture(&self) {
        // Hold `capture` across the whole operation so concurrent calls can't both spawn (and so the
        // lock order is capture → render_sink, matching `stop_capture` — no deadlock).
        if let Ok(mut cap) = self.capture.lock() {
            if cap.is_some() {
                return; // already capturing
            }
            // A fresh render channel; the playback pump forwards provider audio here as the
            // echo-cancellation reference. Only wire the sink when AEC is on — otherwise the sender
            // is dropped immediately and nothing is forwarded (capture won't use it anyway).
            let (render_tx, render_rx) = std::sync::mpsc::channel::<Vec<i16>>();
            if let Ok(mut sink) = self.render_sink.lock() {
                *sink = self.config.echo_cancellation.then_some(render_tx);
            }
            *cap = Some(spawn_capture(
                self.cap_tx.clone(),
                self.config.frame_samples,
                Arc::clone(&self.mic_muted),
                render_rx,
                ApmConfig {
                    echo_cancellation: self.config.echo_cancellation,
                    noise_suppression: self.config.noise_suppression,
                    auto_gain: self.config.auto_gain,
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
        if let Ok(mut sink) = self.render_sink.lock() {
            *sink = None; // stop forwarding the AEC reference
        }
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
/// flush the playback buffer immediately (FR-2). Each non-empty chunk is also forwarded to the
/// active capture's echo-cancellation reference (so the mic's copy of Joi's own voice is removed).
fn spawn_playback_pump(handle: &SessionManagerHandle, render_sink: RenderSink) {
    let playback_tx = spawn_playback();
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
                    // Feed the AEC reference (what we're about to play) before playing it.
                    if let Ok(sink) = render_sink.lock() {
                        if let Some(tx) = sink.as_ref() {
                            let _ = tx.send(pcm.clone());
                        }
                    }
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
