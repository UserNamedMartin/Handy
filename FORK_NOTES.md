# Fork Notes — Martin's local Handy

Personal fork of [cjpais/Handy](https://github.com/cjpais/Handy) with custom features.
**Read this before working on the code in a new session.** It's the fast path to
building, installing, and safely extending this fork. General architecture lives in
[AGENTS.md](AGENTS.md); this file is the fork-specific delta + local ops.

Remotes: `origin` = your fork (`UserNamedMartin/Handy`), `upstream` = `cjpais/Handy`.
Bundle id: `com.pais.handy` (same as the official app → shares settings/models in
`~/Library/Application Support/com.pais.handy`).

---

## Custom features in this fork (not upstream)

### Key activation: `HoldOrDoubleTap`
All fork work lives directly on `main` (this is a personal fork — no
feature-branch ceremony; commit straight to `main`).

**This used to be a whole parallel state machine and no longer is.** Upstream
shipped its own `ShortcutActivation { Toggle, PushToTalk, HoldOrToggle }` with a
configurable `hold_threshold_ms` in v0.9.6 — their take on what this fork built
in July. Carrying a second implementation of their feature is what made that
merge expensive, and it would have cost the same again every release. Their
coordinator is now used wholesale; the fork owns **one enum variant**:

- **`HoldOrDoubleTap`** (the fork default) — hold to talk and release to stop,
  a **lone tap does nothing**, a **double tap** inside `double_tap_window_ms`
  locks recording on until the next press.

Upstream's `HoldOrToggle` latches on a *single* tap. That is fine for a normal
hotkey and wrong for this setup, where the transcribe key is `fn`: a stray brush
against a modifier would start a live recording. One deliberate gesture buys
immunity to that. The mode is app-wide and switchable in
General → Shortcut Behavior, so upstream's single-tap variant is one click away
if the extra tap ever annoys.

The **`latch` binding** (`fn+space`, registered only while recording) survives as
a second way into the lock upstream already models — press it mid-hold and the
transcribe key can be released.

What the fork used to have and deliberately dropped: `ActivationMode` per
binding, and the `transcribe_toggle` key. One app-wide mode plus the latch key
covers both, and 200 consecutive dictations had used nothing but `transcribe`.

Where the variant lives (keep these together if you extend it):
- `settings.rs` — the `HoldOrDoubleTap` variant, `double_tap_window_ms`, and a
  **fork-specific migration**: a pre-merge store says `push_to_talk: true` *and*
  `transcribe.activation_mode: hybrid`, and the binding won. Upstream's
  migration reads the bool alone, which would silently downgrade exactly the
  users who had configured hands-free. Covered by tests both ways.
- `transcription_coordinator.rs` — `PendingTap` + `tap_deadline()`, resolved by
  `on_tap_expired()`. This reuses upstream's own deferred-decision machinery
  (they already defer a release by a grace window to absorb X11 auto-repeat);
  `next_deadline()` sleeps until whichever timer lands first. `Command::Latch` →
  `on_latch()` sets the same `locked` flag a double tap sets.
- `shortcut/handler.rs` — routes the `latch` binding to `notify_latch()`.

Eight unit tests cover the variant and the latch: long hold stops, lone tap
starts nothing lasting, second tap locks, a late second tap does not, latch
locks a live hold, cancel clears a pending window, and `next_deadline` picks the
nearer timer. Upstream's 36 coordinator tests still pass unchanged.

### Other local fixes
- **Fullscreen-aware overlay position** (`src-tauri/src/overlay.rs`): the bottom anchor used only macOS `work_area`, which a background app is handed as the *desktop's* Dock-reserved frame even when another app is in fullscreen — so the pill floated up "as if the Dock were there." Fixed with `dock_state::dock_is_on_screen()` (hand-declared `CGWindowList` + `core-foundation` externs): ask the window server directly whether the Dock's tile-bar window (owner `"Dock"`, layer 20 = `kCGDockWindowLevel`) is currently on screen. No Dock on screen (fullscreen space or auto-hidden) → anchor to the physical screen bottom; Dock on screen → above it via `work_area`. This is **app-agnostic** — an earlier attempt used the Accessibility `AXFullScreen` attribute, which works for native apps but NOT Electron apps (Claude, ChatGPT), so their fullscreen still floated high; the CGWindowList check works for all. No screen-recording permission needed (metadata only). Added dep: `core-foundation` (macOS).
- **Live overlay repositioning (animated glide)** (`src-tauri/src/overlay.rs`): while the overlay is visible, a ~60 fps loop (`start_overlay_reposition_loop` → `overlay_anim_tick`) **eases** the pill toward its target, so Dock/fullscreen changes mid-dictation glide instead of snapping. The target (the Dock check) is refreshed ~8×/sec; each frame `overlay_anim_tick` lerps `OVERLAY_ANIM.0` (current) → `.1` (target) at 0.30/frame and snaps within 0.5 px. `overlay_anim_snap_to` sets it instantly on show (appear in place) and clears it on hide. Gated by `OVERLAY_VISIBLE` + `OVERLAY_REPOSITION_GEN`. macOS only.
- **Long-form punctuation** (`src-tauri/src/managers/transcription.rs`): whisper-family
  transcription now sets `condition_on_prev_tokens: false`. whisper.cpp's default
  conditions each 30 s window on the previous window's decoded text; on long dictations
  that self-conditioning collapses punctuation into an unbroken wall of text (and can
  trigger repetition loops). Disabling it transcribes each window fresh. Short clips
  (one window) were never affected.
- **Whisper style primer** (`src-tauri/src/managers/transcription.rs`, `WHISPER_STYLE_PRIMER` + `whisper_initial_prompt()`): whisper-family models now get a short, well-punctuated mixed RU/EN snippet (English terms in Latin) as their `initial_prompt` instead of `None`. whisper's initial_prompt is a *style example* (not an instruction): the model mirrors its punctuation, capitalisation, and Latin-vs-Cyrillic rendering. Measured on a 195-clip corpus of real dictations vs the cloud-consensus reference: Punct-F1 ~73→84, Latin-term retention ~29%→49%, small CER gain; ~zero speed cost; stays under whisper's 224-token budget. Applied per decode window (we run condition_on_prev=false), so it keeps steering style across long dictations without error propagation. Any user `custom_words` are appended to the primer. Non-whisper archs (family=None) are unaffected. Primer chosen by A/B ("P0") over style-only and term-list variants; see `~/tools-for-agents/handy-eval/`.
- **Whisper pseudo-streaming for long dictations** (`whisper_streaming` setting, default **OFF**; `audio_toolkit/audio/stream_segmenter.rs`, `managers/transcription.rs` `run_whisper_segment_worker` + `transcribe_whisper_segment`, `actions.rs` start-stream gate): whisper isn't a streaming arch, but on long clips we hide the post-release wait by transcribing completed clauses in the background *while you keep talking*. Reuses the existing `StreamRouter` (per-frame `feed`) + streaming-worker engine lease: for a whisper model with the setting on, `run_stream_worker` branches into `run_whisper_segment_worker`, which drives a `StreamSegmenter` (pure state machine, unit-tested: min-segment 10 s, close on 700 ms silence via a coarse RMS gate; the generous minimum is what stops a thinking-pause cutting mid-sentence) over the fed frames, batch-transcribes each closed segment on the leased session with the style primer (+ per-segment `whisper_autogain`), and on Finalize transcribes the open tail and replies with the concatenated text — same contract as the native streaming worker, so `actions.rs` consumes it identically and still batch-falls-back if empty. Measured (offline sim on 22 fresh long clips): ~10 s-segment cutting keeps quality identical to single-shot (Punct-F1 even slightly better — segments stay fresh) while the post-release wait drops to ~1 s regardless of length (156 s clip: 7.1 s→1.1 s). Aggressive short segments hurt punctuation, hence the 10 s floor. **Caveat/known-limit:** incompatible with the whole-utterance `whisper_autogain` deferral — streaming applies auto-gain *per segment* instead. Only the offline path is captured for debug. Needs a real-mic shake-out (no dropped frames / real-time keep-up) before trusting — hence OFF by default. Tuning: `StreamSegmenterConfig` (min_segment_ms / close_on_silence_ms) + `WHISPER_STREAM_SILENCE_RMS`. To try it: set `"whisper_streaming": true` in `settings_store.json` (no UI toggle yet).
- **Hands-free latch — Space while holding** (`settings.rs`, `shortcut/*`, `transcription_coordinator.rs`, `actions.rs`, `utils.rs`): while a Hybrid recording is live, pressing the `latch` binding (default `fn+space` on macOS) locks it hands-free — release the transcribe key and it keeps recording; press the key again to stop. Built like the Escape/cancel shortcut: a `latch` binding registered dynamically **only while recording** (`register_latch_shortcut`/`unregister_latch_shortcut` in both backends; excluded from `init_shortcuts`), routed in `handler.rs` to `TranscriptionCoordinator::notify_latch()` → `Command::Latch`, which sets `hybrid.latched = true`. handy_keys backend only — fn is a modifier there; the Tauri backend stubs are no-ops.
- **Compact "Tiny" overlay look + centre-weighted waveform** (`src/overlay/RecordingOverlay.css` + `RecordingOverlay.tsx`): pill ~92/132 px wide, 24 px tall, 14 px radius; 7 bars; dot 5 px, cancel 16 px, spinner 11 px, label 10 px; dot and cancel are inset from their edge by their own vertical-centring gap so they sit symmetric. The waveform pulses **from the centre** (symmetric cosine envelope) driven by the **peak** of the voice band with a curved gain (`MIC_GAIN`/`MIC_CURVE`/`BAR_MIN`/`BAR_MAX` consts in `.tsx`), so it reacts at normal speaking volume — an earlier averaging approach diluted quiet buckets and barely moved.
- **Waveform sensitivity** (`src/overlay/RecordingOverlay.tsx`): the fork raised
  `MIC_GAIN`/`MIC_CURVE` because the bars needed near-shouting to move. Upstream
  later fixed the same complaint at the root — `db` in `visualizer.rs` is not
  true dBFS but a per-bin average, landing ~20 dB low for speech, so they
  recalibrated the window against measured audio (`-68/-30`) instead of nudging
  it (`-58/-8`). **Their calibration is the one in the tree now.** The fork keeps
  its centre-weighted envelope (the "Tiny" pill was tuned against it) but that
  envelope now sits on a wider input range, so the gain constants may want
  re-tuning — check the bars before trusting them.
- **Cancel ✕ drawn as CSS bars** (`RecordingOverlay.css` `.sx::before`/`::after`; the `.tsx` cancel button is now empty): the sub-sized inline `<svg>` glyph rendered visibly off-centre inside the round button in the overlay WebView (it was fine in a normal browser). Two absolutely-centred pseudo-element bars (`translate(-50%,-50%)` + `rotate(±45deg)`) centre the ✕ on the button's own centre, so it can't drift regardless of svg rendering.
- **Overlay window sized to the card** (`src-tauri/src/overlay.rs`): the overlay
  window is not just a canvas — it swallows every click inside it, so slack
  beyond the pill is dead space over whatever sits underneath. `OVERLAY_WIDTH` /
  `OVERLAY_HEIGHT` still described the pre-"Tiny" pill (172/216 wide, 40 tall)
  long after the CSS moved to 92/132 wide and 24 tall, leaving ~80 px of
  click-eating margin either side — and 400x120 in streaming mode, around a
  110 px card. **Keep these in sync with the `--ov-*` vars in
  RecordingOverlay.css**; nothing enforces it.
- **Whisper auto-gain — quiet/whispered speech now transcribes** (`src-tauri/src/audio_toolkit/audio/gain.rs` + `recorder.rs`): whispered dictation used to come back empty. Measured cause (harnesses below): the Whisper model handles quiet speech fine, but a whisper sits ~15 dB below normal voice (~−54 vs ~−39 dBFS RMS) and the **Silero VAD gate drops it** before it reaches Whisper — end-to-end WER was 41% (natural whisper) / 98% (faint), vs ~5% for normal voice. Fix: `whisper_autogain()` **conditionally** boosts an utterance — if its RMS is below `WHISPER_LEVEL_DBFS` (−45) it's treated as a whisper and peak-normalized to `BOOST_TARGET_DBFS` (−3, clip-safe, capped at `MAX_BOOST_DB`); at or above that it's normal voice and returned **bit-identical**. So it's **always-on, no mode to toggle** (matches Wispr Flow's "just speak quietly"). Applied in the recorder's **offline path only**: `run_consumer` buffers raw frames when `offline_autogain` is set (whole-utterance level is needed to decide the boost) and runs auto-gain → VAD at stop via `autogain_then_vad`. Result on Martin's recordings: natural whisper 41%→5% WER, faint 98%→~11%, normal voice untouched. Stress-tested: robust to background noise (pink/hum/babble at SNR 5–10 dB → 7–22% WER); no hallucination on boosted non-speech (Silero rejects even loud non-speech). **Not yet covered:** the streaming VAD path (Parakeet-style models) — would need a running AGC; and the boost is disabled there (offline only). Toggle via `AudioRecorder::with_whisper_autogain(false)` if ever needed.

