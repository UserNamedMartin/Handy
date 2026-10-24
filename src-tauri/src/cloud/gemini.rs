//! Google Gemini 3.5 Transcribe — cloud speech-to-text over the Interactions API.
//!
//! Unlike every local engine, there is no model to load: "loading" this engine
//! just builds an HTTP client, so switching to it is instant and costs no disk
//! or RAM. Each dictation is one POST of the whole utterance; the model has no
//! streaming mode here on purpose — Google's own numbers put the streaming
//! variant *behind* the batch one (4.0% vs 2.6% AA-WER, 5.50% vs 5.04% FLEURS),
//! and batch already returns in well under a second for dictation-length audio.
//!
//! The same model is reachable two ways — see [`GeminiRoute`]. Google AI Studio
//! takes an API key and the Interactions API; Vertex AI takes an OAuth token
//! and `generateContent`. Both stay supported side by side: they differ only in
//! who pays, so the choice is a setting, not a separate model.
//!
//! Docs: <https://ai.google.dev/gemini-api/docs/transcribe> (AI Studio),
//! <https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/gemini/3-5-transcribe> (Vertex)

use anyhow::{anyhow, Result};
use base64::Engine as _;
use log::{debug, warn};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::google_oauth::VertexCredentials;
use crate::settings::{GeminiConnection, GeminiTranscribeMode, GeminiTranscribeSettings};

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

/// Vertex host for the `global` location. Every Gemini model this fork uses is
/// served only there: the regional hosts (`us-central1-…`) answer 404 for them.
pub(super) const VERTEX_HOST: &str = "aiplatform.googleapis.com";

/// The location segment that goes with [`VERTEX_HOST`].
pub(super) const VERTEX_LOCATION: &str = "global";

/// Full Vertex resource name of a Google model in `project`.
pub(super) fn vertex_model_path(project: &str, model: &str) -> String {
    format!(
        "projects/{}/locations/{}/publishers/google/models/{}",
        project, VERTEX_LOCATION, model
    )
}

/// How a request reaches Gemini, and therefore who is billed for it.
#[derive(Clone)]
pub enum GeminiRoute {
    /// Gemini Developer API with an `AIza…` key. Billed to the key's AI Studio
    /// project — its free tier or its own prepaid balance.
    AiStudio { api_key: String },
    /// Vertex AI with an OAuth token. Billed to `project`'s Cloud billing
    /// account, which is what Google Cloud credits (the $300 trial included)
    /// pay for.
    Vertex {
        project: String,
        credentials: Arc<VertexCredentials>,
    },
}

impl GeminiRoute {
    /// Build the route the settings ask for, failing with a message that names
    /// the missing setting. `stored_key` is the AI Studio key from
    /// `cloud_api_keys`; it is ignored on the Vertex route.
    pub fn from_settings(config: &GeminiTranscribeSettings, stored_key: &str) -> Result<Self> {
        match config.connection {
            GeminiConnection::AiStudio => {
                let api_key = resolve_api_key(stored_key);
                if api_key.is_empty() {
                    return Err(anyhow!(
                        "No Gemini API key configured. Add one in Settings → Gemini, or switch the connection to Vertex AI."
                    ));
                }
                Ok(Self::AiStudio { api_key })
            }
            GeminiConnection::Vertex => {
                let project = config.vertex_project.trim().to_string();
                if project.is_empty() {
                    return Err(anyhow!(
                        "No Vertex project configured. Set the project ID in Settings → Gemini → Vertex AI."
                    ));
                }
                let credentials = VertexCredentials::shared(&config.vertex_credentials)?;
                Ok(Self::Vertex {
                    project,
                    credentials,
                })
            }
        }
    }

    /// For logs: which wallet a request was charged to.
    pub fn label(&self) -> &'static str {
        match self {
            Self::AiStudio { .. } => "AI Studio",
            Self::Vertex { .. } => "Vertex AI",
        }
    }
}

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

/// A cloud "engine". Holds no model — just the HTTP client and the route and
/// model id it was configured with.
pub struct GeminiTranscriber {
    client: reqwest::Client,
    route: GeminiRoute,
    model: String,
}

impl GeminiTranscriber {
    pub fn new(route: GeminiRoute, model: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            // Generous relative to the sub-second median, but bounded: a hung
            // request must not leave the user staring at the overlay.
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| anyhow!("Failed to build HTTP client for Gemini: {}", e))?;

