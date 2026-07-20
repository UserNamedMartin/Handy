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

### Per-binding activation mode + dedicated hands-free toggle key
All fork work lives directly on `main` (this is a personal fork — no feature-branch ceremony; commit straight to `main`).

Upstream has a single global `push_to_talk` flag that forces **every** transcribe
binding into hold-to-talk (`true`) or toggle (`false`). This fork lets **each binding
choose its own mode** via `ActivationMode { Global, PushToTalk, Toggle, Hybrid }`,
and ships a second transcribe binding.

`Hybrid` = the "Wispr Flow" one-key experience: **hold** to talk (push-to-talk),
**double-tap** to lock hands-free recording, **tap** once more to stop. Implemented in
the coordinator via `evaluate_hybrid` + a double-tap-window timeout.

Default behaviour after this change:
- `transcribe` → `ActivationMode::Hybrid` (hold, or double-tap to latch hands-free).
- `transcribe_toggle` → `ActivationMode::Toggle` (press once = start, press again = stop). Default key `ctrl+option+space` on macOS.
- `Global` still follows the app-wide Push-To-Talk toggle (used by `transcribe_with_post_process`).

Files touched (keep them consistent if you extend this):
- `src-tauri/src/settings.rs` — `enum ActivationMode { Global, PushToTalk, Toggle, Hybrid }` with `resolve(global_ptt) -> ActivationMode` (maps `Global` → PushToTalk/Toggle; never returns `Global`); `activation_mode` field on `ShortcutBinding` (`#[serde(default)]` → `Global` for old stores); `transcribe` default is `Hybrid`; new default `transcribe_toggle` binding. New default bindings are auto-back-filled into existing settings stores by the merge in `get_settings()`.
- `src-tauri/src/shortcut/handler.rs` — resolves each binding's `ActivationMode` and passes it to the coordinator instead of the global bool.
- `src-tauri/src/transcription_coordinator.rs` — `send_input`/`Command::Input` now carry an `ActivationMode`. `PushToTalk`/`Toggle` use the existing logic; `Hybrid` runs `evaluate_hybrid` (hold vs quick-tap vs double-tap-latch) with the double-tap window enforced by the coordinator loop's timeout. `is_transcribe_binding()` also matches `transcribe_toggle`. Unit tests cover hold / double-tap-latch / lone-tap. Single-slot `Stage` means only one binding records at a time.
- `src-tauri/src/signal_handle.rs` — CLI/signal triggers send `ActivationMode::Toggle`.
- `src-tauri/src/actions.rs` — `ACTION_MAP` maps `transcribe_toggle` → `TranscribeAction { post_process: false }`.
- `src/components/settings/general/GeneralSettings.tsx` — renders a second `<ShortcutInput shortcutId="transcribe_toggle" />`. Labels fall back to the binding's Rust `name`/`description` via `t(key, defaultValue)`.

### Tuning the Hybrid feel
`HOLD_THRESHOLD` (300 ms — hold vs tap) and `DOUBLE_TAP_WINDOW` (400 ms — max gap
between taps) are consts at the top of `transcription_coordinator.rs`.

