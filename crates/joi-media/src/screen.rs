//! Native screen capture: grab the primary monitor at `fps` via `xcap`, downscale
//! to `max_width`, JPEG-encode, and push [`VideoFrame`]s to the session. Capture stays in Rust —
//! the frame bytes only ever cross into the provider adapter, never a frontend.

use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, RgbaImage};
use joi_core::media::{FrameEncoding, VideoFrame};
use xcap::Monitor;

use crate::MediaError;

/// Stops screen capture when dropped — the capture loop exits once this handle's sender is dropped.
pub struct ScreenHandle {
    _stop: Sender<()>,
}

/// Spawn screen capture on its own thread. JPEG [`VideoFrame`]s of the primary monitor are pushed to
/// `frames` at `fps` (downscaled so the longest edge ≤ `max_width`, encoded at `quality` 1–100).
/// Capture stops when the returned [`ScreenHandle`] is dropped.
#[must_use]
pub fn spawn_screen_capture(
    frames: tokio::sync::mpsc::Sender<VideoFrame>,
    fps: f32,
    max_width: u32,
    quality: u8,
) -> ScreenHandle {
    let (stop_tx, stop_rx) = channel::<()>();
    let interval = Duration::from_secs_f32(1.0 / fps.max(0.1));
    let spawned = std::thread::Builder::new()
        .name("joi-screen".to_string())
        .spawn(move || run(&frames, interval, max_width, quality, &stop_rx));
    if let Err(e) = spawned {
        tracing::error!("failed to spawn screen-capture thread: {e}");
    }
    ScreenHandle { _stop: stop_tx }
}

fn run(
    frames: &tokio::sync::mpsc::Sender<VideoFrame>,
    interval: Duration,
    max_width: u32,
    quality: u8,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) {
    tracing::info!(
        ?interval,
        max_width,
        quality,
        "native screen capture started"
    );
    while !matches!(stop_rx.try_recv(), Err(TryRecvError::Disconnected)) {
        let started = Instant::now();
        match capture_jpeg(max_width, quality) {
            Ok(frame) => {
                let _ = frames.try_send(frame);
            }
            Err(e) => tracing::warn!("screen frame failed: {e}"),
        }
        if let Some(rest) = interval.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    // On stop we simply stop sending frames. Gemini receives nothing further (verified), though it
    // may still hallucinate about the screen from earlier frames retained in its context — a model
    // limitation a blank frame can't fix. A textual "sharing ended" signal could, if wanted later.
    tracing::info!("native screen capture stopped");
}

/// Capture the primary monitor and return a downscaled JPEG frame.
fn capture_jpeg(max_width: u32, quality: u8) -> Result<VideoFrame, MediaError> {
    let monitors = Monitor::all().map_err(|e| MediaError::Backend(e.to_string()))?;
    let monitor = monitors
        .into_iter()
        .next()
        .ok_or_else(|| MediaError::Backend("no monitor found".to_string()))?;
    let rgba: RgbaImage = monitor
        .capture_image()
        .map_err(|e| MediaError::Backend(e.to_string()))?;

    let mut img = DynamicImage::ImageRgba8(rgba);
    if img.width() > max_width {
        let height = img.height() * max_width / img.width();
        img = img.resize(max_width, height, image::imageops::FilterType::Triangle);
    }
    let rgb = img.to_rgb8();
    let (width, height) = (rgb.width(), rgb.height());

    let mut data = Vec::new();
    JpegEncoder::new_with_quality(&mut data, quality)
        .encode_image(&rgb)
        .map_err(|e| MediaError::Backend(e.to_string()))?;

    Ok(VideoFrame {
        width,
        height,
        encoding: FrameEncoding::Jpeg,
        data,
    })
}