- **Debug capture — rich per-dictation logging** (`src-tauri/src/debug_capture.rs`, `recorder.rs`, `actions.rs`, `settings.rs`): **on by default** (`debug_capture` setting; `debug_capture_limit` = 200). Every dictation writes `~/Library/Application Support/com.pais.handy/debug/<timestamp>/`:
  - `raw.wav` — the **untouched raw capture** (16 kHz mono), before auto-gain and VAD — exactly what the tuning harnesses consume, so any real dictation can be replayed/re-tuned offline.
  - `meta.json` — `transcribe_ms`; audio stats (raw duration, RMS/peak dBFS); the auto-gain decision (`classified_whisper`, `applied_gain_db`); VAD stats (`frames_in`/`frames_kept`/`kept_ratio`); the raw + final (+ post-processed) transcript; and a settings snapshot (model, vad_enabled).
  Bundles are pruned to the newest `debug_capture_limit`. This is **separate from and does not touch** the native history (`history.db` + `recordings/`). Plumbing: the recorder stashes a `CaptureDebug { raw, autogain_db, classified_whisper, vad_frames_in, vad_frames_kept }` at stop (offline path), drained via `RecordingManager::take_capture_debug()` in `actions.rs`, which writes the bundle after transcription. Only the offline (whisper-autogain) path is captured; streaming isn't. Detected language isn't logged yet (`transcribe()` returns only text).

