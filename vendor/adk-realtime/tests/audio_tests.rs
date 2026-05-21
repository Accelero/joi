//! Tests for the audio module.

use adk_realtime::{AudioEncoding, AudioFormat};

#[test]
fn test_audio_encoding_display() {
    assert_eq!(AudioEncoding::Pcm16.to_string(), "pcm16");
    assert_eq!(AudioEncoding::G711Ulaw.to_string(), "g711_ulaw");
    assert_eq!(AudioEncoding::G711Alaw.to_string(), "g711_alaw");
}

#[test]
fn test_audio_format_constructors() {
    let pcm16_24k = AudioFormat::pcm16_24khz();
    assert_eq!(pcm16_24k.encoding, AudioEncoding::Pcm16);
    assert_eq!(pcm16_24k.sample_rate, 24000);
    assert_eq!(pcm16_24k.channels, 1);

    let pcm16_16k = AudioFormat::pcm16_16khz();
    assert_eq!(pcm16_16k.sample_rate, 16000);

    let g711_ulaw = AudioFormat::g711_ulaw();
    assert_eq!(g711_ulaw.encoding, AudioEncoding::G711Ulaw);
    assert_eq!(g711_ulaw.sample_rate, 8000);
}

#[test]
fn test_audio_format_default() {
    let format = AudioFormat::default();
    assert_eq!(format.encoding, AudioEncoding::Pcm16);
    assert_eq!(format.sample_rate, 24000);
    assert_eq!(format.channels, 1);
}
