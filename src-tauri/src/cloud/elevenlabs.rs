//! ElevenLabs Scribe v2 — cloud speech-to-text over the REST transcription API.
//!
//! Same shape as the Gemini backend next door: no model to load, one POST per
//! utterance, and a plain synchronous `transcribe` so it slots into the
//! `LoadedEngine` match without colouring anything async.
//!
//! Why this model and not their streaming one. Measured on 10 of Martin's real
//! dictations (Russian with English terms, recorded through the soundproof
//! mask) against a Gemini 3.1 Pro reference: Scribe v2 scored 10.8% WER,
//! Gemini's batch model 14.4% and its Live model 15.9% — while
//! `scribe_v2_realtime` came in at **30.3%**, mangling exactly the code-switched
//! terms this setup exists to get right ("в треде" → "в Телеге"). Feeding that
//! streaming model at true 1x pacing and lifting the quiet mask audio by 18 dB
//! both failed to close the gap, so it is deliberately not offered here — even
//! though Artificial Analysis ranks it 3rd in the world on their English
//! benchmark. English leaderboards do not describe this use case.
//!
//! Docs: <https://elevenlabs.io/docs/api-reference/speech-to-text>

use anyhow::{anyhow, Result};
use log::debug;
use serde::Deserialize;
use std::time::Duration;

/// Provider key under `AppSettings::cloud_api_keys`, and the prefix of the
/// catalog id for every model this backend serves.
pub const PROVIDER_ID: &str = "elevenlabs";

/// Catalog id of the one ElevenLabs model Handy offers.
pub const MODEL_ID: &str = "elevenlabs-scribe-v2";

/// The API model name behind [`MODEL_ID`].
pub const API_MODEL: &str = "scribe_v2";

/// Escape hatch for headless runs and offline eval harnesses, which have no
/// settings UI to paste a key into. The stored setting always wins.
pub const API_KEY_ENV: &str = "ELEVENLABS_API_KEY";

const TRANSCRIBE_URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";

/// Handy always hands engines 16 kHz mono f32, so the WAV header we synthesize
/// is fixed rather than derived.
const SAMPLE_RATE: u32 = 16_000;

/// ElevenLabs accepts files up to 3 GB, but a dictation that large is a bug in
/// the caller, not a request worth making. Cap at roughly an hour of 16 kHz
/// mono PCM and fail with something readable instead.
const MAX_UPLOAD_BYTES: usize = 120 * 1024 * 1024;

