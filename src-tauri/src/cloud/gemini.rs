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
use log::{debug, warn};
use serde::Deserialize;
use serde_json::{json, Value};
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
/// this budget is worth roughly seven minutes of speech. Anything longer goes
/// through [`GeminiTranscriber::upload_wav`] instead, which has no such limit.
const MAX_INLINE_BYTES: usize = 18 * 1024 * 1024;

/// Resumable-upload entry point of the Files API.
const UPLOAD_URL: &str = "https://generativelanguage.googleapis.com/upload/v1beta/files";

/// Base for addressing an uploaded file by its `files/<id>` name.
const FILES_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// The only container we ever send.
const WAV_MIME: &str = "audio/wav";

/// Google's documented ceiling on acoustic biasing terms. Their guidance is
/// that ~100 works best and more starts to dilute; we only enforce the hard cap
/// so a large `custom_words` list can never 400 the request.
const MAX_CUSTOM_VOCABULARY: usize = 1000;

/// A file the Files API is holding for us.
struct UploadedFile {
    uri: String,
    /// `files/<id>`, which is how a delete addresses it.
    name: String,
}

/// Where the audio for a request lives.
enum AudioRef {
    /// Base64 in the request body. One round trip, capped at 20 MB.
    Inline(String),
    /// Uploaded first and referenced by URI. No length limit.
    Uploaded(UploadedFile),
}

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

        let wav = super::encode_wav_16k(audio)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&wav);
        let seconds = audio.len() as f64 / SAMPLE_RATE as f64;

        // A clip that does not fit inline is uploaded rather than refused. This
        // used to be a hard error at roughly seven minutes, which also made the
        // history screen's re-transcribe button useless on exactly the long
        // dictations most worth recovering.
        let audio_ref = if encoded.len() > MAX_INLINE_BYTES {
            debug!(
                "Gemini transcribe: {:.1}s of audio exceeds the inline budget; uploading",
                seconds
            );
            AudioRef::Uploaded(self.upload_wav(&wav)?)
        } else {
            AudioRef::Inline(encoded)
        };

        let body = self.build_request(&audio_ref, config, language, custom_words);

        debug!(
            "Gemini transcribe: model={}, audio={:.1}s, via={}",
            self.model,
            seconds,
            match &audio_ref {
                AudioRef::Inline(data) => format!("inline {} KB", data.len() / 1024),
                AudioRef::Uploaded(file) => format!("upload {}", file.name),
            }
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
        });

        // Delete before unwrapping the result: the file is ours either way, and
        // a failed transcription should not leave it behind.
        if let AudioRef::Uploaded(file) = &audio_ref {
            self.delete_file(&file.name);
        }

        extract_transcript(&response?)
    }

    /// Hand the WAV to the Files API and return the handle to reference it by.
    ///
    /// Two round trips — open the session, then send the bytes — measured at
    /// 1.9 s for a 14 MB (7.5 minute) clip against 7 s for the transcription
    /// itself, so it is only worth doing for clips that cannot go inline.
    /// Uploads expire on the service after 48 h; we delete ours immediately
    /// after use anyway.
    fn upload_wav(&self, wav: &[u8]) -> Result<UploadedFile> {
        let start = self
            .client
            .post(UPLOAD_URL)
            .header("x-goog-api-key", &self.api_key)
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header("X-Goog-Upload-Header-Content-Length", wav.len().to_string())
            .header("X-Goog-Upload-Header-Content-Type", WAV_MIME)
            .json(&json!({ "file": { "display_name": "handy-dictation" } }));

        // The URL comes back in a header and already carries the credentials,
        // so the second leg needs no key of its own.
        let upload_url = crate::cloud::block_on(async move {
            let response = start
                .send()
                .await
                .map_err(|e| anyhow!("Gemini upload could not start: {}", e))?;
            let status = response.status();
            let url = response
                .headers()
                .get("x-goog-upload-url")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);

            match url {
                Some(url) if status.is_success() => Ok(url),
                _ => {
                    let text = response.text().await.unwrap_or_default();
                    Err(anyhow!(
                        "Gemini upload could not start ({}): {}",
                        status,
                        describe_api_error(&text)
                    ))
                }
            }
        })?;

        let finish = self
            .client
            .post(upload_url)
            .header("Content-Length", wav.len().to_string())
            .header("X-Goog-Upload-Offset", "0")
            .header("X-Goog-Upload-Command", "upload, finalize")
            .body(wav.to_vec());

        crate::cloud::block_on(async move {
            let response = finish
                .send()
                .await
                .map_err(|e| anyhow!("Gemini upload failed: {}", e))?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| anyhow!("Failed to read the Gemini upload response: {}", e))?;

            if !status.is_success() {
                return Err(anyhow!(
                    "Gemini upload returned {}: {}",
                    status,
                    describe_api_error(&text)
                ));
            }

            parse_uploaded_file(&text)
        })
    }

    /// Best-effort cleanup — the service expires uploads on its own, so a
    /// failure here costs a log line and nothing else.
    fn delete_file(&self, name: &str) {
        let request = self
            .client
            .delete(format!("{}/{}", FILES_BASE, name))
            .header("x-goog-api-key", &self.api_key);

        let outcome = crate::cloud::block_on(async move {
            request
                .send()
                .await
                .map_err(|e| anyhow!("{}", e))
                .map(|response| response.status())
        });

        match outcome {
            Ok(status) if status.is_success() => {}
            Ok(status) => warn!("Gemini: deleting the uploaded audio returned {}", status),
            Err(e) => warn!("Gemini: could not delete the uploaded audio: {}", e),
        }
    }

    fn build_request(
        &self,
        audio: &AudioRef,
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

        // `uri` is the field the Interactions API accepts for an uploaded file;
        // `file_uri` and `file_data`, which the rest of the Gemini surface
        // uses, are both rejected as unknown parameters here.
        let input = match audio {
            AudioRef::Inline(data) => json!({
                "type": "audio",
                "data": data,
                "mime_type": WAV_MIME,
            }),
            AudioRef::Uploaded(file) => json!({
                "type": "audio",
                "uri": file.uri,
                "mime_type": WAV_MIME,
            }),
        };

        json!({
            "model": self.model,
            "input": [input],
            "generation_config": {
                "transcription_config": Value::Object(transcription_config),
            },
        })
    }
}