### Cloud transcription backends (`src-tauri/src/cloud/`)
The first non-local engines. Handy stays offline-first: a cloud model only runs
when explicitly selected from the **Cloud** category in the model list, and it
sends raw dictation audio to a third party.

- **`EngineType::Gemini` + `ModelSource::Cloud { provider }`** (`managers/model.rs`):
  a catalog entry with no file — `size_mb: 0`, permanently `is_downloaded: true`
  (reasserted in `update_download_status`), and refused by `download_model` /
  `delete_model` / `get_model_path`. Two entries ship: `gemini-3.5-transcribe`
  (batch) and `gemini-3.5-transcribe-live` (streaming).
- **`cloud/gemini.rs` — batch.** One POST per dictation to the Interactions API
  with the WAV inlined as base64. `LoadedEngine::Gemini` holds only an HTTP
  client, so load/unload is free. **Measured: a hard ~3 s floor per request** —
  a 3 s clip and a 21 s clip both take ~3 s, upload is 12–57 ms even at 5.9 MB,
  so it is fixed server-side cost, not transfer. That makes batch *slower than
  local whisper* on the 62% of dictations under 30 s.
- **`cloud/gemini_live.rs` — streaming, and the reason to use the cloud at all.**
  WebSocket (`tokio-tungstenite` on the rustls stack reqwest already links).
  Transcribes while you talk, so key-release leaves only the tail: **measured
  219–507 ms**, against 1.6–23 s for local whisper. Connection + setup (~700 ms)
  is paid when recording starts, overlapping the first words. Interim hypotheses
  drive the overlay's `tentative` text, finalized chunks its `committed` prefix.
