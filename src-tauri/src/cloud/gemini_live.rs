//! Gemini 3.5 Transcribe Live — streaming speech-to-text over the Live API.
//!
//! The batch sibling in [`super::gemini`] has a hard ~3 s floor per request
//! (measured: a 3 s clip and a 21 s clip both take ~3 s, so it is fixed
//! server-side cost, not transfer). Streaming removes that entirely by
//! transcribing *while* the user talks: at key-release only the tail is
//! outstanding, measured at 220–500 ms.
//!
//! Wire protocol (verified against the live service):
//! - Connect to `wss://…BidiGenerateContent?key=…`.
//! - Send one `setup` message; the server replies `setupComplete`.
//! - Send `realtimeInput.activityStart`, then stream `realtimeInput.audio`
//!   chunks of raw 16-bit LE PCM at 16 kHz, ~100 ms each.
//! - Send `realtimeInput.activityEnd` to close the turn.
//! - `serverContent.interimInputTranscription` carries volatile partials;
//!   `serverContent.inputTranscription` carries finalized, sentence-shaped
//!   chunks with no leading/trailing spaces (so they join with a single space).
//! - `serverContent.turnComplete` ends the turn.
//!
//! Google caps a session at 10 minutes. Handy's longest observed dictation is
//! ~8 minutes, so an overrun is not a normal path: the socket dies, `finish`
//! returns what it has, and an empty result makes the caller batch-transcribe
//! the same audio rather than lose it.
//!
//! Vertex AI speaks the identical protocol (verified: same setup, same
//! `activityStart`/`activityEnd` bracketing, same `inputTranscription` frames);
//! only the endpoint, the authentication and the model's resource name differ.
//! [`GeminiRoute`] carries that difference, so everything below `connect` is
//! shared by both.
//!
//! Docs: <https://ai.google.dev/gemini-api/docs/live-api/live-transcribe>