        Ok(Self {
            client,
            route,
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
        match &self.route {
            GeminiRoute::AiStudio { api_key } => {
                self.transcribe_ai_studio(api_key, &wav, config, language, custom_words)
            }
            GeminiRoute::Vertex {
                project,
                credentials,
            } => self.transcribe_vertex(project, credentials, &wav, config, language, custom_words),
        }
    }

    fn transcribe_ai_studio(
        &self,
        api_key: &str,
        wav: &[u8],
        config: &GeminiTranscribeSettings,
        language: &str,
        custom_words: &[String],
    ) -> Result<String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(wav);
        let seconds = wav_seconds(wav);

        // A clip that does not fit inline is uploaded rather than refused. This
        // used to be a hard error at roughly seven minutes, which also made the
        // history screen's re-transcribe button useless on exactly the long
        // dictations most worth recovering.
        let audio_ref = if encoded.len() > MAX_INLINE_BYTES {
            debug!(
                "Gemini transcribe: {:.1}s of audio exceeds the inline budget; uploading",
                seconds
            );
            AudioRef::Uploaded(self.upload_wav(api_key, wav)?)
        } else {
            AudioRef::Inline(encoded)
        };

        let body = self.build_request(&audio_ref, config, language, custom_words);

        debug!(
            "Gemini transcribe: model={}, audio={:.1}s, route=AI Studio, via={}",
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
            .header("x-goog-api-key", api_key)
            .header("Api-Revision", API_REVISION)
            .json(&body);

        let response = send_for_text(request);

        // Delete before unwrapping the result: the file is ours either way, and
        // a failed transcription should not leave it behind.
        if let AudioRef::Uploaded(file) = &audio_ref {
            self.delete_file(api_key, &file.name);
        }

        extract_transcript(&response?)
    }

    /// Vertex AI: one `generateContent` call with the WAV inline.
    ///
    /// No upload step, unlike AI Studio: Vertex has no Files API (its
    /// equivalent is a Cloud Storage bucket), and it does not need one —
    /// measured, a 10.6-minute dictation went through as a single 27 MB inline
    /// request in 8.7 s. Google documents 15 minutes as the model's limit for a
    /// synchronous request, past anything Handy has recorded.
    fn transcribe_vertex(
        &self,
        project: &str,
        credentials: &VertexCredentials,
        wav: &[u8],
        config: &GeminiTranscribeSettings,
        language: &str,
        custom_words: &[String],
    ) -> Result<String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(wav);
        debug!(
            "Gemini transcribe: model={}, audio={:.1}s, route=Vertex AI ({}), inline {} KB",
            self.model,
            wav_seconds(wav),
            project,
            encoded.len() / 1024
        );

        let body = build_vertex_request(&encoded, config, language, custom_words);
        let url = format!(
            "https://{}/v1/{}:generateContent",
            VERTEX_HOST,
            vertex_model_path(project, &self.model)
        );
        let send = |token: String| {
            let request = self
                .client
                .post(&url)
                .bearer_auth(token)
                // Bill quota to the same project the URL names; user credentials
                // otherwise fall back to gcloud's own shared client project.
                .header("x-goog-user-project", project)
                .json(&body);
            send_for_text(request)
        };

        // One retry on 401 with a freshly minted token: the cached one can be
        // dead before its stated expiry (revoked, or held across a sleep), and
        // the dictation is worth one more round trip.
        let response = match send(credentials.access_token()?) {
            Err(e) if e.to_string().contains("401") => {
                warn!("Gemini (Vertex): token rejected, refreshing and retrying once: {}", e);
                credentials.invalidate();
                send(credentials.access_token()?)
            }
            other => other,
        };

        extract_vertex_transcript(&response?)
    }

