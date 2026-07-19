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
Branch: `feat/per-binding-activation-mode`

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
- **Live overlay repositioning** (`src-tauri/src/overlay.rs`): while the overlay is visible, `start_overlay_reposition_loop` polls every 250 ms (background thread → `run_on_main_thread` → `update_overlay_position`) so the pill tracks Dock/fullscreen changes mid-dictation (reveal the Dock in fullscreen, leave fullscreen, toggle auto-hide) and moves within ~250 ms. Gated by `OVERLAY_VISIBLE` (cleared in `hide_recording_overlay`) + an `OVERLAY_REPOSITION_GEN` generation counter so only the latest show keeps polling. macOS only.
- **Long-form punctuation** (`src-tauri/src/managers/transcription.rs`): whisper-family
  transcription now sets `condition_on_prev_tokens: false`. whisper.cpp's default
  conditions each 30 s window on the previous window's decoded text; on long dictations
  that self-conditioning collapses punctuation into an unbroken wall of text (and can
  trigger repetition loops). Disabling it transcribes each window fresh. Short clips
  (one window) were never affected.

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
bun run tauri build      # release .app + .dmg  → src-tauri/target/release/bundle/macos/Handy.app
# bun run tauri dev      # debug run; ALSO regenerates src/bindings.ts (tauri-specta)
```
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
codesign --force --deep \
  --sign D01CBC8B3BE2C8661FBB4A4E7BECE27061FEEB35 \
  --keychain "$HOME/Library/Keychains/handy-signing.keychain-db" \
  /Applications/Handy.app
xattr -cr /Applications/Handy.app
open /Applications/Handy.app
```

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

**First-time only:** self-signed apps fail Gatekeeper assessment (`spctl` → "rejected"),
but a locally-built app still launches — the first launch may need Right-click → Open (or
System Settings → Privacy & Security → "Open Anyway"). Then enable **Handy** under
**Accessibility** and **Microphone**. Not needed again on later rebuilds.

**If the signing keychain is ever lost**, re-import the *same* cert to keep the identity
(and your grants) stable:
```bash
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

```bash
git fetch upstream
git checkout feat/per-binding-activation-mode
git rebase upstream/main        # or: git merge upstream/main
# likely conflict files: settings.rs, shortcut/handler.rs, GeneralSettings.tsx — see the
# "Custom features" section above to re-apply intent, then rebuild.
```