- **`run_gemini_live_worker`** (`managers/transcription.rs`) implements the same
  `StreamCmd` contract as the native streaming worker, so `actions.rs` consumes
  it identically — **an empty result batch-falls-back**, meaning a dead socket
  costs latency, never the dictation.
- **`cloud::block_on`** — `transcribe()` is sync but is called both from a tokio
  worker thread (`actions.rs`, inside an `async fn`) and from `spawn_blocking`.
  `Runtime::block_on` and `reqwest::blocking` both panic inside a runtime, so
  requests hop to a scratch OS thread with no runtime context.

**Gotchas found by probing the live service, not the docs:**
- `output_text` is **always null** on the REST surface (it is an SDK convenience);
  the transcript must be assembled from `steps[].content[].text`.
- Live final chunks are **whole sentences with no padding**, so a naive
  concatenation yields `"чата.И там"` — join with a single space.
- **`SMART` mode is silently disabled when `languageCodes` is non-empty.** You
  get filler-word removal *or* a language hint, never both. Surfaced in the UI.
- **`turnComplete` never arrives.** It means "the model finished *its* turn", and
  a transcription-only session has no model turn — the socket just stays open
  waiting for you to speak again. Verified by holding it open 30 s after the
  tail: final chunk, `generationComplete`, then nothing, connection still live.
  Waiting on `turnComplete` is waiting forever.