use anyhow::{anyhow, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use log::{debug, warn};
use serde_json::{json, Value};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use super::gemini::{vertex_model_path, GeminiRoute, VERTEX_HOST};
use crate::settings::{GeminiTranscribeMode, GeminiTranscribeSettings};

const WS_URL: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

/// Vertex's Live endpoint path, appended to [`VERTEX_HOST`].
const VERTEX_WS_PATH: &str = "/ws/google.cloud.aiplatform.v1.LlmBidiService/BidiGenerateContent";

/// API model name for the streaming variant. Distinct from the batch model, and
/// the Live API wants it `models/`-prefixed.
pub const LIVE_API_MODEL: &str = "models/gemini-3.5-transcribe-live";

/// The same model without the prefix, for building a Vertex resource name.
const LIVE_MODEL_NAME: &str = "gemini-3.5-transcribe-live";

/// Catalog id of the streaming model in Handy's model list.
pub const LIVE_MODEL_ID: &str = "gemini-3.5-transcribe-live";

const SAMPLE_RATE: u32 = 16_000;

/// How long to wait for `turnComplete` after `audioStreamEnd`.
///
/// The service normally answers in 220-500 ms. Waiting forever is what
/// deadlocked the first build: with the audio branch disabled after end-of-
/// stream, a server that never closed the turn left the reader parked on
/// `reader.next()` for good.
const FINAL_WAIT: Duration = Duration::from_secs(10);

/// Backstop between tail chunks when no end-of-turn signal lands.
const IDLE_AFTER_FINAL: Duration = Duration::from_millis(600);

/// Ceiling on the wait for the first transcript chunk after `activityEnd`.
///
/// With manual activity detection the server emits nothing until the turn
/// closes, so this is the whole transcript's latency, not a tail: measured 316 ms
/// for 3 s of audio, 1.2 s for 90 s, 1.4 s for 142 s. The budget is generous
/// against that curve because overshooting merely delays, while undershooting
/// discards a finished transcript. Matches Jot's `TimeoutPolicy.liveFinal`.
const TAIL_FIRST_WAIT: Duration = Duration::from_secs(6);

/// Floor under [`tail_first_wait`], covering the round trip itself.
const TAIL_WAIT_FLOOR: Duration = Duration::from_millis(800);

/// How much of the ceiling a second of audio earns. 40 ms/s sits ~3x above the
/// measured curve at every length, so the bound stays conservative.
const TAIL_WAIT_PER_AUDIO_SECOND: f64 = 0.04;

/// How long to wait for the first chunk, given how much audio was actually sent.
///
/// A flat ceiling is calibrated for the longest dictation and is absurd for the
/// shortest: a key brushed for 250 ms produced no speech, so the service will
/// never send a chunk, and waiting the full 6 s for one blocked the whole
/// pipeline behind an accident. The transcript cannot outlast its audio, so the
/// budget scales with it and only ever shortens the wait — long dictations still
/// get the full ceiling.
pub fn tail_first_wait(audio: Duration) -> Duration {
    let scaled = TAIL_WAIT_FLOOR + audio.mul_f64(TAIL_WAIT_PER_AUDIO_SECOND);
    scaled.min(TAIL_FIRST_WAIT)
}

/// Absolute ceiling on a session, generous against Google's own 10-minute cap.
/// A socket that goes quiet mid-dictation must not strand the thread.
const SESSION_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// How long one Live connection keeps transcribing before the service stops
/// answering. Measured, not documented: five real clips (234-637 s) each
/// delivered their last transcript at **221.6-221.9 s**, with the last interim
/// a second or two earlier, and the socket then went silent while the client
/// kept streaming audio into it. Neither sliding-window context compression nor
/// closing and re-opening the activity on the same socket moved it, while a
/// fresh connection always gets its own full budget — so the ceiling is per
/// connection. Google documents 10 minutes for this model, so this number is an
/// observation that may change under us; the margin below the measurement is
/// deliberate, and being wrong by a few seconds only costs one extra batch
/// request on a dictation that was already long.
pub const TRANSCRIPTION_CAP: Duration = Duration::from_secs(210);

/// What the worker thread reports back to the (synchronous) caller.
#[derive(Debug)]
pub enum LiveEvent {
    /// A volatile partial hypothesis — safe to show, never to keep.
    Interim(String),
    /// A finalized chunk. Sentence-shaped and unpadded; join with one space.
    Final(String),
    /// The session ended. `Ok(())` means the server closed the turn cleanly.
    Closed(Result<()>),
}

enum Outbound {
    Audio(Vec<u8>),
    End,
}

/// A live transcription session running on its own thread.
///
/// Synchronous on the outside so it drops straight into Handy's streaming
/// worker, which owns a `std::sync::mpsc` command loop and cannot be async.
pub struct GeminiLiveSession {
    audio_tx: Option<tokio_mpsc::UnboundedSender<Outbound>>,
    events: std_mpsc::Receiver<LiveEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GeminiLiveSession {
    /// Open a session and wait for `setupComplete`.
    ///
    /// Connection + setup measured at ~700 ms, which is why this is called when
    /// *recording starts* rather than at key-release: the handshake overlaps the
    /// user's first words and costs nothing perceptible.
    pub fn connect(
        route: &GeminiRoute,
        config: &GeminiTranscribeSettings,
        language: &str,
        custom_words: &[String],
    ) -> Result<Self> {
        let (request, model) = live_endpoint(route)?;
        let setup = build_setup(&model, config, language, custom_words);

        let (audio_tx, audio_rx) = tokio_mpsc::unbounded_channel::<Outbound>();
        let (event_tx, event_rx) = std_mpsc::channel::<LiveEvent>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();

        let thread = std::thread::Builder::new()
            .name("gemini-live".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow!("failed to build runtime: {}", e)));
                        return;
                    }
                };
                runtime.block_on(session(request, setup, audio_rx, event_tx, ready_tx));
            })
            .map_err(|e| anyhow!("failed to spawn Gemini Live thread: {}", e))?;

        // Surface a bad key / rejected config here rather than after the user
        // has spoken a whole paragraph into a dead socket.
        match ready_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(())) => Ok(Self {
                audio_tx: Some(audio_tx),
                events: event_rx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("Gemini Live setup timed out")),
        }
    }

    /// Feed one frame of 16 kHz mono f32 samples.
    pub fn feed(&self, pcm: &[f32]) {
        let Some(tx) = &self.audio_tx else { return };
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in pcm {
            // Clamp before scaling: the recorder's auto-gain can push a boosted
            // whisper to full scale, and an out-of-range cast wraps.
            let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            bytes.extend_from_slice(&scaled.to_le_bytes());
        }
        let _ = tx.send(Outbound::Audio(bytes));
    }

    /// Drain whatever has arrived without blocking.
    pub fn poll(&self) -> Vec<LiveEvent> {
        self.events.try_iter().collect()
    }

    /// Signal end-of-audio and collect the remaining transcript.
    ///
    /// Returns every finalized chunk seen after this point; the caller joins
    /// them onto whatever it already collected from [`poll`].
    pub fn finish(&mut self, timeout: Duration, first_wait: Duration) -> Result<Vec<String>> {
        if let Some(tx) = &self.audio_tx {
            let _ = tx.send(Outbound::End);
        }

        let deadline = std::time::Instant::now() + timeout;
        let mut tail = Vec::new();
        loop {
            let hard_remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if hard_remaining.is_zero() {
                warn!("Gemini Live: gave up waiting for the tail of the transcript");
                break;
            }
            // Both waits are short and bounded. `hard_remaining` is only ever
            // the ceiling — reaching it means something is wrong, not that we
            // are being patient.
            let wait = if tail.is_empty() {
                hard_remaining.min(first_wait)
            } else {
                hard_remaining.min(IDLE_AFTER_FINAL)
            };

            match self.events.recv_timeout(wait) {
                Ok(LiveEvent::Final(text)) => tail.push(text),
                Ok(LiveEvent::Interim(_)) => {}
                Ok(LiveEvent::Closed(result)) => {
                    result?;
                    break;
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    // Nothing after end-of-stream is the normal "you paused
                    // before releasing" case, not an error: whatever the caller
                    // already collected is the complete transcript.
                    debug!("Gemini Live: nothing further after end of audio");
                    break;
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(tail)
    }
}

impl Drop for GeminiLiveSession {
    fn drop(&mut self) {
        // Dropping the sender makes the session loop observe a closed channel
        // and shut down. We deliberately do NOT join: this runs on the
        // transcription worker, which owns the engine lease, and any blocking
        // teardown here strands that lease and takes dictation down with it —
        // exactly the deadlock this replaced. Every wait inside the session is
        // bounded (FINAL_WAIT, SESSION_DEADLINE), so the thread always exits on
        // its own; detaching costs at most one idle thread for a few seconds.
        self.audio_tx = None;
        self.thread.take();
    }
}

/// Join finalized chunks into one transcript.
///
/// The service emits sentence-shaped chunks with no padding
/// (`"…панели чата."` then `"И там видно…"`), so a plain concatenation would
/// produce `"чата.И там"`. One space between chunks is the correct join.
pub fn join_finals(chunks: &[String]) -> String {
    chunks
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The handshake request and the `setup.model` value for a route.
///
/// AI Studio authenticates with the key in the query string and names the
/// model `models/…`; Vertex wants a bearer token in a header and the model's
/// full resource name under the project being billed.
fn live_endpoint(route: &GeminiRoute) -> Result<(Request, String)> {
    match route {
        GeminiRoute::AiStudio { api_key } => {
            let request = format!("{}?key={}", WS_URL, api_key)
                .into_client_request()
                .map_err(|e| anyhow!("Gemini Live: bad endpoint: {}", e))?;
            Ok((request, LIVE_API_MODEL.to_string()))
        }
        GeminiRoute::Vertex {
            project,
            credentials,
        } => {
            // Minted before the socket opens: a token refresh is a network
            // round trip, and this runs while the user has already started
            // talking — the same window the handshake itself uses.
            let token = credentials.access_token()?;
            let mut request = format!("wss://{}{}", VERTEX_HOST, VERTEX_WS_PATH)
                .into_client_request()
                .map_err(|e| anyhow!("Gemini Live: bad endpoint: {}", e))?;
            let headers = request.headers_mut();
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {}", token))
                    .map_err(|e| anyhow!("Gemini Live: unusable access token: {}", e))?,
            );
            headers.insert(
                "x-goog-user-project",
                HeaderValue::from_str(project)
                    .map_err(|e| anyhow!("Gemini Live: unusable project id: {}", e))?,
            );
            Ok((request, vertex_model_path(project, LIVE_MODEL_NAME)))
        }
    }
}

fn build_setup(
    model: &str,
    config: &GeminiTranscribeSettings,
    language: &str,
    custom_words: &[String],
) -> Value {
    let mut transcription = serde_json::Map::new();
    transcription.insert(
        "languageCodes".to_string(),
        json!(super::gemini::resolve_language_codes(config, language)),
    );

    let vocabulary = super::gemini::resolve_custom_vocabulary(config, custom_words);
    if !vocabulary.is_empty() {
        transcription.insert("customVocabulary".to_string(), json!(vocabulary));
    }

    // The Live API spells the mode as an enum string rather than the batch
    // API's nested object. VERBATIM is the service default; SMART additionally
    // strips fillers and resolves self-corrections.
    transcription.insert(
        "mode".to_string(),
        json!(match config.mode {
            GeminiTranscribeMode::Smart => "SMART",
            GeminiTranscribeMode::Verbatim => "VERBATIM",
        }),
    );

    json!({
        "setup": {
            "model": model,
            "generationConfig": { "responseModalities": ["TEXT"] },
            "inputAudioTranscription": Value::Object(transcription),
            // The push-to-talk key owns the turn boundaries, so server-side
            // voice detection is turned off: left on, it ends the turn on a
            // thinking pause and truncates the dictation. This mirrors Google's
            // own Jot client, which disables it for exactly that reason.
            "realtimeInputConfig": {
                "automaticActivityDetection": { "disabled": true }
            },
        }
    })
}

async fn session(
    request: Request,
    setup: Value,
    mut audio_rx: tokio_mpsc::UnboundedReceiver<Outbound>,
    event_tx: std_mpsc::Sender<LiveEvent>,
    ready_tx: std_mpsc::Sender<Result<()>>,
) {
    let (stream, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(pair) => pair,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow!("Gemini Live connection failed: {}", e)));
            return;
        }
    };
    let (mut writer, mut reader) = stream.split();

    if let Err(e) = writer.send(Message::text(setup.to_string())).await {
        let _ = ready_tx.send(Err(anyhow!("Gemini Live setup send failed: {}", e)));
        return;
    }

    // The server acknowledges the config before accepting audio. Anything else
    // here is a rejected setup (bad key, unknown field) and must surface now.
    match reader.next().await {
        // A refused session (exhausted balance, bad key, unknown model) is a
        // close frame whose reason is the only explanation the service gives.
        // Parsing that empty frame as JSON is what used to log
        // "EOF while parsing a value" while the real cause — a 402 — was lost.
        Some(Ok(Message::Close(frame))) => {
            let reason = match frame {
                Some(f) => format!("code {}: {}", f.code, f.reason),
                None => "no reason given".to_string(),
            };
            let _ = ready_tx.send(Err(anyhow!(
                "Gemini Live refused the session ({})",
                reason
            )));
            return;
        }
        Some(Ok(msg)) => {
            let text = message_text(&msg);
            match serde_json::from_str::<Value>(&text) {
                Ok(value) if value.get("setupComplete").is_some() => {
                    // With automatic detection off the server ignores audio until
                    // the turn is explicitly opened.
                    let start = json!({ "realtimeInput": { "activityStart": {} } });
                    if let Err(e) = writer.send(Message::text(start.to_string())).await {
                        let _ = ready_tx.send(Err(anyhow!(
                            "Gemini Live: could not open the turn: {}", e
                        )));
                        return;
                    }
                    let _ = ready_tx.send(Ok(()));
                }
                Ok(value) => {
                    let _ = ready_tx.send(Err(anyhow!(
                        "Gemini Live rejected the session setup: {}",
                        truncate(&value.to_string())
                    )));
                    return;
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("Gemini Live setup reply unreadable: {}", e)));
                    return;
                }
            }
        }
        Some(Err(e)) => {
            let _ = ready_tx.send(Err(anyhow!("Gemini Live setup failed: {}", e)));
            return;
        }
        None => {
            let _ = ready_tx.send(Err(anyhow!("Gemini Live closed before setup completed")));
            return;
        }
    }

    let mut ended = false;
    let deadline = tokio::time::Instant::now() + SESSION_DEADLINE;
    loop {
        // After end-of-stream we are only waiting for the tail, so the read gets
        // a tight bound. Before that, the session-wide deadline applies.
        let read_deadline = if ended {
            tokio::time::Instant::now() + FINAL_WAIT
        } else {
            deadline
        };

        tokio::select! {
            _ = tokio::time::sleep_until(read_deadline) => {
                let _ = event_tx.send(LiveEvent::Closed(Err(anyhow!(
                    "Gemini Live: no response from the server before the deadline"
                ))));
                return;
            }
            outbound = audio_rx.recv() => {
                match outbound {
                    Some(Outbound::Audio(pcm)) if !ended => {
                        let frame = json!({ "realtimeInput": { "audio": {
                            "data": base64::engine::general_purpose::STANDARD.encode(&pcm),
                            "mimeType": format!("audio/pcm;rate={}", SAMPLE_RATE),
                        }}});
                        if writer.send(Message::text(frame.to_string())).await.is_err() {
                            let _ = event_tx.send(LiveEvent::Closed(Err(anyhow!(
                                "Gemini Live: connection dropped while sending audio"
                            ))));
                            return;
                        }
                    }
                    // Audio that races end-of-stream is dropped, not sent: the
                    // turn is already closed and the service would reject it.
                    Some(Outbound::Audio(_)) => {}
                    Some(Outbound::End) => {
                        if ended {
                            continue;
                        }
                        ended = true;
                        // Ordering is load-bearing: an end signal that overtakes
                        // queued audio makes the server finalize without the last
                        // words. Safe here because `Outbound` is a FIFO channel
                        // and every audio frame ahead of this one has already
                        // been written to the socket by this same loop.
                        let end = json!({ "realtimeInput": { "activityEnd": {} } });
                        if writer.send(Message::text(end.to_string())).await.is_err() {
                            let _ = event_tx.send(LiveEvent::Closed(Err(anyhow!(
                                "Gemini Live: connection dropped at end of audio"
                            ))));
                            return;
                        }
                    }
                    None => {
                        // The session was dropped. Nobody is left to receive a
                        // transcript, so stop rather than linger on the socket.
                        return;
                    }
                }
            }
            inbound = reader.next() => {
                match inbound {
                    Some(Ok(msg)) => {
                        if let Message::Close(frame) = &msg {
                            match frame {
                                Some(f) => warn!(
                                    "Gemini Live: server sent a close frame: code={} reason={:?} (end-of-audio already sent: {})",
                                    f.code, f.reason, ended
                                ),
                                None => warn!(
                                    "Gemini Live: server sent a close frame with no code (end-of-audio already sent: {})",
                                    ended
                                ),
                            }
                        }
                        if handle_server_message(&message_text(&msg), &event_tx, ended) {
                            let _ = event_tx.send(LiveEvent::Closed(Ok(())));
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = event_tx.send(LiveEvent::Closed(Err(anyhow!(
                            "Gemini Live socket error: {}", e
                        ))));
                        return;
                    }
                    None => {
                        // A close arriving before `activityEnd` is the service
                        // hanging up mid-dictation, not a finished turn.
                        // Reporting it as `Ok` is what let a truncated
                        // transcript look like a complete one.
                        if ended {
                            let _ = event_tx.send(LiveEvent::Closed(Ok(())));
                        } else {
                            let _ = event_tx.send(LiveEvent::Closed(Err(anyhow!(
                                "Gemini Live: the service closed the connection mid-dictation"
                            ))));
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Dispatch one server frame. Returns true when there is nothing more to wait for.
///
/// `ended` is whether `audioStreamEnd` has been sent, and it is what makes
/// `generationComplete` meaningful. Measured against the live service:
/// `generationComplete` fires after *every* finalized chunk — seven times on an
/// 89 s dictation — so on its own it means "that sentence is done", not "the
/// turn is done". After end-of-stream, though, the tail arrives and the next
/// `generationComplete` is the last one. That is the signal we finish on.
///
/// `turnComplete` never arrives at all here: it means "the model finished its
/// turn", and a transcription-only session has no model turn. Confirmed by
/// holding the socket open for 30 s after the tail — the final chunk,
/// `generationComplete`, then nothing, connection still open.
fn handle_server_message(
    text: &str,
    event_tx: &std_mpsc::Sender<LiveEvent>,
    ended: bool,
) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        debug!("Gemini Live: unparseable frame: {}", truncate(text));
        return false;
    };
    let Some(content) = value.get("serverContent") else {
        // Everything the service says that is not transcript used to be dropped
        // here without a trace — `goAway` included, which is precisely the frame
        // that says why a session is about to end. A 451 s dictation shipped
        // half of itself and the log had nothing to explain it.
        debug!("Gemini Live: non-transcript frame: {}", truncate(text));
        return false;
    };

    if let Some(text) = transcript_text(content, "interimInputTranscription") {
        let _ = event_tx.send(LiveEvent::Interim(text));
    }
    if let Some(text) = transcript_text(content, "inputTranscription") {
        debug!("Gemini Live: final chunk ({} chars)", text.len());
        let _ = event_tx.send(LiveEvent::Final(text));
    }
    // Anything that is neither transcript nor an end-of-turn flag is worth
    // seeing while this backend is young — a silently-ignored frame is how a
    // stalled stream hides.
    if content.get("interimInputTranscription").is_none()
        && content.get("inputTranscription").is_none()
    {
        debug!("Gemini Live: other serverContent: {}", truncate(&content.to_string()));
    }

    let flag = |name: &str| content.get(name).and_then(Value::as_bool).unwrap_or(false);
    flag("turnComplete") || (ended && flag("generationComplete"))
}

fn transcript_text(content: &Value, field: &str) -> Option<String> {
    content
        .get(field)?
        .get("text")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn message_text(msg: &Message) -> String {
    match msg {
        Message::Text(text) => text.to_string(),
        // The service sends JSON as binary frames as well as text ones.
        Message::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

fn truncate(body: &str) -> String {
    const LIMIT: usize = 300;
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

    /// A quarter-second brush against the key cannot have a transcript pending,
    /// so it must not buy the full ceiling — that six-second wait is what left
    /// the pipeline busy, and the app unusable, after an accident.
    #[test]
    fn tail_first_wait_shrinks_for_clips_too_short_to_have_a_transcript() {
        let brushed = tail_first_wait(Duration::from_millis(250));
        assert!(brushed < Duration::from_secs(1), "{brushed:?}");
        assert!(brushed >= TAIL_WAIT_FLOOR, "{brushed:?}");
    }

    /// Only ever shortens: a long dictation still gets the full, conservative
    /// ceiling, because undershooting there discards a finished transcript.
    #[test]
    fn tail_first_wait_keeps_the_ceiling_for_long_dictations() {
        assert_eq!(tail_first_wait(Duration::from_secs(142)), TAIL_FIRST_WAIT);
        assert_eq!(tail_first_wait(Duration::from_secs(600)), TAIL_FIRST_WAIT);
    }

    /// Comfortably above every measured latency on the curve (316 ms at 3 s,
    /// 1.2 s at 90 s), so the shortening never races a real transcript.
    #[test]
    fn tail_first_wait_stays_above_the_measured_latency_curve() {
        assert!(tail_first_wait(Duration::from_secs(3)) > Duration::from_millis(316));
        assert!(tail_first_wait(Duration::from_secs(90)) > Duration::from_millis(1200));
    }

    #[test]
    fn finals_join_with_a_single_space() {
        let chunks = vec![
            "…открывается в правой панели чата.".to_string(),
            "И там видно оригинальный скан.".to_string(),
        ];
        assert_eq!(
            join_finals(&chunks),
            "…открывается в правой панели чата. И там видно оригинальный скан."
        );
    }

    #[test]
    fn join_skips_blank_chunks_and_trims() {
        let chunks = vec!["  Один. ".to_string(), String::new(), " Два.".to_string()];
        assert_eq!(join_finals(&chunks), "Один. Два.");
    }

    #[test]
    fn setup_uses_the_live_model_and_enum_mode() {
        let mut cfg = GeminiTranscribeSettings::default();
        cfg.mode = GeminiTranscribeMode::Smart;
        let setup = build_setup(LIVE_API_MODEL, &cfg, "auto", &[]);
        let inner = &setup["setup"];
        assert_eq!(inner["model"], json!(LIVE_API_MODEL));
        assert_eq!(inner["generationConfig"]["responseModalities"], json!(["TEXT"]));
        assert_eq!(inner["inputAudioTranscription"]["mode"], json!("SMART"));
        assert_eq!(inner["inputAudioTranscription"]["languageCodes"], json!([]));
        assert!(inner["inputAudioTranscription"].get("customVocabulary").is_none());
    }

    #[test]
    fn setup_carries_languages_and_vocabulary() {
        let mut cfg = GeminiTranscribeSettings::default();
        cfg.mode = GeminiTranscribeMode::Verbatim;
        cfg.language_codes = vec!["ru-RU".into(), "en-US".into()];
        cfg.custom_vocabulary = vec!["10Clouds".into()];
        cfg.include_custom_words = true;
        let setup = build_setup(LIVE_API_MODEL, &cfg, "auto", &["Handy".to_string()]);
        let iat = &setup["setup"]["inputAudioTranscription"];
        assert_eq!(iat["mode"], json!("VERBATIM"));
        assert_eq!(iat["languageCodes"], json!(["ru-RU", "en-US"]));
        assert_eq!(iat["customVocabulary"], json!(["10Clouds", "Handy"]));
    }

    #[test]
    fn ai_studio_endpoint_carries_the_key_and_the_prefixed_model() {
        let route = GeminiRoute::AiStudio {
            api_key: "AIza-test".into(),
        };
        let (request, model) = live_endpoint(&route).unwrap();
        assert_eq!(request.uri().host(), Some("generativelanguage.googleapis.com"));
        assert_eq!(request.uri().query(), Some("key=AIza-test"));
        assert!(request.headers().get("Authorization").is_none());
        assert_eq!(model, LIVE_API_MODEL);
    }

    #[test]
    fn vertex_setup_names_the_model_under_the_billed_project() {
        let model = vertex_model_path("p-1", LIVE_MODEL_NAME);
        let setup = build_setup(&model, &GeminiTranscribeSettings::default(), "auto", &[]);
        assert_eq!(
            setup["setup"]["model"],
            json!("projects/p-1/locations/global/publishers/google/models/gemini-3.5-transcribe-live")
        );
    }

    #[test]
    fn server_message_splits_interim_from_final() {
        let (tx, rx) = std_mpsc::channel();

        assert!(!handle_server_message(
            r#"{"serverContent":{"interimInputTranscription":{"text":"привет"}}}"#, &tx, false));
        assert!(!handle_server_message(
            r#"{"serverContent":{"inputTranscription":{"text":"Привет."}}}"#, &tx, false));

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], LiveEvent::Interim(t) if t == "привет"));
        assert!(matches!(&events[1], LiveEvent::Final(t) if t == "Привет."));
    }

    #[test]
    fn generation_complete_only_ends_the_turn_after_end_of_stream() {
        let (tx, _rx) = std_mpsc::channel();
        let frame = r#"{"serverContent":{"generationComplete":true}}"#;

        // Mid-dictation it just means "that sentence is done" — treating it as
        // the end truncated an 89 s clip to its first 48 characters.
        assert!(!handle_server_message(frame, &tx, false));
        // After audioStreamEnd it is the last one, and the tail is in.
        assert!(handle_server_message(frame, &tx, true));
    }

    #[test]
    fn turn_complete_ends_the_turn_whenever_it_arrives() {
        let (tx, _rx) = std_mpsc::channel();
        let frame = r#"{"serverContent":{"turnComplete":true}}"#;
        assert!(handle_server_message(frame, &tx, false));
        assert!(handle_server_message(frame, &tx, true));
    }

    #[test]
    fn unrelated_frames_are_ignored() {
        let (tx, rx) = std_mpsc::channel();
        assert!(!handle_server_message(r#"{"setupComplete":{}}"#, &tx, true));
        assert!(!handle_server_message("not json", &tx, true));
        assert!(!handle_server_message(r#"{"serverContent":{}}"#, &tx, true));
        assert!(rx.try_iter().next().is_none());
    }
}

/// End-to-end check against the real service, through the same code the app
/// runs. Ignored by default (network, credentials, a bill); run it with
///
/// ```sh
/// HANDY_SMOKE_WAV=… HANDY_SMOKE_VERTEX_PROJECT=… HANDY_SMOKE_VERTEX_CREDENTIALS=… \
///   cargo test --lib live_smoke -- --ignored --nocapture
/// ```
///
/// Without the Vertex variables it takes the AI Studio route with
/// `HANDY_GEMINI_API_KEY`. Audio is fed at 4x real time: faster than that and
/// the service closes the socket with "Client overran StreamHandler internal
/// queue size".
#[cfg(test)]
mod live_smoke {
    use super::*;
    use crate::cloud::gemini::{GeminiTranscriber, MODEL_ID};
    use crate::settings::GeminiConnection;

    fn smoke_config() -> Option<(GeminiTranscribeSettings, Vec<f32>)> {
        let wav_path = std::env::var("HANDY_SMOKE_WAV").ok()?;
        let mut reader = hound::WavReader::open(wav_path).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.expect("sample") as f32 / i16::MAX as f32)
            .collect();
        let mut config = GeminiTranscribeSettings::default();
        config.mode = GeminiTranscribeMode::Verbatim;
        if let Ok(project) = std::env::var("HANDY_SMOKE_VERTEX_PROJECT") {
            config.connection = GeminiConnection::Vertex;
            config.vertex_project = project;
            config.vertex_credentials =
                std::env::var("HANDY_SMOKE_VERTEX_CREDENTIALS").expect("credentials path");
        }
        Some((config, samples))
    }

    #[test]
    #[ignore]
    fn batch_and_live_transcribe_a_real_recording() {
        let Some((config, samples)) = smoke_config() else {
            eprintln!("HANDY_SMOKE_WAV not set; skipping");
            return;
        };
        let route = GeminiRoute::from_settings(&config, "").expect("route");
        eprintln!("route: {}", route.label());

        let started = std::time::Instant::now();
        let batch = GeminiTranscriber::new(MODEL_ID.into())
            .expect("engine")
            .transcribe(&route, &samples, &config, "auto", &[])
            .expect("batch transcript");
        eprintln!("batch ({:?}): {}", started.elapsed(), batch);
        assert!(!batch.is_empty());

        let mut session =
            GeminiLiveSession::connect(&route, &config, "auto", &[]).expect("live connect");
        let mut finals = Vec::new();
        for chunk in samples.chunks(1600) {
            session.feed(chunk);
            std::thread::sleep(Duration::from_millis(25));
            for event in session.poll() {
                if let LiveEvent::Final(text) = event {
                    finals.push(text);
                }
            }
        }
        let audio = Duration::from_secs_f64(samples.len() as f64 / SAMPLE_RATE as f64);
        let released = std::time::Instant::now();
        finals.extend(
            session
                .finish(Duration::from_secs(15), tail_first_wait(audio))
                .expect("live tail"),
        );
        let live = join_finals(&finals);
        eprintln!("live (tail {:?}): {}", released.elapsed(), live);
        assert!(!live.is_empty());
    }
}
