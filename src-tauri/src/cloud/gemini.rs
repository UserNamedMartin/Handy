//! Google Gemini 3.5 Transcribe — cloud speech-to-text over the Interactions API.
//!
//! Unlike every local engine, there is no model to load: "loading" this engine
//! just builds an HTTP client, so switching to it is instant and costs no disk
//! or RAM. Each dictation is one POST of the whole utterance; the model has no
//! streaming mode here on purpose — Google's own numbers put the streaming
//! variant *behind* the batch one (4.0% vs 2.6% AA-WER, 5.50% vs 5.04% FLEURS),
//! and batch already returns in well under a second for dictation-length audio.
//!
//! Docs: <https://ai.google.dev/gemini-api/docs/transcribe>

use anyhow::{anyhow, Result};
use base64::Engine as _;
use log::debug;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Cursor;
use std::time::Duration;

use crate::settings::{GeminiTranscribeMode, GeminiTranscribeSettings};

/// Provider key under `AppSettings::cloud_api_keys`, and the prefix of the
/// catalog id for every model this backend serves.
pub const PROVIDER_ID: &str = "gemini";

/// Catalog id of the one Gemini model Handy currently offers.
pub const MODEL_ID: &str = "gemini-3.5-transcribe";

/// The API model name behind [`MODEL_ID`].
pub const API_MODEL: &str = "gemini-3.5-transcribe";

/// Escape hatch for headless runs (`--transcribe-file`) and offline eval
/// harnesses, which have no settings UI to paste a key into. The stored setting
/// always wins; this is only consulted when it is empty.
pub const API_KEY_ENV: &str = "HANDY_GEMINI_API_KEY";

/// The key to use: the stored setting, else [`API_KEY_ENV`].
pub fn resolve_api_key(stored: &str) -> String {
    if !stored.trim().is_empty() {
        return stored.trim().to_string();
    }
    std::env::var(API_KEY_ENV).unwrap_or_default().trim().to_string()
}

const INTERACTIONS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";

/// Pin the Interactions API surface. The model is in public preview, so the
/// unversioned surface can shift under us; this header keeps request/response
/// shapes stable until we deliberately move it.
const API_REVISION: &str = "2026-05-20";

/// Handy always hands engines 16 kHz mono f32 (the recorder resamples to this),
/// so the WAV header we synthesize is fixed rather than derived.
const SAMPLE_RATE: u32 = 16_000;

/// Google's cap on inline (base64) request bodies is 20 MB. We encode 16-bit
/// PCM at 16 kHz mono — 32 kB per second of audio, ~42.7 kB after base64 — so
/// this budget is worth roughly seven minutes of speech. Longer clips need the
/// Files API instead of an inline body; until that lands we fail with a clear
/// message rather than letting the request bounce off Google with a 400.
const MAX_INLINE_BYTES: usize = 18 * 1024 * 1024;

/// Google's documented ceiling on acoustic biasing terms. Their guidance is
/// that ~100 works best and more starts to dilute; we only enforce the hard cap
/// so a large `custom_words` list can never 400 the request.
const MAX_CUSTOM_VOCABULARY: usize = 1000;

/// A cloud "engine". Holds no model — just the HTTP client and the credentials
/// and model id it was configured with.
pub struct GeminiTranscriber {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiTranscriber {
    pub fn new(api_key: String, model: String) -> Result<Self> {
        let api_key = resolve_api_key(&api_key);
        if api_key.is_empty() {
            return Err(anyhow!(
                "No Gemini API key configured. Add one in Settings → Models → Gemini 3.5 Transcribe."
            ));
        }

        let client = reqwest::Client::builder()
            // Generous relative to the sub-second median, but bounded: a hung
            // request must not leave the user staring at the overlay.
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| anyhow!("Failed to build HTTP client for Gemini: {}", e))?;

        Ok(Self {
            client,
            api_key,
            model,
        })
    }

    /// Transcribe one utterance. `language` is Handy's already-validated
    /// language *intent* ("auto" or a code); it is only consulted when the
    /// settings block carries no explicit `language_codes` of its own.
    pub fn transcribe(
        &self,
        audio: &[f32],
        config: &GeminiTranscribeSettings,
        language: &str,
        custom_words: &[String],
    ) -> Result<String> {
        if audio.is_empty() {
            return Ok(String::new());
        }

        let wav = encode_wav(audio)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&wav);

        if encoded.len() > MAX_INLINE_BYTES {
            let seconds = audio.len() as f64 / SAMPLE_RATE as f64;
            return Err(anyhow!(
                "Recording is too long for cloud transcription ({:.0}s). \
                 Gemini accepts about 7 minutes per inline request; \
                 use a local model for clips this long.",
                seconds
            ));
        }