- **Manual activity detection, and no client-side audio processing.** Google's
  guidance for the Live API is to send raw 16-bit PCM and let the service decide;
  their own macOS client (`google-gemini/jot-gemini-transcribe-macOS`) gates
  nothing, and its only level-measuring type is documented as "always runs, and
  it decides nothing". It also sets
  `realtimeInputConfig.automaticActivityDetection.disabled` and brackets the turn
  with `activityStart` / `activityEnd`, because server-side voice detection "would
  cut turns in the middle of someone pausing to think". This fork now does the
  same, and `actions.rs` puts cloud models on `VadPolicy::Disabled`: Handy's gate
  exists to spare *local* engines audio they handle badly, and it passed 98% of
  frames at normal volume but only 15-22% of a faint whisper (`examples/
  vad_whisper_gate.rs`), because `whisper_autogain` compensates on the offline
  path only. Gemini transcribes that same faint whisper perfectly when given it.
  `activityEnd` must never overtake queued audio — safe here because `Outbound`
  is FIFO through the single socket-writing loop.
- **`generationComplete` fires after every finalized chunk** (seven times on an
  89 s dictation), so on its own it means "that sentence is done". It only means
  "the turn is done" *after* `audioStreamEnd` — that is the end signal, and it
  lands with the tail at ~end+0.35 s. Getting this wrong in either direction
  costs real damage: treating it as unconditional truncated an 89 s dictation to
  48 characters; ignoring it entirely forced a 600 ms idle-timeout on every
  dictation.
- `custom_vocabulary` is accepted on both endpoints but showed **no measurable
  effect** on Latin-vs-Cyrillic rendering in testing.
- Streaming scores *worse* than batch (4.0% vs 2.6% AA-WER); the latency is the
  entire reason to prefer it.

Settings live in `GeminiTranscribeSettings` (mode / language_codes /
custom_vocabulary / include_custom_words / diarization / timestamps) plus
`cloud_api_keys: SecretMap`. Each of these is persisted by a command of its
own — `change_gemini_transcribe_settings`, `change_cloud_api_keys`,
`change_show_live_transcript_setting` — registered in `collect_commands!` and
wired into `settingUpdaters` in `src/stores/settingsStore.ts`.

**A new settings key needs all three or it silently does nothing.** These three
shipped in August with the UI card and *no* backend at all: `updateSetting` looks
the key up in `settingUpdaters`, and on a miss it falls through to
`console.warn("No handler for setting: ...")` — into the WebView console, which
nobody has open. The card rendered, the button highlighted the mode you picked,
React state updated optimistically, and the next refresh restored the stored
value. It read as "the app keeps resetting my setting". Nothing in the type
system catches this: `settingUpdaters` is `Partial<...>`, so a missing key is
legal. If you add a field to `AppSettings` and expose it in the UI, add the
command and the updater in the same commit, and change it once in the running
app to confirm `settings_store.json` actually moves.

**Use `verbatim`, not the `Smart` default, for dictating commentary.** Google's
SMART mode "might slightly rewrite, omit, or rephrase" (their words, quoted at
`GeminiTranscribeMode`), and on real dictations it does exactly that: it treats
hedged framing as filler and deletes it. Measured against local whisper on the
same audio, it turned "вот тут я бы сформулировал как, что типа ваш лучший
сотрудник доступен 24 на 7 или что-то типа такого" into "Ваш лучший сотрудник
доступен 24/7." — the comment about the slide became the slide's slogan. Direct
speech survives SMART untouched; the damage lands precisely on thinking-out-loud,
which is most of what dictation is for here. The key may also come from `HANDY_GEMINI_API_KEY`
for headless runs; the stored setting wins. `custom_words_sent_to_model` now
gates the fuzzy post-corrector — running it on top of `custom_vocabulary` would
replace a term the model already got right with a near-miss from the same list.

### Testing a streaming backend without a microphone
`--stream` drives a WAV through the **real** streaming worker — engine lease,
socket teardown, batch fallback and all — because that is where every bug in
this backend actually lived. A protocol-level prototype reproduces none of it.

```bash
./target/debug/handy --transcribe-file <clip>.wav \
  --model gemini-3.5-transcribe-live --stream --json
```

