//! Cloud (online) transcription backends.
//!
//! Handy is an offline-first app and stays that way: every engine in
//! [`crate::managers::model::EngineType`] except the ones here runs locally.
//! A cloud backend is opt-in — it only runs when the user explicitly selects a
//! model from the "Cloud" category in the model list — and it sends the raw
//! dictation audio to a third party, so it is never a silent default.
//!
//! Each provider lives in its own submodule and exposes a plain synchronous
//! `transcribe(&[f32]) -> Result<String>` so it can slot straight into the
//! `LoadedEngine` match in [`crate::managers::transcription`] alongside the
//! local engines, with no async colouring anywhere upstream.

pub mod elevenlabs;
pub mod gemini;
pub mod gemini_live;

use anyhow::{anyhow, Result};
use std::future::Future;
use std::io::Cursor;

/// Handy always hands engines 16 kHz mono f32 (the recorder resamples to this),
/// so the WAV header the cloud backends synthesize is fixed rather than derived.
const WAV_SAMPLE_RATE: u32 = 16_000;

/// Run a future to completion from a synchronous caller, whatever context that
/// caller is in.
///
/// `TranscriptionManager::transcribe` is sync but is called from *both* async
/// runtime worker threads (`actions.rs`, directly inside an `async fn`) and
/// plain blocking threads (`spawn_blocking` in `commands/history.rs`). Calling
/// `Runtime::block_on` — or touching `reqwest::blocking` — from inside a
/// runtime panics, so we always hop to a scratch OS thread that is guaranteed
/// to have no runtime context, and block the caller on its join.
///
/// Blocking the calling thread for the duration of the request is the same
/// deal the local engines already make (a whisper decode blocks it for
/// seconds); the cloud round-trip is typically shorter.
pub(crate) fn block_on<F>(fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build cloud transcription runtime");
                runtime.block_on(fut)
            })
            .join()
            // Propagate a panic in the request thread to the caller, which
            // `transcribe()` already wraps in `catch_unwind`.
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}

/// Encode 16 kHz mono f32 samples as an in-memory 16-bit PCM WAV.
///
/// Shared by every cloud backend: `audio/wav` is on both Gemini's and
/// ElevenLabs' accepted MIME lists and `hound` is already a dependency, so this
/// costs nothing. FLAC would halve the upload, which is worth revisiting if
/// payload time ever shows up in the timings.
pub(crate) fn encode_wav_16k(audio: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: WAV_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut buffer = Cursor::new(Vec::with_capacity(audio.len() * 2 + 44));
    {
        let mut writer = hound::WavWriter::new(&mut buffer, spec)
            .map_err(|e| anyhow!("Failed to start WAV encoding: {}", e))?;
        for sample in audio {
            // Clamp before scaling: the recorder's auto-gain can push a boosted
            // whisper right up to full scale, and an out-of-range cast wraps.
            let clamped = sample.clamp(-1.0, 1.0);
            writer
                .write_sample((clamped * i16::MAX as f32) as i16)
                .map_err(|e| anyhow!("Failed to encode WAV sample: {}", e))?;
        }
        writer
            .finalize()
            .map_err(|e| anyhow!("Failed to finalize WAV encoding: {}", e))?;
    }

    Ok(buffer.into_inner())
}

/// Published price in USD per minute of audio for a paid model, or `None` for
/// anything that runs locally and therefore costs nothing.
///
/// Google bills audio in and text out separately ($0.003 + $0.002 per minute
/// for the batch model, $0.005 + $0.004 for Live); we use the blended per-minute
/// figure they publish, because output length is not knowable up front. That
/// makes every cost here an *estimate* — no provider exposes a spend API, so a
/// real invoice will differ by rounding.
pub fn usd_per_minute(model_id: &str) -> Option<f64> {
    match model_id {
        gemini::MODEL_ID => Some(0.005),
        gemini_live::LIVE_MODEL_ID => Some(0.009),
        // ElevenLabs publishes Scribe v2 as a flat $0.22 per audio hour, with no
        // separate charge for the text it returns — so unlike Google's blended
        // figure this one is exact.
        elevenlabs::MODEL_ID => Some(0.22 / 60.0),
        _ => None,
    }
}

/// Cost of one dictation, or `None` for a free (local) model.
pub fn estimate_cost_usd(model_id: &str, duration_secs: f64) -> Option<f64> {
    usd_per_minute(model_id).map(|rate| rate * duration_secs / 60.0)
}

/// Which side of the local/cloud split a model sits on — the grouping key the
/// usage screen uses, and stable across catalog renames.
pub fn engine_kind(model_id: &str) -> &'static str {
    if usd_per_minute(model_id).is_some() {
        "cloud"
    } else {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_models_are_free_and_labelled_local() {
        assert_eq!(usd_per_minute("whisper-large-v3"), None);
        assert_eq!(estimate_cost_usd("whisper-large-v3", 600.0), None);
        assert_eq!(engine_kind("whisper-large-v3"), "local");
    }

    #[test]
    fn cloud_cost_scales_with_duration() {
        // 20 minutes of audio through the Live model at $0.009/min.
        let cost = estimate_cost_usd(gemini_live::LIVE_MODEL_ID, 20.0 * 60.0).unwrap();
        assert!((cost - 0.18).abs() < 1e-9, "got {cost}");
        assert_eq!(engine_kind(gemini_live::LIVE_MODEL_ID), "cloud");
    }

    #[test]
    fn batch_is_cheaper_than_live() {
        let batch = estimate_cost_usd(gemini::MODEL_ID, 3600.0).unwrap();
        let live = estimate_cost_usd(gemini_live::LIVE_MODEL_ID, 3600.0).unwrap();
        assert!(batch < live);
        assert!((batch - 0.30).abs() < 1e-9, "got {batch}");
    }
}