### Other local fixes
- **Fullscreen-aware overlay position** (`src-tauri/src/overlay.rs`): the bottom anchor used only macOS `work_area`, which a background app is handed as the *desktop's* Dock-reserved frame even when another app is in fullscreen — so the pill floated up "as if the Dock were there." Fixed with `dock_state::dock_is_on_screen()` (hand-declared `CGWindowList` + `core-foundation` externs): ask the window server directly whether the Dock's tile-bar window (owner `"Dock"`, layer 20 = `kCGDockWindowLevel`) is currently on screen. No Dock on screen (fullscreen space or auto-hidden) → anchor to the physical screen bottom; Dock on screen → above it via `work_area`. This is **app-agnostic** — an earlier attempt used the Accessibility `AXFullScreen` attribute, which works for native apps but NOT Electron apps (Claude, ChatGPT), so their fullscreen still floated high; the CGWindowList check works for all. No screen-recording permission needed (metadata only). Added dep: `core-foundation` (macOS).
- **Live overlay repositioning (animated glide)** (`src-tauri/src/overlay.rs`): while the overlay is visible, a ~60 fps loop (`start_overlay_reposition_loop` → `overlay_anim_tick`) **eases** the pill toward its target, so Dock/fullscreen changes mid-dictation glide instead of snapping. The target (the Dock check) is refreshed ~8×/sec; each frame `overlay_anim_tick` lerps `OVERLAY_ANIM.0` (current) → `.1` (target) at 0.30/frame and snaps within 0.5 px. `overlay_anim_snap_to` sets it instantly on show (appear in place) and clears it on hide. Gated by `OVERLAY_VISIBLE` + `OVERLAY_REPOSITION_GEN`. macOS only.
- **Long-form punctuation** (`src-tauri/src/managers/transcription.rs`): whisper-family
  transcription now sets `condition_on_prev_tokens: false`. whisper.cpp's default
  conditions each 30 s window on the previous window's decoded text; on long dictations
  that self-conditioning collapses punctuation into an unbroken wall of text (and can
  trigger repetition loops). Disabling it transcribes each window fresh. Short clips
  (one window) were never affected.