Three flags exist because each one caught a bug a simpler run could not:
- **`--repeat N`** runs N dictations *in one process*. The first Live build
  stranded the engine lease, so run 1 looked perfect and every run after it
  failed with `Model is not loaded for transcription`. Separate processes never
  show it.
- **`--stream-trailing-silence-ms MS`** keeps the key held without speaking
  before finalizing. Finalizing the instant the audio ends is the *easy* case;
  pausing first is what left a flat 20 s wait, because the transcript was
  already complete and the code was waiting for more.
- The feed is paced to wall-clock. Dumping 21 s of audio in milliseconds is not
  a faster version of the same test — the service returned nothing at all.

`engine_returned` in the JSON is the regression canary: false means the lease
was stranded and every later dictation is broken, which the transcript alone
would not reveal. `source` distinguishes `stream` from `batch-fallback`.

`examples/vad_whisper_gate.rs` measures what fraction of a clip survives
Handy's VAD, using the recorder's own detector, threshold and hangover.

### Usage & spend tracking (`Usage` sidebar section)
`transcription_history` gained nullable `duration_ms` / `model_id` / `engine` /
`cost_usd` (migrations in `managers/history.rs`), written per dictation by
`dictation_usage()` in `actions.rs`. Failed transcriptions record a duration but
**no cost** — billing a request that produced nothing would inflate the report.

`usage_daily` / `usage_monthly` / `usage_summary` aggregate in SQL (local-time
day and month buckets); commands are in `commands/history.rs`. The UI
(`components/settings/usage/UsageSettings.tsx`) shows totals, a daily activity
chart, a per-model split and a monthly spend retrospective.

**Costs are estimates.** No provider exposes a spend API — Google's billing lives
in Cloud Console — so `cloud::estimate_cost_usd` multiplies billed duration by the
published blended rate ($0.005/min batch, $0.009/min Live). Buckets also carry
`measured`, the count of entries that actually had a duration, so pre-migration
history reads as "untimed" rather than as a quiet week.

**Whisper-mode tuning harnesses (in-repo, not part of the app build):** `src-tauri/examples/whisper_gain_sweep.rs` (gain × VAD-threshold sweep → WER + VAD pass-rate + end-to-end gated WER), `whisper_stress.rs` (silence-hallucination + noise robustness), `whisper_gate_tune.rs` (VAD threshold sweep), `whisper_halluc_guard.rs` (decoder `no_speech_thold`/`logprob_thold`). They read WAV corpora in `examples/whisper_corpus/` (real recordings) and `examples/whisper_stress/` (synthesized). Corpus captured with a Playwright-driven raw-mic recorder page (getUserMedia with AGC/NS/EC off). Run e.g. `cargo run --release --example whisper_gain_sweep`.

**Overlay tuner (design tool, in-repo):** the overlay look is dialled in with a standalone HTML tuner at [`tools/overlay-tuner/`](tools/overlay-tuner/) — a live, real-mic preview with sliders whose "config" readout maps 1:1 to the CSS values above. Serve it (the mic needs localhost) with `python3 -m http.server 8787 --bind 127.0.0.1 --directory tools/overlay-tuner`, then open <http://localhost:8787/>. Details + a config→CSS mapping table in `tools/overlay-tuner/README.md`. Not part of the app build.

To expose a per-binding mode **dropdown** in the UI later: add a Tauri command mirroring `change_ptt_setting` (in `shortcut/mod.rs`), register it in `lib.rs` `collect_commands!`, then run a debug build to regenerate `src/bindings.ts`.

---

## Local dev setup (macOS, Apple Silicon)

Install once:
- **Rust** (stable) via <https://rustup.rs> — ensure `cargo`/`rustc` on PATH (`source ~/.cargo/env`).
- **Bun** — `brew install bun`.
- **cmake** — `brew install cmake`. cmake 4.x needs `CMAKE_POLICY_VERSION_MINIMUM=3.5` (whisper.cpp).
- Xcode Command Line Tools (clang). Full Xcode only needed for Apple Intelligence post-processing (otherwise stubbed — harmless build warning).

One-time repo setup:
```bash
bun install
mkdir -p src-tauri/resources/models
curl -fsSL -o src-tauri/resources/models/silero_vad_v4.onnx https://blob.handy.computer/silero_vad_v4.onnx
```

---

## Build