/// The key to use: the stored setting, else [`API_KEY_ENV`].
pub fn resolve_api_key(stored: &str) -> String {
    if !stored.trim().is_empty() {
        return stored.trim().to_string();
    }
    std::env::var(API_KEY_ENV)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A cloud "engine". Holds no model — just the HTTP client, the credentials and
/// the model id it was configured with.
pub struct ElevenLabsTranscriber {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl ElevenLabsTranscriber {
    pub fn new(api_key: String, model: String) -> Result<Self> {
        let api_key = resolve_api_key(&api_key);
        if api_key.is_empty() {
            return Err(anyhow!(
                "No ElevenLabs API key configured. Add one in Settings → Models → ElevenLabs Scribe v2."
            ));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| anyhow!("Failed to build HTTP client for ElevenLabs: {}", e))?;

        Ok(Self {
            client,
            api_key,
            model,
        })
    }

    /// Transcribe one utterance. `language` is Handy's already-validated
    /// language *intent*: "auto" leaves detection to the service, anything else
    /// is passed through as an ISO 639-1 code, which their docs say to prefer
    /// when the language is known.
    pub fn transcribe(&self, audio: &[f32], language: &str) -> Result<String> {
        if audio.is_empty() {
            return Ok(String::new());
        }

        let wav = super::encode_wav_16k(audio)?;
        if wav.len() > MAX_UPLOAD_BYTES {
            let seconds = audio.len() as f64 / SAMPLE_RATE as f64;
            return Err(anyhow!(
                "Recording is too long for cloud transcription ({:.0}s). \
                 Use a local model for clips this long.",
                seconds
            ));
        }

        debug!(
            "ElevenLabs transcribe: model={}, audio={:.1}s, payload={} KB",
            self.model,
            audio.len() as f64 / SAMPLE_RATE as f64,
            wav.len() / 1024
        );

        let mut form = reqwest::multipart::Form::new()
            .text("model_id", self.model.clone())
            // Scribe annotates non-speech ("(laughter)") unless told not to.
            // Dictation goes straight into the user's cursor, so an annotation
            // is a typo they have to delete.
            .text("tag_audio_events", "false")
            .text("diarize", "false")
            .part(
                "file",
                reqwest::multipart::Part::bytes(wav)
                    .file_name("dictation.wav")
                    .mime_str("audio/wav")
                    .map_err(|e| anyhow!("Failed to build ElevenLabs upload: {}", e))?,
            );

        if let Some(code) = language_code(language) {
            form = form.text("language_code", code);
        }

        let request = self
            .client
            .post(TRANSCRIBE_URL)
            .header("xi-api-key", &self.api_key)
            .multipart(form);

        let response = super::block_on(async move {
            let response = request
                .send()
                .await
                .map_err(|e| anyhow!("ElevenLabs request failed: {}", e))?;

            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| anyhow!("Failed to read ElevenLabs response: {}", e))?;

            if !status.is_success() {
                return Err(anyhow!(
                    "ElevenLabs returned {}: {}",
                    status,
                    describe_api_error(&text)
                ));
            }

            Ok(text)
        })?;

        extract_transcript(&response)
    }
}

/// Handy's language intent → the `language_code` field, or `None` for auto.
///
/// Handy stores plain ISO 639-1 ("ru"), which is one of the two forms the API
/// accepts, so the only real work here is recognising "auto".
fn language_code(language: &str) -> Option<String> {
    let trimmed = language.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return None;
    }
    Some(trimmed.to_string())
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    #[serde(default)]
    text: Option<String>,
}

fn extract_transcript(body: &str) -> Result<String> {
    let parsed: TranscriptionResponse = serde_json::from_str(body).map_err(|e| {
        anyhow!(
            "Could not parse ElevenLabs response: {} (body: {})",
            e,
            truncate(body)
        )
    })?;

    Ok(parsed.text.unwrap_or_default().trim().to_string())
}

/// Surface the human-readable part of an API error instead of the raw envelope.
///
/// Their errors nest the useful sentence under `detail.message`, with
/// `detail.status` carrying the machine-readable reason (`quota_exceeded`,
/// `missing_permissions`, …).
fn describe_api_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            let detail = value.get("detail")?;
            if let Some(message) = detail.get("message").and_then(|m| m.as_str()) {
                return Some(message.to_string());
            }
            detail.as_str().map(str::to_string)
        })
        .unwrap_or_else(|| truncate(body))
}

fn truncate(body: &str) -> String {
    const MAX: usize = 400;
    if body.len() <= MAX {
        return body.to_string();
    }
    format!("{}…", &body[..MAX])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_language_is_left_to_the_service() {
        assert_eq!(language_code("auto"), None);
        assert_eq!(language_code(""), None);
        assert_eq!(language_code("  "), None);
        assert_eq!(language_code("ru"), Some("ru".to_string()));
    }

    #[test]
    fn transcript_is_pulled_and_trimmed() {
        let body = r#"{"language_code":"rus","text":"  Привет, мир.  "}"#;
        assert_eq!(extract_transcript(body).unwrap(), "Привет, мир.");
    }

    #[test]
    fn missing_text_is_empty_not_an_error() {
        assert_eq!(
            extract_transcript(r#"{"language_code":"rus"}"#).unwrap(),
            ""
        );
    }

    #[test]
    fn api_errors_surface_their_message() {
        let body =
            r#"{"detail":{"status":"quota_exceeded","message":"You have exceeded your quota."}}"#;
        assert_eq!(describe_api_error(body), "You have exceeded your quota.");
    }

    #[test]
    fn unparseable_errors_fall_back_to_the_body() {
        assert_eq!(describe_api_error("upstream exploded"), "upstream exploded");
    }
}