        let body = self.build_request(&encoded, config, language, custom_words);

        debug!(
            "Gemini transcribe: model={}, audio={:.1}s, payload={} KB",
            self.model,
            audio.len() as f64 / SAMPLE_RATE as f64,
            encoded.len() / 1024
        );

        let request = self
            .client
            .post(INTERACTIONS_URL)
            .header("x-goog-api-key", &self.api_key)
            .header("Api-Revision", API_REVISION)
            .json(&body);

        let response = crate::cloud::block_on(async move {
            let response = request
                .send()
                .await
                .map_err(|e| anyhow!("Gemini request failed: {}", e))?;

            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| anyhow!("Failed to read Gemini response: {}", e))?;

            if !status.is_success() {
                return Err(anyhow!(
                    "Gemini returned {}: {}",
                    status,
                    describe_api_error(&text)
                ));
            }

            Ok(text)
        })?;

        extract_transcript(&response)
    }

    fn build_request(
        &self,
        encoded_audio: &str,
        config: &GeminiTranscribeSettings,
        language: &str,
        custom_words: &[String],
    ) -> Value {
        let mut transcription_config = serde_json::Map::new();

        transcription_config.insert(
            "language_codes".to_string(),
            json!(resolve_language_codes(config, language)),
        );

        let vocabulary = resolve_custom_vocabulary(config, custom_words);
        if !vocabulary.is_empty() {
            transcription_config.insert("custom_vocabulary".to_string(), json!(vocabulary));
        }

        transcription_config.insert("mode".to_string(), build_mode(config));

        json!({
            "model": self.model,
            "input": [{
                "type": "audio",
                "data": encoded_audio,
                "mime_type": "audio/wav",
            }],
            "generation_config": {
                "transcription_config": Value::Object(transcription_config),
            },
        })
    }
}

/// Build the `mode` object.
///
/// Google makes `smart` mutually exclusive with diarization and word
/// timestamps. Rather than surface an API error for a combination the settings
/// UI should have prevented, `smart` wins and the other two are dropped — the
/// UI disables them in that state, so reaching here means a stale settings
/// store, not a user decision.
fn build_mode(config: &GeminiTranscribeSettings) -> Value {
    match config.mode {
        GeminiTranscribeMode::Smart => json!({ "type": "smart" }),
        GeminiTranscribeMode::Verbatim => {
            let mut mode = serde_json::Map::new();
            mode.insert("type".to_string(), json!("verbatim"));
            if config.diarization {
                mode.insert("diarization_mode".to_string(), json!("speaker"));
            }
            if config.timestamps {
                mode.insert("timestamp_granularities".to_string(), json!(["word"]));
            }
            Value::Object(mode)
        }
    }
}

/// Explicit `language_codes` from the model's own settings win. Otherwise fall
/// back to Handy's global language intent, where "auto" means an empty list —
/// which is how the API is told to detect across all 85+ locales.
pub(super) fn resolve_language_codes(
    config: &GeminiTranscribeSettings,
    language: &str,
) -> Vec<String> {
    if !config.language_codes.is_empty() {
        return config.language_codes.clone();
    }
    if language.is_empty() || language == "auto" {
        return Vec::new();
    }
    vec![language.to_string()]
}

/// Merge the model's own biasing list with Handy's global `custom_words` (when
/// enabled), de-duplicated case-insensitively and clamped to the API's cap.
pub(super) fn resolve_custom_vocabulary(
    config: &GeminiTranscribeSettings,
    custom_words: &[String],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();

    let extra: &[String] = if config.include_custom_words {
        custom_words
    } else {
        &[]
    };

    for term in config.custom_vocabulary.iter().chain(extra.iter()) {
        let trimmed = term.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_lowercase()) {
            terms.push(trimmed.to_string());
        }
        if terms.len() == MAX_CUSTOM_VOCABULARY {
            break;
        }
    }

    terms
}