/// Pull the handle out of an upload response.
///
/// The payload is wrapped in a `file` object, and `uri` is the absolute URL the
/// transcription request wants while `name` (`files/<id>`) is what a delete
/// addresses — so both are kept.
fn parse_uploaded_file(body: &str) -> Result<UploadedFile> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| anyhow!("Gemini upload returned unparseable JSON: {}", e))?;
    let file = value
        .get("file")
        .ok_or_else(|| anyhow!("Gemini upload response had no 'file'"))?;

    let field = |name: &str| {
        file.get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Gemini upload response had no '{}'", name))
    };

    Ok(UploadedFile {
        uri: field("uri")?,
        name: field("name")?,
    })
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
    #[test]
    fn uploaded_file_keeps_both_the_uri_and_the_name() {
        let parsed = parse_uploaded_file(
            r#"{"file":{"name":"files/abc123","uri":"https://x/v1beta/files/abc123","state":"ACTIVE"}}"#,
        )
        .expect("parse");
        assert_eq!(parsed.name, "files/abc123");
        assert_eq!(parsed.uri, "https://x/v1beta/files/abc123");
    }

    #[test]
    fn an_upload_response_without_a_uri_is_an_error() {
        // Failing here is better than sending a request whose audio field is
        // silently empty, which the service answers with an empty transcript.
        assert!(parse_uploaded_file(r#"{"file":{"name":"files/abc123"}}"#).is_err());
        assert!(parse_uploaded_file(r#"{"name":"files/abc123"}"#).is_err());
    }

    #[test]
    fn an_uploaded_reference_is_sent_as_uri_not_as_data() {
        let engine = GeminiTranscriber::new("k".into(), MODEL_ID.into()).expect("engine");
        let body = engine.build_request(
            &AudioRef::Uploaded(UploadedFile {
                uri: "https://x/v1beta/files/abc".into(),
                name: "files/abc".into(),
            }),
            &GeminiTranscribeSettings::default(),
            "auto",
            &[],
        );
        let input = &body["input"][0];
        assert_eq!(input["uri"], "https://x/v1beta/files/abc");
        assert_eq!(input["mime_type"], WAV_MIME);
        assert!(input.get("data").is_none());
        assert!(input.get("file_uri").is_none());
    }

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
        let wav = crate::cloud::encode_wav_16k(&[0.0, 0.5, -0.5]).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }
}
