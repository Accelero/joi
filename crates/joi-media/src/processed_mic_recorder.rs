//! Diagnostic WAV recording for the post-APM microphone stream.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use joi_core::media::AudioFormat;

const ENV_VAR: &str = "JOI_PROCESSED_MIC_WAV";
const CHANNELS: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;
const BYTES_PER_SAMPLE: u16 = BITS_PER_SAMPLE / 8;

/// A best-effort diagnostic recorder for the exact post-processing mic format Joi sends upstream.
pub(crate) struct ProcessedMicRecorder {
    path: PathBuf,
    writer: BufWriter<File>,
    data_bytes: u32,
    failed: bool,
}

impl ProcessedMicRecorder {
    /// Create a recorder when `JOI_PROCESSED_MIC_WAV` is set to a non-empty path.
    pub(crate) fn from_env() -> Option<Self> {
        let path = std::env::var_os(ENV_VAR).filter(|v| !v.is_empty())?;
        let path = PathBuf::from(path);
        match Self::create(&path) {
            Ok(recorder) => {
                tracing::info!(path = %path.display(), "recording processed mic audio");
                Some(recorder)
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "processed mic recorder unavailable"
                );
                None
            }
        }
    }

    fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        write_header(&mut writer, 0)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer,
            data_bytes: 0,
            failed: false,
        })
    }

    /// Append post-APM PCM16 samples. On the first write failure, logging is emitted once and future
    /// writes become no-ops so capture can continue.
    pub(crate) fn write_samples(&mut self, samples: &[i16]) {
        if self.failed || self.data_bytes == u32::MAX {
            return;
        }

        let free = u32::MAX - self.data_bytes;
        let sample_capacity = (free / u32::from(BYTES_PER_SAMPLE)) as usize;
        let samples = &samples[..samples.len().min(sample_capacity)];
        if samples.is_empty() {
            return;
        }

        let mut bytes = Vec::with_capacity(samples.len() * usize::from(BYTES_PER_SAMPLE));
        for &sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }

        if let Err(e) = self.writer.write_all(&bytes) {
            self.failed = true;
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "processed mic recorder write failed"
            );
            return;
        }
        self.data_bytes = self.data_bytes.saturating_add(bytes.len() as u32);
    }

    fn finish(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        self.writer.seek(SeekFrom::Start(0))?;
        write_header(&mut self.writer, self.data_bytes)?;
        self.writer.flush()
    }
}

impl Drop for ProcessedMicRecorder {
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "processed mic recorder finalization failed"
            );
        }
    }
}

fn write_header<W: Write>(writer: &mut W, data_bytes: u32) -> std::io::Result<()> {
    let sample_rate = AudioFormat::INPUT.sample_rate;
    let byte_rate = sample_rate * u32::from(CHANNELS) * u32::from(BYTES_PER_SAMPLE);
    let block_align = CHANNELS * BYTES_PER_SAMPLE;
    let riff_size = 36u32.saturating_add(data_bytes);

    writer.write_all(b"RIFF")?;
    writer.write_all(&riff_size.to_le_bytes())?;
    writer.write_all(b"WAVE")?;
    writer.write_all(b"fmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&CHANNELS.to_le_bytes())?;
    writer.write_all(&sample_rate.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&block_align.to_le_bytes())?;
    writer.write_all(&BITS_PER_SAMPLE.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_wav_sizes() {
        let path = std::env::temp_dir().join(format!(
            "joi-processed-mic-test-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut recorder = ProcessedMicRecorder::create(&path).unwrap();
            recorder.write_samples(&[0, 1, -1, i16::MAX]);
        }

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 44);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        assert_eq!(bytes.len() as u64, 44 + 8);
        std::fs::remove_file(path).unwrap();
    }
}