/// Encode 16 kHz mono f32 samples as an in-memory 16-bit PCM WAV.
///
/// `audio/wav` is on Gemini's accepted MIME list and `hound` is already a
/// dependency, so this costs nothing. FLAC would halve the upload, which is
/// worth revisiting if payload time ever shows up in the timings.
fn encode_wav(audio: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
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

#[derive(Deserialize)]
struct InteractionResponse {
    /// Convenience field: the whole transcript as one string.
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    #[serde(default)]
    content: Vec<Content>,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    text: Option<String>,
}

/// Pull the transcript out of an Interactions response.
///
/// `output_text` is the documented convenience field, but it is not guaranteed
/// to be present on the raw REST surface the way it is in the SDKs, so fall
/// back to concatenating the text parts of the returned steps.
fn extract_transcript(body: &str) -> Result<String> {
    let parsed: InteractionResponse = serde_json::from_str(body)
        .map_err(|e| anyhow!("Could not parse Gemini response: {} (body: {})", e, truncate(body)))?;

    if let Some(text) = parsed.output_text {
        if !text.trim().is_empty() {
            return Ok(text.trim().to_string());
        }
    }

    let joined = parsed
        .steps
        .iter()
        .flat_map(|step| step.content.iter())
        .filter_map(|content| content.text.as_deref())
        .collect::<Vec<_>>()
        .join(" ");

    Ok(joined.trim().to_string())
}

/// Surface the human-readable part of an API error instead of the raw envelope.
fn describe_api_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| truncate(body))
}

fn truncate(body: &str) -> String {
    const LIMIT: usize = 400;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let mut end = LIMIT;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> GeminiTranscribeSettings {
        GeminiTranscribeSettings::default()
    }

    #[test]
    fn smart_mode_drops_incompatible_options() {
        let mut cfg = config();
        cfg.mode = GeminiTranscribeMode::Smart;
        cfg.diarization = true;
        cfg.timestamps = true;

        assert_eq!(build_mode(&cfg), json!({ "type": "smart" }));
    }

    #[test]
    fn verbatim_mode_carries_diarization_and_timestamps() {
        let mut cfg = config();
        cfg.mode = GeminiTranscribeMode::Verbatim;
        cfg.diarization = true;
        cfg.timestamps = true;

        assert_eq!(
            build_mode(&cfg),
            json!({
                "type": "verbatim",
                "diarization_mode": "speaker",
                "timestamp_granularities": ["word"],
            })
        );
    }

    #[test]
    fn explicit_language_codes_win_over_global_intent() {
        let mut cfg = config();
        cfg.language_codes = vec!["ru-RU".to_string(), "en-US".to_string()];

        assert_eq!(
            resolve_language_codes(&cfg, "de"),
            vec!["ru-RU".to_string(), "en-US".to_string()]
        );
    }

    #[test]
    fn auto_language_sends_an_empty_list() {
        assert!(resolve_language_codes(&config(), "auto").is_empty());
        assert!(resolve_language_codes(&config(), "").is_empty());
    }

    #[test]
    fn global_intent_is_used_when_no_explicit_codes() {
        assert_eq!(
            resolve_language_codes(&config(), "ru"),
            vec!["ru".to_string()]
        );
    }

    #[test]
    fn custom_vocabulary_merges_and_dedupes_case_insensitively() {
        let mut cfg = config();
        cfg.custom_vocabulary = vec!["Kubernetes".to_string(), "  ".to_string()];
        cfg.include_custom_words = true;

        let merged = resolve_custom_vocabulary(
            &cfg,
            &["kubernetes".to_string(), "staging".to_string()],
        );

        assert_eq!(merged, vec!["Kubernetes".to_string(), "staging".to_string()]);
    }

    #[test]
    fn custom_words_are_excluded_when_disabled() {
        let mut cfg = config();
        cfg.custom_vocabulary = vec!["Kubernetes".to_string()];
        cfg.include_custom_words = false;

        let merged = resolve_custom_vocabulary(&cfg, &["staging".to_string()]);

        assert_eq!(merged, vec!["Kubernetes".to_string()]);
    }

    #[test]
    fn transcript_prefers_output_text() {
        let body = r#"{"output_text":"  Привет, deploy на staging.  ","steps":[]}"#;
        assert_eq!(
            extract_transcript(body).unwrap(),
            "Привет, deploy на staging."
        );
    }

    #[test]
    fn transcript_falls_back_to_step_content() {
        let body = r#"{
            "steps":[{"content":[{"type":"text","text":"Hello"},{"type":"text","text":"world"}]}]
        }"#;
        assert_eq!(extract_transcript(body).unwrap(), "Hello world");
    }

    #[test]
    fn api_error_message_is_surfaced() {
        let body = r#"{"error":{"code":400,"message":"Invalid custom_vocabulary"}}"#;
        assert_eq!(describe_api_error(body), "Invalid custom_vocabulary");
    }

    #[test]
    fn wav_encoding_produces_a_riff_header() {
        let wav = encode_wav(&[0.0, 0.5, -0.5]).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }
}
