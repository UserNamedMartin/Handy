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
choose its own mode**, and ships a second transcribe binding so a hold-to-talk key
**and** a hands-free toggle key work at the same time.

Default behaviour after this change:
- `transcribe` → `ActivationMode::Global` (follows the global Push-To-Talk toggle; default = hold).
- `transcribe_toggle` → `ActivationMode::Toggle` (press once = start, press again = stop). Default key `ctrl+option+space` on macOS.

Files touched (keep them consistent if you extend this):
- `src-tauri/src/settings.rs` — `enum ActivationMode { Global, PushToTalk, Toggle }` (+ `resolve(global_ptt) -> bool`), `activation_mode` field on `ShortcutBinding` (`#[serde(default)]` → `Global` for old stores), and the new default `transcribe_toggle` binding. New default bindings are auto-back-filled into existing settings stores by the merge in `get_settings()`.
- `src-tauri/src/shortcut/handler.rs` — resolves the effective push-to-talk boolean **per binding** (`binding.activation_mode.resolve(settings.push_to_talk)`) instead of always reading the global flag.
- `src-tauri/src/transcription_coordinator.rs` — `is_transcribe_binding()` also matches `transcribe_toggle`. **The coordinator itself is deliberately unchanged** — it already takes a `push_to_talk` bool per call and tracks which binding owns the recording via `Stage::Recording(id)`, so two bindings coexist (only one records at a time).
- `src-tauri/src/actions.rs` — `ACTION_MAP` maps `transcribe_toggle` → `TranscribeAction { post_process: false }`.
- `src/components/settings/general/GeneralSettings.tsx` — renders a second `<ShortcutInput shortcutId="transcribe_toggle" />`. Labels fall back to the binding's Rust `name`/`description` via `t(key, defaultValue)`.

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
# 2. if the official build was installed via Homebrew, remove it so it can't auto-update over ours
brew uninstall --cask handy 2>/dev/null || true
# 3. install
cp -R src-tauri/target/release/bundle/macos/Handy.app /Applications/
xattr -cr /Applications/Handy.app
```

### ⚠️ Permissions after replacing (important — this bit Martin once)
Our build is **ad-hoc signed** — a different signature from the official build. macOS ties
Accessibility/Microphone (TCC) permissions to the signature, so replacing the app leaves
**stale/duplicate "Handy" entries** and permissions silently stop working. After installing:
```bash
tccutil reset All com.pais.handy
```
Then open Handy and grant **Accessibility** + **Microphone** fresh in
System Settings → Privacy & Security.

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