- **Hands-free latch — Space while holding** (`settings.rs`, `shortcut/*`, `transcription_coordinator.rs`, `actions.rs`, `utils.rs`): while a Hybrid recording is live, pressing the `latch` binding (default `fn+space` on macOS) locks it hands-free — release the transcribe key and it keeps recording; press the key again to stop. Built like the Escape/cancel shortcut: a `latch` binding registered dynamically **only while recording** (`register_latch_shortcut`/`unregister_latch_shortcut` in both backends; excluded from `init_shortcuts`), routed in `handler.rs` to `TranscriptionCoordinator::notify_latch()` → `Command::Latch`, which sets `hybrid.latched = true`. handy_keys backend only — fn is a modifier there; the Tauri backend stubs are no-ops.
- **Compact "Tiny" overlay look + centre-weighted waveform** (`src/overlay/RecordingOverlay.css` + `RecordingOverlay.tsx`): pill ~92/132 px wide, 24 px tall, 14 px radius; 7 bars; dot 5 px, cancel 16 px, spinner 11 px, label 10 px; dot and cancel are inset from their edge by their own vertical-centring gap so they sit symmetric. The waveform pulses **from the centre** (symmetric cosine envelope) driven by the **peak** of the voice band with a curved gain (`MIC_GAIN`/`MIC_CURVE`/`BAR_MIN`/`BAR_MAX` consts in `.tsx`), so it reacts at normal speaking volume — an earlier averaging approach diluted quiet buckets and barely moved.
- **Waveform sensitivity bump** (`RecordingOverlay.tsx` + `audio_toolkit/audio/visualizer.rs`): the bars still needed near-shouting to move. Raised the response ~1.5× (progressive, quiet helped most) via `MIC_GAIN` 1.7→2.3 and the loudness curve `MIC_CURVE` 0.5→0.45 (exponent <1 lifts quiet more than loud, so loud speech doesn't overshoot the cap). This is a **multiplicative** change so it's ~1.5× more movement for the same input regardless of absolute mic level. Also nudged the spectrum floor `DB_MIN` −55→−58 dBFS in `visualizer.rs` to open a little more low-end range (kept conservative so a quiet room's idle bands still map to ~0). `DB_MIN`/`DB_MAX`/`GAIN`/`CURVE_POWER` in `visualizer.rs` are the upstream (pre-overlay) spectrum mapping; visual only, no effect on VAD/transcription.
- **Cancel ✕ drawn as CSS bars** (`RecordingOverlay.css` `.sx::before`/`::after`; the `.tsx` cancel button is now empty): the sub-sized inline `<svg>` glyph rendered visibly off-centre inside the round button in the overlay WebView (it was fine in a normal browser). Two absolutely-centred pseudo-element bars (`translate(-50%,-50%)` + `rotate(±45deg)`) centre the ✕ on the button's own centre, so it can't drift regardless of svg rendering.
- **Whisper auto-gain — quiet/whispered speech now transcribes** (`src-tauri/src/audio_toolkit/audio/gain.rs` + `recorder.rs`): whispered dictation used to come back empty. Measured cause (harnesses below): the Whisper model handles quiet speech fine, but a whisper sits ~15 dB below normal voice (~−54 vs ~−39 dBFS RMS) and the **Silero VAD gate drops it** before it reaches Whisper — end-to-end WER was 41% (natural whisper) / 98% (faint), vs ~5% for normal voice. Fix: `whisper_autogain()` **conditionally** boosts an utterance — if its RMS is below `WHISPER_LEVEL_DBFS` (−45) it's treated as a whisper and peak-normalized to `BOOST_TARGET_DBFS` (−3, clip-safe, capped at `MAX_BOOST_DB`); at or above that it's normal voice and returned **bit-identical**. So it's **always-on, no mode to toggle** (matches Wispr Flow's "just speak quietly"). Applied in the recorder's **offline path only**: `run_consumer` buffers raw frames when `offline_autogain` is set (whole-utterance level is needed to decide the boost) and runs auto-gain → VAD at stop via `autogain_then_vad`. Result on Martin's recordings: natural whisper 41%→5% WER, faint 98%→~11%, normal voice untouched. Stress-tested: robust to background noise (pink/hum/babble at SNR 5–10 dB → 7–22% WER); no hallucination on boosted non-speech (Silero rejects even loud non-speech). **Not yet covered:** the streaming VAD path (Parakeet-style models) — would need a running AGC; and the boost is disabled there (offline only). Toggle via `AudioRecorder::with_whisper_autogain(false)` if ever needed.

- **Debug capture — rich per-dictation logging** (`src-tauri/src/debug_capture.rs`, `recorder.rs`, `actions.rs`, `settings.rs`): **on by default** (`debug_capture` setting; `debug_capture_limit` = 200). Every dictation writes `~/Library/Application Support/com.pais.handy/debug/<timestamp>/`:
  - `raw.wav` — the **untouched raw capture** (16 kHz mono), before auto-gain and VAD — exactly what the tuning harnesses consume, so any real dictation can be replayed/re-tuned offline.
  - `meta.json` — `transcribe_ms`; audio stats (raw duration, RMS/peak dBFS); the auto-gain decision (`classified_whisper`, `applied_gain_db`); VAD stats (`frames_in`/`frames_kept`/`kept_ratio`); the raw + final (+ post-processed) transcript; and a settings snapshot (model, vad_enabled).
  Bundles are pruned to the newest `debug_capture_limit`. This is **separate from and does not touch** the native history (`history.db` + `recordings/`). Plumbing: the recorder stashes a `CaptureDebug { raw, autogain_db, classified_whisper, vad_frames_in, vad_frames_kept }` at stop (offline path), drained via `RecordingManager::take_capture_debug()` in `actions.rs`, which writes the bundle after transcription. Only the offline (whisper-autogain) path is captured; streaming isn't. Detected language isn't logged yet (`transcribe()` returns only text).

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

**Don't leave the build-copy around.** `src-tauri/target/release/bundle/macos/Handy.app`
shows up in Spotlight as a second "Handy". Deleting `src-tauri/target/release/bundle/` is
safe (regenerated on rebuild; keeps the compile cache) — but keep the rest of `target/`.

---

## Sync with upstream

Your `main` has diverged from the original (that's expected for a personal fork).
To pull the original's updates into it:

```bash
git fetch upstream
git checkout main
git merge upstream/main          # or: git rebase upstream/main
# likely conflict files: settings.rs, shortcut/handler.rs, GeneralSettings.tsx,
# overlay.rs, RecordingOverlay.{css,tsx} — see the "Custom features" / "Other
# local fixes" sections above to re-apply intent, then rebuild.
```
