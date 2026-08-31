//! Does Handy's VAD let *whispered* speech through on the streaming path?
//!
//! Not part of the app. Run from `src-tauri/`:
//!   cargo run --release --example vad_whisper_gate
//!
//! The fork added `whisper_autogain` because the Silero gate dropped quiet
//! speech before it ever reached the model — but that boost is applied on the
//! **offline** path only (`offline_autogain = whisper_autogain && policy ==
//! Offline`). A cloud streaming model runs under `VadPolicy::Streaming`, so it
//! gets the same gate with none of the compensation. This measures exactly what
//! fraction of each corpus clip survives that gate, using the same detector,
//! threshold and hangover the recorder configures.

use anyhow::Result;
use handy_app_lib::audio_toolkit::vad::{
    frames_for_duration_ms, SmoothedVad, VoiceActivityDetector, VAD_ONSET_MS, VAD_PREFILL_MS,
    VAD_STREAMING_HANGOVER_MS,
};
use handy_app_lib::audio_toolkit::{vad::VadFrame, SileroVad};
use std::path::Path;

/// Mirrors `VAD_THRESHOLD` in managers/audio.rs.
const VAD_THRESHOLD: f32 = 0.3;
/// Silero here is fed 30 ms at 16 kHz (SILERO_FRAME_MS in vad/silero.rs); it
/// rejects any other length outright.
const FRAME: usize = 480;
const SILERO: &str = "resources/models/silero_vad_v4.onnx";
const CORPUS: &str = "examples/whisper_corpus";

fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    Ok(reader
        .samples::<i16>()
        .filter_map(Result::ok)
        .map(|s| s as f32 / i16::MAX as f32)
        .collect())
}

fn rms_dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return f32::NEG_INFINITY;
    }
    let mean_sq = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    20.0 * mean_sq.sqrt().log10()
}

fn main() -> Result<()> {
    let mut files: Vec<_> = std::fs::read_dir(CORPUS)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    files.sort();

    println!(
        "{:<34} {:>9} {:>8} {:>8} {:>7}",
        "clip", "RMS dBFS", "frames", "kept", "kept %"
    );
    println!("{}", "-".repeat(72));

    for path in files {
        let samples = read_wav(&path)?;
        // A fresh detector per clip: the recorder resets state per session.
        let silero = SileroVad::new(SILERO, VAD_THRESHOLD)?;
        // Upstream now expresses these as durations and converts per frame size,
        // so the harness must do the same to keep measuring the real gate.
        let mut vad: Box<dyn VoiceActivityDetector> = Box::new(SmoothedVad::new(
            Box::new(silero),
            frames_for_duration_ms(VAD_PREFILL_MS, FRAME),
            frames_for_duration_ms(VAD_STREAMING_HANGOVER_MS, FRAME),
            frames_for_duration_ms(VAD_ONSET_MS, FRAME),
        ));

        let (mut frames_in, mut frames_kept) = (0usize, 0usize);
        for chunk in samples.chunks(FRAME) {
            if chunk.len() < FRAME {
                break;
            }
            frames_in += 1;
            // Fail OPEN on a detector error, exactly like `handle_frame` in the
            // recorder — otherwise a measurement bug reads as a silent gate.
            if let VadFrame::Speech(kept) = vad
                .push_frame(chunk)
                .unwrap_or(VadFrame::Speech(chunk))
            {
                frames_kept += kept.len() / FRAME;
            }
        }

        let ratio = if frames_in > 0 {
            100.0 * frames_kept as f32 / frames_in as f32
        } else {
            0.0
        };
        println!(
            "{:<34} {:>9.1} {:>8} {:>8} {:>6.0}%",
            path.file_name().unwrap().to_string_lossy(),
            rms_dbfs(&samples),
            frames_in,
            frames_kept,
            ratio
        );
    }
    Ok(())
}