```bash
export CMAKE_POLICY_VERSION_MINIMUM=3.5
bun run tauri build          # release .app + .dmg  → src-tauri/target/release/bundle/macos/Handy.app
# bun run tauri dev          # debug run; ALSO regenerates src/bindings.ts (tauri-specta)
(cd src-tauri && cargo check)  # fast Rust type-check before a full build (~seconds incremental)
(cd src-tauri && cargo test)   # unit tests — run after coordinator/settings changes
```
- **Run `cargo test` (in `src-tauri/`) after touching the coordinator or settings** — the fork adds unit tests for the Hybrid state machine (hold / double-tap-latch / lone-tap) and settings back-fill/salvage; keep them green.
- **First build is slow** (~10–25 min: whisper.cpp + onnxruntime + all Rust deps). Incremental rebuilds are ~30 s–2 min.
- A harmless error at the very end — `A public key has been found, but no private key ... TAURI_SIGNING_PRIVATE_KEY` — is the auto-updater artifact signing. The `.app` is already built before it. Ignore, or disable the updater in `tauri.conf.json`.
- `src/bindings.ts` is auto-generated and only re-exported on **debug** builds. Release builds don't regenerate it — fine unless the frontend needs new Rust types.

---

## Install the local build (replacing a previous install)

```bash
# 1. quit any running instance (single-instance: a stale process hijacks the launch)
osascript -e 'tell application "Handy" to quit'; pkill -f "Handy.app/Contents/MacOS/handy"
# 2. (first time only) if the official build was installed via Homebrew, remove it
brew uninstall --cask handy 2>/dev/null || true
# 3. install
rm -rf /Applications/Handy.app
cp -R src-tauri/target/release/bundle/macos/Handy.app /Applications/
# 4. RE-SIGN with the stable local cert — REQUIRED every rebuild to keep TCC grants
#    Unlock the signing keychain FIRST, or codesign pops a blocking GUI password
#    prompt (and a headless/timed shell just hangs on it). The partition-list line
#    grants codesign non-interactive access so it won't prompt again this session.
security unlock-keychain -p handydev "$HOME/Library/Keychains/handy-signing.keychain-db"
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k handydev \
  "$HOME/Library/Keychains/handy-signing.keychain-db" >/dev/null
codesign --force --deep \
  --sign D01CBC8B3BE2C8661FBB4A4E7BECE27061FEEB35 \
  --keychain "$HOME/Library/Keychains/handy-signing.keychain-db" \
  /Applications/Handy.app
xattr -cr /Applications/Handy.app
open /Applications/Handy.app
```
Verify the re-sign took: `codesign -dvvv /Applications/Handy.app` should show
`Authority=Handy Dev (Martin)` (NOT `Signature=adhoc`). Adhoc means the re-sign
didn't happen and TCC grants will reset.

### Stable code-signing (why there are no more permission re-grants)
Ad-hoc signing changes the app's identity on every build, which resets macOS
Accessibility/Microphone (TCC) grants. This fork is signed with a **stable self-signed
certificate**, so the designated requirement
(`identifier "com.pais.handy" and certificate leaf = H"d01cbc8b…"`) never changes —
grant permissions **once** and every future rebuild keeps them. **Always re-sign with
this cert (step 4 above) after copying a new build.**

- Identity: `Handy Dev (Martin)`, SHA-1 `D01CBC8B3BE2C8661FBB4A4E7BECE27061FEEB35`
- Keychain: `~/Library/Keychains/handy-signing.keychain-db` (password `handydev`)
- Cert/key backup (outside the repo): `~/tools-for-agents/.handy-signing/`
- **Expected steady state — don't mistake it for breakage:** since the cert is self-signed, `security find-identity -p codesigning` lists it as untrusted (`CSSMERR_TP_NOT_TRUSTED`, "0 valid identities found") and `spctl -a` reports the app `rejected`, permanently. That's normal — `codesign --sign <SHA> --keychain …` still signs fine and the app runs (TCC keys on the designated requirement, not on Gatekeeper trust). Only treat signing as broken if `codesign` itself fails with `<SHA>: no identity found` → run the recovery below.

**First-time only:** self-signed apps fail Gatekeeper assessment (`spctl` → "rejected"),
but a locally-built app still launches — the first launch may need Right-click → Open (or
System Settings → Privacy & Security → "Open Anyway"). Then enable **Handy** under
**Accessibility** and **Microphone**. Not needed again on later rebuilds.