    /// Hand the WAV to the Files API and return the handle to reference it by.
    ///
    /// Two round trips — open the session, then send the bytes — measured at
    /// 1.9 s for a 14 MB (7.5 minute) clip against 7 s for the transcription
    /// itself, so it is only worth doing for clips that cannot go inline.
    /// Uploads expire on the service after 48 h; we delete ours immediately
    /// after use anyway.
    fn upload_wav(&self, api_key: &str, wav: &[u8]) -> Result<UploadedFile> {
        let start = self
            .client
            .post(UPLOAD_URL)
            .header("x-goog-api-key", api_key)
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
    fn delete_file(&self, api_key: &str, name: &str) {
        let request = self
            .client
            .delete(format!("{}/{}", FILES_BASE, name))
            .header("x-goog-api-key", api_key);

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

/// Send a request and return its body, turning a non-2xx status into an error
/// that carries Google's own message.
fn send_for_text(request: reqwest::RequestBuilder) -> Result<String> {
    crate::cloud::block_on(async move {
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
    })
}

/// Audio length of a 16 kHz mono 16-bit WAV, for log lines.
fn wav_seconds(wav: &[u8]) -> f64 {
    wav.len().saturating_sub(44) as f64 / (SAMPLE_RATE as f64 * 2.0)
}

/// The Vertex `generateContent` body.
///
/// Same knobs as the Interactions request, spelled the way Vertex wants them:
/// `generationConfig.audioTranscriptionConfig`, camelCase, `mode` as an enum
/// string, and `diarization` / `wordTimestamp` as flat booleans — all verified
/// against the live service. Smart mode drops the two Verbatim-only options for
/// the same reason [`build_mode`] does.
fn build_vertex_request(
    encoded_wav: &str,
    config: &GeminiTranscribeSettings,
    language: &str,
    custom_words: &[String],
) -> Value {
    let mut transcription = serde_json::Map::new();
    transcription.insert(
        "languageCodes".to_string(),
        json!(resolve_language_codes(config, language)),
    );

    let vocabulary = resolve_custom_vocabulary(config, custom_words);
    if !vocabulary.is_empty() {
        transcription.insert("customVocabulary".to_string(), json!(vocabulary));
    }

    match config.mode {
        GeminiTranscribeMode::Smart => {
            transcription.insert("mode".to_string(), json!("SMART"));
        }
        GeminiTranscribeMode::Verbatim => {
            transcription.insert("mode".to_string(), json!("VERBATIM"));
            if config.diarization {
                transcription.insert("diarization".to_string(), json!(true));
            }
            if config.timestamps {
                transcription.insert("wordTimestamp".to_string(), json!(true));
            }
        }
    }

    json!({
        "contents": [{
            "role": "user",
            "parts": [{ "inlineData": { "mimeType": WAV_MIME, "data": encoded_wav } }],
        }],
        "generationConfig": { "audioTranscriptionConfig": Value::Object(transcription) },
    })
}

/// Pull the transcript out of a Vertex `generateContent` response.
///
/// With speaker labels on, the model returns one part per speaker turn, so the
/// text parts are joined the same way the Interactions steps are.
fn extract_vertex_transcript(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| anyhow!("Could not parse Gemini response: {} (body: {})", e, truncate(body)))?;

    let parts = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let joined = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    Ok(joined)
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
        let engine = GeminiTranscriber::new(
            GeminiRoute::AiStudio { api_key: "k".into() },
            MODEL_ID.into(),
        )
        .expect("engine");
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
    fn vertex_request_spells_the_config_the_way_vertex_wants_it() {
        let mut cfg = config();
        cfg.mode = GeminiTranscribeMode::Verbatim;
        cfg.language_codes = vec!["ru-RU".into(), "en-US".into()];
        cfg.custom_vocabulary = vec!["Handy".into()];
        cfg.diarization = true;
        cfg.timestamps = true;

        let body = build_vertex_request("QUJD", &cfg, "auto", &[]);
        let atc = &body["generationConfig"]["audioTranscriptionConfig"];
        assert_eq!(atc["mode"], json!("VERBATIM"));
        assert_eq!(atc["languageCodes"], json!(["ru-RU", "en-US"]));
        assert_eq!(atc["customVocabulary"], json!(["Handy"]));
        assert_eq!(atc["diarization"], json!(true));
        assert_eq!(atc["wordTimestamp"], json!(true));

        let audio = &body["contents"][0]["parts"][0]["inlineData"];
        assert_eq!(audio["mimeType"], json!(WAV_MIME));
        assert_eq!(audio["data"], json!("QUJD"));
    }

    #[test]
    fn vertex_smart_mode_drops_verbatim_only_options() {
        let mut cfg = config();
        cfg.mode = GeminiTranscribeMode::Smart;
        cfg.diarization = true;
        cfg.timestamps = true;

        let atc = build_vertex_request("", &cfg, "auto", &[])["generationConfig"]
            ["audioTranscriptionConfig"]
            .clone();
        assert_eq!(atc["mode"], json!("SMART"));
        assert!(atc.get("diarization").is_none());
        assert!(atc.get("wordTimestamp").is_none());
    }

    #[test]
    fn vertex_transcript_joins_speaker_parts() {
        let body = r#"{"candidates":[{"content":{"role":"model","parts":[
            {"text":"Привет.","audioTranscription":{"speakerLabel":"spk:0"}},
            {"text":" Hello. "}
        ]}}]}"#;
        assert_eq!(extract_vertex_transcript(body).unwrap(), "Привет. Hello.");
    }

    #[test]
    fn vertex_transcript_of_silence_is_empty_not_an_error() {
        let body = r#"{"candidates":[{"content":{"role":"model","parts":[]}}]}"#;
        assert_eq!(extract_vertex_transcript(body).unwrap(), "");
    }

    #[test]
    fn vertex_model_path_targets_the_global_location() {
        assert_eq!(
            vertex_model_path("p-1", "gemini-3.5-transcribe"),
            "projects/p-1/locations/global/publishers/google/models/gemini-3.5-transcribe"
        );
    }

    #[test]
    fn ai_studio_route_without_a_key_names_both_fixes() {
        std::env::remove_var(API_KEY_ENV);
        let error = GeminiRoute::from_settings(&config(), "").err().unwrap().to_string();
        assert!(error.contains("API key") && error.contains("Vertex"), "{error}");
    }

    #[test]
    fn vertex_route_without_a_project_says_so() {
        let mut cfg = config();
        cfg.connection = GeminiConnection::Vertex;
        let error = GeminiRoute::from_settings(&cfg, "AIza-ignored")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("project"), "{error}");
    }

    #[test]
    fn wav_encoding_produces_a_riff_header() {
        let wav = crate::cloud::encode_wav_16k(&[0.0, 0.5, -0.5]).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }
}