**If codesigning breaks — recreate the keychain from the backup p12.** This is a
recurring gotcha, not a one-off: because signing uses a *separate* keychain (not the
login keychain), a session/login reset can drop `codesign`'s access to the private key.
Tell-tale symptoms (any of these → run the recovery below):
- `codesign` fails with **`<SHA>: no identity found`** even though `security find-identity
  -p codesigning` still lists the cert; or
- `codesign` fails with **`errSecInternalComponent`** and pops the GUI keychain-password
  prompt; or
- `security unlock-keychain -p handydev` is rejected with **"The user name or passphrase
  you entered is not correct"** — i.e. the documented password no longer opens the
  keychain (the file itself went bad, not the password). Recreating from the p12 with the
  same `handydev` works because it's a fresh keychain.

Re-importing the *same* cert from the p12 fixes all of these, and because it's the same
cert the **SHA (and your TCC grants) stay the same** — no permission re-grant needed:
```bash
# Remove the broken keychain first — create-keychain won't overwrite an existing
# file, and if it's the "passphrase not correct" case you can't open it anyway.
security delete-keychain ~/Library/Keychains/handy-signing.keychain-db 2>/dev/null || true
rm -f ~/Library/Keychains/handy-signing.keychain-db
security create-keychain -p handydev ~/Library/Keychains/handy-signing.keychain-db
security unlock-keychain  -p handydev ~/Library/Keychains/handy-signing.keychain-db
security import ~/tools-for-agents/.handy-signing/handy.p12 \
  -k ~/Library/Keychains/handy-signing.keychain-db -P handy -A -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k handydev \
  ~/Library/Keychains/handy-signing.keychain-db
security list-keychains -d user -s ~/Library/Keychains/handy-signing.keychain-db \
  $(security list-keychains -d user | sed -e 's/"//g')
```

**Editing `settings_store.json` while an OLD build is running silently discards
your edit.** The store round-trips through the running app's in-memory copy, so
a binary that predates a settings field drops that field entirely on quit — not
just its value, the key. This ate the Gemini API key and the whole
`gemini_transcribe` block once. Quit Handy first, edit, then relaunch; or make
the edit from a build that already knows the field.

The cloud API key lives at `settings.cloud_api_keys.gemini`; headless runs can
use `HANDY_GEMINI_API_KEY` instead (the stored setting wins). Neither ever
belongs in this repo.

**Don't leave the build-copy around.** `src-tauri/target/release/bundle/macos/Handy.app`
shows up in Spotlight as a second "Handy". Deleting `src-tauri/target/release/bundle/` is
safe (regenerated on rebuild; keeps the compile cache) — but keep the rest of `target/`.

---

## Sync with upstream

Last synced: **v0.9.6** (2026-08-30, 80 upstream commits). `main` has diverged
from the original — expected for a personal fork.

```bash
git fetch upstream
git tag -a working-<date> -m "known-good before the merge"   # rollback in one command
git checkout -b merge-upstream-<version>
git merge upstream/main
```

Merge on a branch, never on `main` directly: the installed build should stay
reachable while the merge is in pieces.

**Where the conflicts actually come from.** The cloud backend — the largest
thing this fork adds — produced *zero* conflicts in the v0.9.6 merge, because it
lives in files upstream does not have. Every painful conflict came from a place
where the fork had written its own version of something upstream also owns.
Keep new work in new files, and prefer adopting upstream's implementation over
maintaining a parallel one; the cost is not the first merge, it is every merge
after it.

Expect conflicts in: `settings.rs`, `transcription.rs`, `recorder.rs`,
`overlay.rs`, `actions.rs`, `bindings.ts` (generated — resolve by rebuilding),
`translation.json`, `RecordingOverlay.{css,tsx}`.

**After merging, before trusting it:**
```bash
cargo test                                   # 300+ tests, incl. the fork's key modes
./target/debug/handy --transcribe-file <clip>.wav \
  --model gemini-3.5-transcribe-live --stream --repeat 3
```
`engine_returned=false` or `source=batch-fallback` means a regression. Neither is
visible in the transcript alone.

Lessons that cost time in v0.9.6, worth checking every sync:
- `transcribe-cpp` majors rename things (`ModelOptions.gpu_device` →
  `device: Option<Device>`); the in-repo `examples/` break silently because
  `cargo build` does not compile them.
- Blanket "keep both sides" on a conflict is wrong when upstream *replaced*
  something — it duplicated a loop header once and cost a confusing build error.
- Upstream tests encode upstream defaults. Where the fork deliberately differs,
  update the test and say why in a comment rather than reverting the behaviour.
